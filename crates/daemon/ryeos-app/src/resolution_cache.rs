//! Admission-time resolution cache.
//!
//! `run_resolution_pipeline` (parse + signature-verify walk + compose over the
//! whole extends/references chain) is a pure function of signed/verified
//! content plus the immutable daemon generation, yet it runs on every launch.
//! This cache stores its output keyed on `(generation, ref, typed subject
//! authority, plan-context)` and serves a hit only after proving the outcome
//! is still current — cheaply, from content, never by recompute.
//!
//! ## Soundness
//!
//! A resolution outcome depends on both POSITIVE dependencies (the winner and
//! its ancestors/references, each carrying a whole-file digest) and NEGATIVE
//! dependencies (paths probed and found absent at a precedence at least as high
//! as the winner — recorded as [`ProbedAbsence`]). A digest-only cache is
//! unsound: if a project item APPEARS where the winner was resolved from a
//! bundle, every stored digest still matches but the correct chain is now
//! different. Revalidation therefore checks both directions.
//!
//! ## Two tiers
//!
//! - **Bundle-space** positives and absences need no revalidation: bundle
//!   content and layout are immutable for a generation's lifetime, and the
//!   generation is in the key — a `bundle.install`/`uninstall` bump makes every
//!   bundle-derived entry unreachable. A chain touching only bundle space is a
//!   pure key lookup.
//! - **Mutable project-space** (`LiveFs`) positives are re-hashed and absences
//!   re-probed. Exact pinned and sealed COW generations instead carry opaque
//!   descriptor/content proof and are pure content-generation hits; their
//!   disposable materialization lease remains request-owned.
//!
//! Static verification evidence has its own engine-owned cache. This cache
//! covers the full resolution/composition closure.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ryeos_engine::contracts::{ItemSpace, ProbedAbsence, SubjectResolutionAuthority};
use ryeos_engine::engine::VerifiedArtifactAttestation;
use ryeos_engine::resolution::ResolutionOutput;

use crate::temp_dir_guard::TempDirGuard;

const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_PENDING: usize = 128;
/// A live filesystem may change between a cold observation and publication.
/// Retry that race a small, explicit number of times; immutable authority
/// mismatches are hard failures and never consume this allowance.
pub const MAX_MUTABLE_AUTHORITY_RACE_RETRIES: usize = 2;
const IDLE_TTL: Duration = Duration::from_secs(10 * 60);

/// Identity of one admission's resolution inputs. A hit requires an exact key
/// match AND passing content-derived revalidation.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ResolutionCacheKey {
    /// System-install generation — mixed in so any bundle install/uninstall
    /// bump makes every bundle-derived entry unreachable.
    pub generation: String,
    /// Monotonic daemon epoch for retiring entries from prior generations.
    /// `None` is reserved for isolated/test cache instances without a daemon
    /// generation clock.
    pub generation_epoch: Option<u64>,
    /// Canonical ref being admitted.
    pub canonical_ref: String,
    /// Exact admitted subject authority. Immutable snapshots and sealed COW
    /// generations are identified by content, never by their disposable
    /// checkout pathname.
    pub subject_authority: SubjectResolutionAuthority,
    /// Canonical path identity is authority only for live filesystems. Pinned
    /// and COW keys leave this empty.
    pub live_project_root: Option<PathBuf>,
    /// Identity of the remaining resolution inputs the generation and project
    /// root do NOT capture, pre-rendered by the caller to one stable string:
    /// the engine/bundle generation identity, the PROJECT parser-overlay
    /// fingerprint (`.ai/parsers/`), and the effective trust identity
    /// (`.ai/trust-keys/`). An edit to any of these changes this string, so a
    /// stale resolution misses rather than being served.
    pub plan_context_identity: String,
}

/// Opaque, admission-issued proof that a resolution subject is paired with
/// the exact active materialization owned by this execution transition.
///
/// Public cache APIs accept this binding rather than a path plus a claimed
/// snapshot digest. Fields are private; only `AdmittedProjectBinding` can
/// construct one after validating durable project authority and its lifeline.
#[derive(Clone)]
pub struct ResolutionMaterializationBinding {
    subject_authority: SubjectResolutionAuthority,
    active_project_root: Option<PathBuf>,
    materialization_lifeline: Option<Arc<TempDirGuard>>,
    pinned_materialization: Option<ryeos_state::PinnedProjectMaterialization>,
    #[cfg(test)]
    allow_unproven_materialization: bool,
}

impl ResolutionMaterializationBinding {
    pub(crate) fn admitted(
        subject_authority: SubjectResolutionAuthority,
        active_project_root: Option<PathBuf>,
        materialization_lifeline: Option<Arc<TempDirGuard>>,
        pinned_materialization: Option<ryeos_state::PinnedProjectMaterialization>,
    ) -> anyhow::Result<Self> {
        let binding = Self {
            subject_authority,
            active_project_root,
            materialization_lifeline,
            pinned_materialization,
            #[cfg(test)]
            allow_unproven_materialization: false,
        };
        binding.validate_structure()?;
        Ok(binding)
    }

    fn validate(&self) -> anyhow::Result<()> {
        self.validate_structure()
    }

    fn validate_structure(&self) -> anyhow::Result<()> {
        self.subject_authority
            .validate_for_materialized_root(self.active_project_root.as_deref())?;
        #[cfg(test)]
        if self.allow_unproven_materialization {
            return Ok(());
        }
        match self.subject_authority {
            SubjectResolutionAuthority::PinnedGeneration { .. }
            | SubjectResolutionAuthority::CowWorkspace { .. } => {
                let root = self.active_project_root.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("pinned resolution binding has no active project root")
                })?;
                let lifeline = self.materialization_lifeline.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("pinned resolution binding has no materialization lifeline")
                })?;
                if !lifeline.owns_effective_path(root) {
                    anyhow::bail!(
                        "pinned resolution binding lifeline does not own its active project root"
                    );
                }
                let materialization = self.pinned_materialization.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "pinned resolution binding has no descriptor/content materialization proof"
                    )
                })?;
                if materialization.snapshot_hash()
                    != self
                        .subject_authority
                        .operational_generation()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "pinned resolution authority has no operational generation"
                            )
                        })?
                    || !materialization.owns_path(root)?
                {
                    anyhow::bail!(
                        "pinned resolution binding proof differs from its generation or root"
                    );
                }
            }
            SubjectResolutionAuthority::Projectless => {
                if self.materialization_lifeline.is_some() || self.pinned_materialization.is_some()
                {
                    anyhow::bail!(
                        "projectless resolution binding cannot carry a materialization lifeline"
                    );
                }
            }
            SubjectResolutionAuthority::LiveFs => {
                if self.pinned_materialization.is_some() {
                    anyhow::bail!(
                        "live resolution binding cannot carry pinned materialization proof"
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_key_after_binding(&self, key: &ResolutionCacheKey) -> anyhow::Result<()> {
        if self.subject_authority != key.subject_authority {
            anyhow::bail!("resolution cache key differs from admitted subject authority");
        }
        let expected_live_root =
            matches!(self.subject_authority, SubjectResolutionAuthority::LiveFs)
                .then(|| self.active_project_root.clone())
                .flatten();
        if key.live_project_root != expected_live_root {
            anyhow::bail!("resolution cache key differs from admitted live project root");
        }
        Ok(())
    }

    pub fn subject_authority(&self) -> &SubjectResolutionAuthority {
        &self.subject_authority
    }

    pub fn active_project_root(&self) -> Option<&std::path::Path> {
        self.active_project_root.as_deref()
    }

    pub fn materialization_lifeline(&self) -> Option<&Arc<TempDirGuard>> {
        self.materialization_lifeline.as_ref()
    }

    /// Return the exact state-issued project content proof paired with this
    /// binding. Callers use it to validate subsystem-specific positive and
    /// negative dependencies without inferring immutability from an authority
    /// enum or reopening the workspace.
    pub fn authoritative_project_content(
        &self,
    ) -> anyhow::Result<Option<(&std::path::Path, &ryeos_state::PinnedProjectMaterialization)>>
    {
        self.validate_structure()?;
        Ok(
            match (
                self.active_project_root.as_deref(),
                self.pinned_materialization.as_ref(),
            ) {
                (Some(root), Some(materialization)) => Some((root, materialization)),
                (None, None) | (Some(_), None) => None,
                (None, Some(_)) => {
                    anyhow::bail!("project content proof has no active project root")
                }
            },
        )
    }

    /// Establish one lexical materialization fence for a batch of cache
    /// operations. Exact key/dependency checks still occur per entry; only the
    /// complete snapshot-tree proof is shared.
    pub fn validate_once(&self) -> anyhow::Result<ValidatedResolutionMaterialization<'_>> {
        self.validate()?;
        Ok(ValidatedResolutionMaterialization { binding: self })
    }

    pub fn validates_closure(&self, closure: &ResolvedClosure) -> anyhow::Result<bool> {
        self.validate()?;
        let relocated = closure.relocated_for_validated_binding(self)?;
        let Some(inputs) = revalidation_inputs(
            &ResolutionCacheKey {
                generation: String::new(),
                generation_epoch: None,
                canonical_ref: relocated.output().root.resolved_ref.clone(),
                subject_authority: self.subject_authority.clone(),
                live_project_root: matches!(
                    self.subject_authority,
                    SubjectResolutionAuthority::LiveFs
                )
                .then(|| self.active_project_root.clone())
                .flatten(),
                plan_context_identity: String::new(),
            },
            &relocated,
            relocated.probed_absent(),
            self.active_project_root(),
        ) else {
            return Ok(false);
        };
        Ok(revalidate_for_binding(self, &inputs))
    }
}

pub struct ValidatedResolutionMaterialization<'a> {
    binding: &'a ResolutionMaterializationBinding,
}

impl ValidatedResolutionMaterialization<'_> {
    pub fn begin<'a>(
        &self,
        cache: &'a ResolutionCache,
        key: &ResolutionCacheKey,
    ) -> anyhow::Result<ResolutionLookup<'a>> {
        self.binding.validate_key_after_binding(key)?;
        Ok(cache.begin_for_binding(key, self.binding))
    }

    pub fn get(
        &self,
        cache: &ResolutionCache,
        key: &ResolutionCacheKey,
    ) -> anyhow::Result<(Option<Arc<ResolvedClosure>>, LookupOutcome)> {
        self.binding.validate_key_after_binding(key)?;
        Ok(cache.get_for_binding(key, self.binding))
    }
}

pub fn build_resolution_cache_key(
    engine: &ryeos_engine::engine::Engine,
    canonical_ref: &ryeos_engine::canonical_ref::CanonicalRef,
    subject_authority: SubjectResolutionAuthority,
    project_root: Option<&std::path::Path>,
) -> anyhow::Result<(
    ResolutionCacheKey,
    ryeos_engine::engine::EffectiveRequestAuthoritySnapshot,
)> {
    let request_snapshot = engine
        .effective_request_authority_snapshot(project_root, &subject_authority)
        .map_err(|error| anyhow::anyhow!("effective resolution snapshot failed: {error}"))?;
    let key = build_resolution_cache_key_from_snapshot(
        engine,
        canonical_ref,
        project_root,
        &request_snapshot,
    );
    Ok((key, request_snapshot))
}

pub fn build_resolution_cache_key_from_snapshot(
    engine: &ryeos_engine::engine::Engine,
    canonical_ref: &ryeos_engine::canonical_ref::CanonicalRef,
    project_root: Option<&std::path::Path>,
    request_snapshot: &ryeos_engine::engine::EffectiveRequestAuthoritySnapshot,
) -> ResolutionCacheKey {
    build_resolution_cache_key_from_identity(
        engine,
        canonical_ref,
        request_snapshot.subject_resolution_authority.clone(),
        project_root,
        [
            request_snapshot.request_engine_generation_identity.as_str(),
            request_snapshot.registry_fingerprint.as_str(),
            request_snapshot.effective_trust_identity.as_str(),
        ]
        .join("\u{1f}"),
    )
}

pub fn build_resolution_cache_key_from_identity(
    engine: &ryeos_engine::engine::Engine,
    canonical_ref: &ryeos_engine::canonical_ref::CanonicalRef,
    subject_authority: SubjectResolutionAuthority,
    project_root: Option<&std::path::Path>,
    plan_context_identity: String,
) -> ResolutionCacheKey {
    ResolutionCacheKey {
        generation: engine.registered_bundle_generation_fingerprint(),
        generation_epoch: engine.registered_bundle_generation_epoch(),
        canonical_ref: canonical_ref.to_string(),
        live_project_root: matches!(subject_authority, SubjectResolutionAuthority::LiveFs)
            .then(|| project_root.map(std::path::Path::to_path_buf))
            .flatten(),
        subject_authority,
        plan_context_identity,
    }
}

/// Immutable resolution closure retained by the cache and root admission.
///
/// Project-space paths are diagnostic/format provenance. The closure retains
/// verified bytes and never keeps a complete ephemeral checkout alive merely
/// so those pathnames continue to exist.
#[derive(Clone)]
pub struct ResolvedClosure {
    output: Arc<ResolutionOutput>,
    subject_authority: SubjectResolutionAuthority,
    resolution_root: Option<PathBuf>,
    materialization_lifeline: Option<Arc<TempDirGuard>>,
    /// Small identity shared by the cache-owned representation and the
    /// request-bound clone returned to its caller. The cache never retains the
    /// caller's materialization lease.
    cache_entry_token: Option<Arc<()>>,
    origin: ResolutionClosureOrigin,
    verified_attestation: Option<Arc<VerifiedArtifactAttestation>>,
    probed_absent: Arc<[ProbedAbsence]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolutionClosureOrigin {
    ActiveResolution,
    SealedProgram,
}

impl std::fmt::Debug for ResolvedClosure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedClosure")
            .field("subject_authority", &self.subject_authority)
            .field("resolution_root", &self.resolution_root)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl ResolvedClosure {
    pub fn new(
        output: ResolutionOutput,
        subject_authority: SubjectResolutionAuthority,
        resolution_root: Option<PathBuf>,
        materialization_lifeline: Option<Arc<TempDirGuard>>,
    ) -> anyhow::Result<Self> {
        Self::new_inner(
            output,
            subject_authority,
            resolution_root,
            materialization_lifeline,
            None,
            Vec::new(),
        )
    }

    /// Construct a resolution closure together with the exact negative probes
    /// that selected its winners. This is the non-attested form used for
    /// secondary launch bindings; admission still requires an opaque
    /// materialization binding before it may be consumed.
    pub fn new_with_probes(
        output: ResolutionOutput,
        subject_authority: SubjectResolutionAuthority,
        resolution_root: Option<PathBuf>,
        materialization_lifeline: Option<Arc<TempDirGuard>>,
        probed_absent: Vec<ProbedAbsence>,
    ) -> anyhow::Result<Self> {
        Self::new_inner(
            output,
            subject_authority,
            resolution_root,
            materialization_lifeline,
            None,
            probed_absent,
        )
    }

    pub(crate) fn new_admitted_with_proof(
        output: ResolutionOutput,
        subject_authority: SubjectResolutionAuthority,
        resolution_root: Option<PathBuf>,
        materialization_lifeline: Option<Arc<TempDirGuard>>,
        verified_attestation: Arc<VerifiedArtifactAttestation>,
        probed_absent: Vec<ProbedAbsence>,
    ) -> anyhow::Result<Self> {
        Self::new_inner(
            output,
            subject_authority,
            resolution_root,
            materialization_lifeline,
            Some(verified_attestation),
            probed_absent,
        )
    }

    fn new_inner(
        output: ResolutionOutput,
        subject_authority: SubjectResolutionAuthority,
        resolution_root: Option<PathBuf>,
        materialization_lifeline: Option<Arc<TempDirGuard>>,
        verified_attestation: Option<Arc<VerifiedArtifactAttestation>>,
        probed_absent: Vec<ProbedAbsence>,
    ) -> anyhow::Result<Self> {
        subject_authority.validate_for_project_context(&match &resolution_root {
            Some(path) => ryeos_engine::contracts::ProjectContext::LocalPath { path: path.clone() },
            None => ryeos_engine::contracts::ProjectContext::None,
        })?;
        match &subject_authority {
            SubjectResolutionAuthority::Projectless => {
                if resolution_root.is_some() || materialization_lifeline.is_some() {
                    anyhow::bail!("projectless resolution closure cannot retain a project root");
                }
                if std::iter::once(&output.root)
                    .chain(output.ancestors.iter())
                    .chain(output.referenced_items.iter())
                    .any(|item| item.source_space == ItemSpace::Project)
                {
                    anyhow::bail!(
                        "projectless resolution closure cannot contain project-space content"
                    );
                }
            }
            SubjectResolutionAuthority::PinnedGeneration { .. }
            | SubjectResolutionAuthority::CowWorkspace { .. } => {
                let root = resolution_root.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("pinned resolution closure has no materialized root")
                })?;
                let lifeline = materialization_lifeline.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("pinned resolution closure has no materialization lease")
                })?;
                if !lifeline.owns_effective_path(root) {
                    anyhow::bail!(
                        "pinned resolution closure lease does not own its materialized root"
                    );
                }
            }
            SubjectResolutionAuthority::LiveFs => {
                if resolution_root.is_none() {
                    anyhow::bail!("live resolution closure has no canonical project root");
                }
            }
        }
        if let Some(attestation) = verified_attestation.as_ref() {
            let verified = attestation.verified_subject();
            if verified.resolved.canonical_ref.to_string() != output.root.resolved_ref
                || verified.resolved.raw_content_digest != output.root.raw_content_digest
                || verified.resolved.source_space != output.root.source_space
                || lillux::sha256_hex(attestation.source_bytes()) != verified.resolved.content_hash
            {
                anyhow::bail!(
                    "admitted resolution closure differs from its verified source attestation"
                );
            }
        }
        Ok(Self {
            output: Arc::new(output),
            subject_authority,
            resolution_root,
            materialization_lifeline,
            cache_entry_token: None,
            origin: ResolutionClosureOrigin::ActiveResolution,
            verified_attestation,
            probed_absent: Arc::from(probed_absent),
        })
    }

    /// Reconstruct a closure from a sealed capsule. Recovery consumes only
    /// verified bytes; diagnostic paths are never reopened, so no ephemeral
    /// materialization lease is required.
    pub fn restored(
        output: ResolutionOutput,
        subject_authority: SubjectResolutionAuthority,
        resolution_root: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let authority_context = match (&subject_authority, resolution_root.as_ref()) {
            (SubjectResolutionAuthority::Projectless, None) => {
                ryeos_engine::contracts::ProjectContext::None
            }
            (SubjectResolutionAuthority::PinnedGeneration { snapshot_hash }, None) => {
                ryeos_engine::contracts::ProjectContext::SnapshotHash {
                    hash: snapshot_hash.clone(),
                }
            }
            (_, Some(path)) => {
                ryeos_engine::contracts::ProjectContext::LocalPath { path: path.clone() }
            }
            _ => {
                anyhow::bail!(
                    "sealed program resolution authority has no compatible rooted context"
                )
            }
        };
        subject_authority.validate_for_project_context(&authority_context)?;
        if matches!(subject_authority, SubjectResolutionAuthority::Projectless)
            && std::iter::once(&output.root)
                .chain(output.ancestors.iter())
                .chain(output.referenced_items.iter())
                .any(|item| item.source_space == ItemSpace::Project)
        {
            anyhow::bail!(
                "projectless sealed program resolution cannot contain project-space content"
            );
        }
        Ok(Self {
            output: Arc::new(output),
            subject_authority,
            resolution_root,
            materialization_lifeline: None,
            cache_entry_token: None,
            origin: ResolutionClosureOrigin::SealedProgram,
            verified_attestation: None,
            probed_absent: Arc::from([]),
        })
    }

    pub fn output(&self) -> &ResolutionOutput {
        self.output.as_ref()
    }

    pub fn output_arc(&self) -> Arc<ResolutionOutput> {
        self.output.clone()
    }

    pub fn subject_authority(&self) -> &SubjectResolutionAuthority {
        &self.subject_authority
    }

    pub fn resolution_root(&self) -> Option<&std::path::Path> {
        self.resolution_root.as_deref()
    }

    pub(crate) fn is_sealed_program(&self) -> bool {
        self.origin == ResolutionClosureOrigin::SealedProgram
    }

    pub fn verified_attestation(&self) -> Option<&Arc<VerifiedArtifactAttestation>> {
        self.verified_attestation.as_ref()
    }

    pub fn probed_absent(&self) -> &[ProbedAbsence] {
        self.probed_absent.as_ref()
    }

    /// Revalidate an uncached diagnostic closure against its current mutable
    /// project authority. Managed execution uses an admitted materialization
    /// binding instead; this narrower path exists for threadless diagnostics
    /// that deliberately do not mint execution authority.
    pub fn validates_current_diagnostic_authority(&self) -> anyhow::Result<bool> {
        if !matches!(
            self.subject_authority,
            SubjectResolutionAuthority::Projectless | SubjectResolutionAuthority::LiveFs
        ) {
            anyhow::bail!(
                "content-addressed project resolution requires an admitted materialization proof"
            );
        }
        let key = ResolutionCacheKey {
            generation: String::new(),
            generation_epoch: None,
            canonical_ref: self.output.root.resolved_ref.clone(),
            subject_authority: self.subject_authority.clone(),
            live_project_root: matches!(self.subject_authority, SubjectResolutionAuthority::LiveFs)
                .then(|| self.resolution_root.clone())
                .flatten(),
            plan_context_identity: String::new(),
        };
        let Some(inputs) =
            revalidation_inputs(&key, self, self.probed_absent(), self.resolution_root())
        else {
            return Ok(false);
        };
        Ok(revalidate(&inputs))
    }

    fn estimated_bytes(&self) -> usize {
        serde_json::to_vec(self.output.as_ref())
            .map(|bytes| bytes.len())
            .unwrap_or(MAX_ENTRY_BYTES.saturating_add(1))
            .saturating_add(
                self.verified_attestation
                    .as_ref()
                    .map(|attestation| attestation.source_bytes().len())
                    .unwrap_or(0),
            )
            .saturating_add(
                serde_json::to_vec(self.probed_absent.as_ref())
                    .map(|bytes| bytes.len())
                    .unwrap_or(MAX_ENTRY_BYTES.saturating_add(1)),
            )
    }

    fn with_probed_absent(&self, probed_absent: Vec<ProbedAbsence>) -> Self {
        let mut closure = self.clone();
        closure.probed_absent = Arc::from(probed_absent);
        closure
    }

    fn without_materialization_lifeline(&self) -> Self {
        let mut closure = self.clone();
        closure.materialization_lifeline = None;
        closure
    }

    fn with_cache_entry_token(&self, token: Arc<()>) -> Self {
        let mut closure = self.clone();
        closure.cache_entry_token = Some(token);
        closure
    }

    fn belongs_to_same_cache_entry(&self, other: &Self) -> bool {
        matches!(
            (&self.cache_entry_token, &other.cache_entry_token),
            (Some(left), Some(right)) if Arc::ptr_eq(left, right)
        )
    }

    /// Rebind diagnostic project paths to the caller's current materialized
    /// workspace. Authoritative bytes/digests remain unchanged.
    fn relocated_for_binding(
        &self,
        binding: &ResolutionMaterializationBinding,
    ) -> anyhow::Result<Self> {
        binding.validate()?;
        self.relocated_for_validated_binding(binding)
    }

    fn relocated_for_validated_binding(
        &self,
        binding: &ResolutionMaterializationBinding,
    ) -> anyhow::Result<Self> {
        if self.subject_authority != binding.subject_authority {
            anyhow::bail!("cached resolution authority differs from admitted binding");
        }
        let active_project_root = binding.active_project_root.as_deref();
        let Some(active_root) = active_project_root else {
            if matches!(
                self.subject_authority,
                SubjectResolutionAuthority::Projectless
            ) {
                return Ok(self.clone());
            }
            anyhow::bail!("rooted cached resolution has no active project root");
        };
        let Some(source_root) = self.resolution_root.as_deref() else {
            anyhow::bail!("rooted cached resolution has no source project root");
        };
        if source_root == active_root {
            let mut rebound = self.clone();
            rebound.materialization_lifeline = binding.materialization_lifeline.clone();
            return Ok(rebound);
        }
        if matches!(self.subject_authority, SubjectResolutionAuthority::LiveFs) {
            anyhow::bail!("live resolution closure cannot relocate between project roots");
        }
        let relocate = |path: &std::path::Path| {
            path.strip_prefix(source_root)
                .map(|relative| active_root.join(relative))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        let mut output = self.output.as_ref().clone();
        for item in std::iter::once(&mut output.root)
            .chain(output.ancestors.iter_mut())
            .chain(output.referenced_items.iter_mut())
        {
            if item.source_space != ItemSpace::Project {
                continue;
            }
            let relocated = relocate(&item.source_path);
            if relocated == item.source_path {
                anyhow::bail!(
                    "project-space cached source path is outside its admitted root: {}",
                    item.source_path.display()
                );
            }
            item.source_path = relocated;
        }
        for edge in &mut output.references_edges {
            edge.from_source_path = relocate(&edge.from_source_path);
            edge.to_source_path = relocate(&edge.to_source_path);
        }
        let probed_absent = self
            .probed_absent
            .iter()
            .cloned()
            .map(|mut absence| {
                if absence.space == ItemSpace::Project {
                    let relocated = relocate(&absence.path);
                    if relocated == absence.path {
                        anyhow::bail!(
                            "project-space cached negative probe is outside its admitted root: {}",
                            absence.path.display()
                        );
                    }
                    absence.path = relocated;
                }
                Ok(absence)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut relocated = self.clone();
        relocated.output = Arc::new(output);
        relocated.resolution_root = Some(active_root.to_path_buf());
        relocated.materialization_lifeline = binding.materialization_lifeline.clone();
        relocated.probed_absent = Arc::from(probed_absent);
        Ok(relocated)
    }
}

struct Entry {
    closure: Arc<ResolvedClosure>,
    estimated_bytes: usize,
    last_touched: Instant,
    /// Insertion order, for bounded eviction.
    seq: u64,
}

struct Inner {
    slots: HashMap<ResolutionCacheKey, Entry>,
    pending: HashMap<ResolutionCacheKey, Arc<PendingResolution>>,
    next_seq: u64,
    total_bytes: usize,
    active_generation: Option<(u64, String)>,
}

#[derive(Debug)]
struct ResolutionFailureMessage {
    message: String,
}

impl std::fmt::Display for ResolutionFailureMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ResolutionFailureMessage {}

trait ShareableResolutionFailure: std::error::Error + Send + Sync + 'static {
    fn into_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync>;
}

impl<T> ShareableResolutionFailure for T
where
    T: std::error::Error + Send + Sync + 'static,
{
    fn into_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> {
        self
    }
}

#[derive(Debug, Clone)]
pub struct SharedResolutionFailure(Arc<dyn ShareableResolutionFailure>);

impl std::fmt::Display for SharedResolutionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for SharedResolutionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[derive(Debug)]
struct SharedAnyhowFailure(anyhow::Error);

impl std::fmt::Display for SharedAnyhowFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.0)
    }
}

impl std::error::Error for SharedAnyhowFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.root_cause())
    }
}

enum PendingResolutionOutcome {
    Success(Arc<ResolvedClosure>),
    Failure(SharedResolutionFailure),
    Retry,
}

struct PendingResolution {
    /// Retaining the in-flight outcome lets waiters consume an oversized
    /// success or the exact Arc-backed leader failure without serial rebuilds.
    /// Failures and retry outcomes are removed from the reusable cache.
    result: Mutex<Option<PendingResolutionOutcome>>,
    wake: Condvar,
}

impl Default for PendingResolution {
    fn default() -> Self {
        Self {
            result: Mutex::new(None),
            wake: Condvar::new(),
        }
    }
}

/// Bounded, content-revalidating store of resolved-pipeline outputs.
pub struct ResolutionCache {
    inner: Mutex<Inner>,
    capacity: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmittedClosureValidation {
    Current,
    MutableAuthorityChanged,
    ImmutableAuthorityMismatch,
}

pub enum ResolutionLookup<'a> {
    Hit(Arc<ResolvedClosure>),
    Wait(ResolutionWait),
    Build(ResolutionFillGuard<'a>),
    Bypass,
}

pub struct ResolutionWait {
    pending: Arc<PendingResolution>,
    key: ResolutionCacheKey,
    materialization: ResolutionMaterializationBinding,
}

pub struct ResolutionFillGuard<'a> {
    cache: &'a ResolutionCache,
    key: ResolutionCacheKey,
    materialization: ResolutionMaterializationBinding,
    pending: Arc<PendingResolution>,
    completed: bool,
}

impl ResolutionWait {
    /// Wait for the synchronous resolver that owns this key.
    ///
    /// This cache coordinates filesystem parsing/verification and is
    /// deliberately synchronous. Callers must invoke this only from the
    /// blocking admission/preparation lane, never from a Tokio worker.
    pub fn wait_blocking(self) -> Result<Option<Arc<ResolvedClosure>>, SharedResolutionFailure> {
        let mut result = self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while result.is_none() {
            result = self
                .pending
                .wake
                .wait(result)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let closure = match result
            .as_ref()
            .expect("completed resolution carries an explicit outcome")
        {
            PendingResolutionOutcome::Success(closure) => closure.clone(),
            PendingResolutionOutcome::Failure(error) => return Err(error.clone()),
            PendingResolutionOutcome::Retry => return Ok(None),
        };
        drop(result);

        let relocated = Arc::new(
            closure
                .relocated_for_binding(&self.materialization)
                .map_err(SharedResolutionFailure::from_anyhow)?,
        );
        let Some(inputs) = revalidation_inputs(
            &self.key,
            &relocated,
            relocated.probed_absent(),
            self.materialization.active_project_root(),
        ) else {
            return self.revalidation_mismatch(
                "single-flight resolution cannot be rebound to its admitted authority",
            );
        };
        if revalidate_for_binding(&self.materialization, &inputs) {
            Ok(Some(relocated))
        } else {
            self.revalidation_mismatch(
                "single-flight resolution differs from its admitted authority",
            )
        }
    }

    fn revalidation_mismatch(
        &self,
        reason: &'static str,
    ) -> Result<Option<Arc<ResolvedClosure>>, SharedResolutionFailure> {
        if !self
            .materialization
            .subject_authority()
            .permits_mutable_revalidation()
        {
            return Err(SharedResolutionFailure::new(format!(
                "{reason}; content-addressed authority cannot be retried as mutable"
            )));
        }
        Ok(None)
    }
}

impl ResolutionFillGuard<'_> {
    pub fn complete(
        mut self,
        closure: Arc<ResolvedClosure>,
        probed_absent: Vec<ProbedAbsence>,
    ) -> Result<Option<Arc<ResolvedClosure>>, SharedResolutionFailure> {
        let closure = match closure.relocated_for_binding(&self.materialization) {
            Ok(closure) => Arc::new(closure),
            Err(error) => return Err(self.fail_anyhow(error)),
        };
        let Some(inputs) = revalidation_inputs(
            &self.key,
            &closure,
            &probed_absent,
            self.materialization.active_project_root(),
        ) else {
            return Err(self.fail_message(
                "fresh resolution closure cannot be bound to its admitted cache authority",
            ));
        };
        if !revalidate_for_binding(&self.materialization, &inputs) {
            if !self
                .materialization
                .subject_authority()
                .permits_mutable_revalidation()
            {
                return Err(self.fail_message(
                    "fresh resolution differs from its content-addressed admitted authority",
                ));
            }
            self.cancel();
            return Ok(None);
        }
        let closure = self.cache.insert(self.key.clone(), closure, probed_absent);
        self.finish(PendingResolutionOutcome::Success(closure.clone()));
        self.completed = true;
        Ok(Some(closure))
    }

    pub fn fail_message(mut self, message: impl Into<String>) -> SharedResolutionFailure {
        let error = SharedResolutionFailure::new(message);
        self.finish(PendingResolutionOutcome::Failure(error.clone()));
        self.completed = true;
        error
    }

    pub fn fail_anyhow(mut self, error: anyhow::Error) -> SharedResolutionFailure {
        let error = SharedResolutionFailure::from_anyhow(error);
        self.finish(PendingResolutionOutcome::Failure(error.clone()));
        self.completed = true;
        error
    }

    pub fn fail_error<E>(mut self, error: E) -> SharedResolutionFailure
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        let error = SharedResolutionFailure(Arc::new(error));
        self.finish(PendingResolutionOutcome::Failure(error.clone()));
        self.completed = true;
        error
    }

    pub fn fail_typed_error<E>(mut self, error: E) -> Arc<E>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        let error = Arc::new(error);
        self.finish(PendingResolutionOutcome::Failure(SharedResolutionFailure(
            error.clone(),
        )));
        self.completed = true;
        error
    }

    fn cancel(&mut self) {
        self.finish(PendingResolutionOutcome::Retry);
        self.completed = true;
    }

    fn finish(&self, result: PendingResolutionOutcome) {
        let mut guard = self
            .cache
            .inner
            .lock()
            .expect("resolution cache mutex poisoned");
        *self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
        if guard
            .pending
            .get(&self.key)
            .is_some_and(|pending| Arc::ptr_eq(pending, &self.pending))
        {
            guard.pending.remove(&self.key);
        }
        drop(guard);
        self.pending.wake.notify_all();
    }
}

impl Drop for ResolutionFillGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.finish(PendingResolutionOutcome::Failure(
                SharedResolutionFailure::new(
                    "resolution cache fill ended without publishing its result",
                ),
            ));
        }
    }
}

impl SharedResolutionFailure {
    fn new(message: impl Into<String>) -> Self {
        Self(Arc::new(ResolutionFailureMessage {
            message: message.into(),
        }))
    }

    fn from_anyhow(error: anyhow::Error) -> Self {
        Self(Arc::new(SharedAnyhowFailure(error)))
    }

    pub fn downcast<T>(&self) -> Option<Arc<T>>
    where
        T: std::error::Error + Send + Sync + 'static,
    {
        self.0.clone().into_any().downcast::<T>().ok()
    }

    #[cfg(test)]
    fn shares_identity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Outcome of a lookup, for timing/observability at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupOutcome {
    /// No entry for the key.
    Miss,
    /// Entry present but revalidation failed; it was evicted.
    Stale,
    /// Entry present and revalidated fresh.
    Hit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionCacheMetric {
    Core,
    Root,
    Compose,
    LaunchBinding,
}

impl ResolutionCacheMetric {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Core => "resolution_cache",
            Self::Root => "root_resolution_cache",
            Self::Compose => "compose_resolution_cache",
            Self::LaunchBinding => "launch_binding_resolution_cache",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionCachePhase {
    Lookup,
    PreResolve,
    Admission,
    Compose,
    Binding,
}

impl ResolutionCachePhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Lookup => "lookup",
            Self::PreResolve => "pre_resolve",
            Self::Admission => "admission",
            Self::Compose => "compose",
            Self::Binding => "binding",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionCacheOutcome {
    Hit,
    SingleFlightWait,
    Miss,
    Bypass,
    Stale,
    Eviction,
}

impl ResolutionCacheOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::SingleFlightWait => "single_flight_wait",
            Self::Miss => "miss",
            Self::Bypass => "bypass",
            Self::Stale => "stale",
            Self::Eviction => "eviction",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionCacheReason {
    Ready,
    PendingCapacity,
    EntryTooLarge,
    GenerationRetired,
    AttestationRevalidationFailed,
}

impl ResolutionCacheReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::PendingCapacity => "pending_capacity",
            Self::EntryTooLarge => "entry_too_large",
            Self::GenerationRetired => "generation_retired",
            Self::AttestationRevalidationFailed => "attestation_revalidation_failed",
        }
    }
}

pub fn emit_resolution_cache_metric(
    metric: ResolutionCacheMetric,
    phase: ResolutionCachePhase,
    outcome: ResolutionCacheOutcome,
    reason: Option<ResolutionCacheReason>,
    entry_bytes: usize,
) {
    tracing::info!(
        target: "ryeos.metrics",
        metric = metric.as_str(),
        phase = phase.as_str(),
        outcome = outcome.as_str(),
        reason = reason.map(ResolutionCacheReason::as_str),
        entry_bytes,
        "resolution cache metric"
    );
}

impl LookupOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Miss => "miss",
            Self::Stale => "stale",
            Self::Hit => "hit",
        }
    }
}

impl ResolutionCache {
    /// Create a cache holding at most `capacity` entries (a node resolves few
    /// distinct roots; tens of entries is ample).
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                slots: HashMap::new(),
                pending: HashMap::new(),
                next_seq: 0,
                total_bytes: 0,
                active_generation: None,
            }),
            capacity: capacity.max(1),
        }
    }

    /// Coordinate one cold resolution per exact authority key. Waiters retry
    /// lookup after the leader publishes or fails; failures are never cached.
    pub fn begin_admitted(
        &self,
        key: &ResolutionCacheKey,
        binding: &ResolutionMaterializationBinding,
    ) -> anyhow::Result<ResolutionLookup<'_>> {
        binding.validate_once()?.begin(self, key)
    }

    #[cfg(test)]
    pub fn begin(
        &self,
        key: &ResolutionCacheKey,
        active_project_root: Option<&std::path::Path>,
    ) -> ResolutionLookup<'_> {
        self.begin_for_binding(
            key,
            &ResolutionMaterializationBinding {
                subject_authority: key.subject_authority.clone(),
                active_project_root: active_project_root.map(PathBuf::from),
                materialization_lifeline: None,
                pinned_materialization: None,
                allow_unproven_materialization: true,
            },
        )
    }

    fn begin_for_binding(
        &self,
        key: &ResolutionCacheKey,
        binding: &ResolutionMaterializationBinding,
    ) -> ResolutionLookup<'_> {
        loop {
            {
                let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
                sweep_idle(&mut guard);
                retire_previous_generation(&mut guard, key);
            }
            if let (Some(closure), LookupOutcome::Hit) = self.get_for_binding(key, binding) {
                return ResolutionLookup::Hit(closure);
            }
            let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
            if guard.slots.contains_key(key) {
                drop(guard);
                continue;
            }
            if let Some(pending) = guard.pending.get(key) {
                return ResolutionLookup::Wait(ResolutionWait {
                    pending: pending.clone(),
                    key: key.clone(),
                    materialization: binding.clone(),
                });
            }
            if guard.pending.len() >= MAX_PENDING {
                emit_resolution_cache_metric(
                    ResolutionCacheMetric::Core,
                    ResolutionCachePhase::Lookup,
                    ResolutionCacheOutcome::Bypass,
                    Some(ResolutionCacheReason::PendingCapacity),
                    0,
                );
                return ResolutionLookup::Bypass;
            }
            let pending = Arc::new(PendingResolution::default());
            guard.pending.insert(key.clone(), pending.clone());
            return ResolutionLookup::Build(ResolutionFillGuard {
                cache: self,
                key: key.clone(),
                materialization: binding.clone(),
                pending,
                completed: false,
            });
        }
    }

    /// Look up `key`, revalidating any entry against current on-disk content.
    /// Returns the cached output only when it is proven still current; a stale
    /// entry is evicted and reported as [`LookupOutcome::Stale`] so the caller
    /// recomputes and re-inserts.
    pub fn get_admitted(
        &self,
        key: &ResolutionCacheKey,
        binding: &ResolutionMaterializationBinding,
    ) -> anyhow::Result<(Option<Arc<ResolvedClosure>>, LookupOutcome)> {
        binding.validate_once()?.get(self, key)
    }

    /// Validate exact caller-held closures under one materialization
    /// fence. The opaque binding is proved once; every pair still receives its
    /// own key/authority check and positive/negative dependency validation.
    pub fn validate_admitted_closures_status(
        &self,
        binding: &ResolutionMaterializationBinding,
        closures: &[(&ResolutionCacheKey, &ResolvedClosure)],
    ) -> anyhow::Result<AdmittedClosureValidation> {
        binding.validate()?;
        for (key, closure) in closures {
            binding.validate_key_after_binding(key)?;
            let relocated = closure.relocated_for_validated_binding(binding)?;
            let Some(inputs) = revalidation_inputs(
                key,
                &relocated,
                relocated.probed_absent(),
                binding.active_project_root(),
            ) else {
                return Ok(
                    if binding.subject_authority().permits_mutable_revalidation() {
                        AdmittedClosureValidation::MutableAuthorityChanged
                    } else {
                        AdmittedClosureValidation::ImmutableAuthorityMismatch
                    },
                );
            };
            if !revalidate_for_binding(binding, &inputs) {
                return Ok(
                    if binding.subject_authority().permits_mutable_revalidation() {
                        AdmittedClosureValidation::MutableAuthorityChanged
                    } else {
                        AdmittedClosureValidation::ImmutableAuthorityMismatch
                    },
                );
            }
        }
        Ok(AdmittedClosureValidation::Current)
    }

    #[cfg(test)]
    pub fn get(
        &self,
        key: &ResolutionCacheKey,
        active_project_root: Option<&std::path::Path>,
    ) -> (Option<Arc<ResolvedClosure>>, LookupOutcome) {
        self.get_for_binding(
            key,
            &ResolutionMaterializationBinding {
                subject_authority: key.subject_authority.clone(),
                active_project_root: active_project_root.map(PathBuf::from),
                materialization_lifeline: None,
                pinned_materialization: None,
                allow_unproven_materialization: true,
            },
        )
    }

    fn get_for_binding(
        &self,
        key: &ResolutionCacheKey,
        binding: &ResolutionMaterializationBinding,
    ) -> (Option<Arc<ResolvedClosure>>, LookupOutcome) {
        let active_project_root = binding.active_project_root.as_deref();
        let authority_shape_matches = match &key.subject_authority {
            SubjectResolutionAuthority::Projectless
            | SubjectResolutionAuthority::PinnedGeneration { .. }
            | SubjectResolutionAuthority::CowWorkspace { .. } => key.live_project_root.is_none(),
            SubjectResolutionAuthority::LiveFs => {
                key.live_project_root.as_deref() == active_project_root
            }
        };
        if !authority_shape_matches {
            return (None, LookupOutcome::Miss);
        }
        // Snapshot both the exact closure and its small revalidation inputs
        // under the lock, then do filesystem I/O UNLOCKED. COW workspaces
        // intentionally share a content-generation key across disposable
        // paths, so a concurrent replacement may belong to a different active
        // workspace. Return only the exact closure proved against this
        // caller's active root; never re-fetch an unvalidated replacement.
        let (closure, inputs) = {
            let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
            sweep_idle(&mut guard);
            match guard.slots.get_mut(key) {
                Some(entry) => {
                    entry.last_touched = Instant::now();
                    (
                        entry.closure.clone(),
                        revalidation_inputs(
                            key,
                            &entry.closure,
                            entry.closure.probed_absent(),
                            active_project_root,
                        ),
                    )
                }
                None => return (None, LookupOutcome::Miss),
            }
        };
        let Some(inputs) = inputs else {
            let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
            if guard
                .slots
                .get(key)
                .is_some_and(|entry| Arc::ptr_eq(&entry.closure, &closure))
            {
                remove_entry(&mut guard, key);
            }
            return (None, LookupOutcome::Stale);
        };
        if revalidate_for_binding(binding, &inputs) {
            match closure.relocated_for_validated_binding(binding) {
                Ok(relocated) => {
                    let relocated = Arc::new(relocated);
                    let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
                    if let Some(entry) = guard
                        .slots
                        .get_mut(key)
                        .filter(|entry| Arc::ptr_eq(&entry.closure, &closure))
                    {
                        entry.last_touched = Instant::now();
                    }
                    (Some(relocated), LookupOutcome::Hit)
                }
                Err(_) => {
                    let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
                    if guard
                        .slots
                        .get(key)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.closure, &closure))
                    {
                        remove_entry(&mut guard, key);
                    }
                    (None, LookupOutcome::Stale)
                }
            }
        } else {
            let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
            if guard
                .slots
                .get(key)
                .is_some_and(|entry| Arc::ptr_eq(&entry.closure, &closure))
            {
                remove_entry(&mut guard, key);
            }
            (None, LookupOutcome::Stale)
        }
    }

    /// Store a freshly computed resolution for `key`. Evicts the oldest entry
    /// when at capacity (unless replacing an existing key).
    fn insert(
        &self,
        key: ResolutionCacheKey,
        closure: Arc<ResolvedClosure>,
        probed_absent: Vec<ProbedAbsence>,
    ) -> Arc<ResolvedClosure> {
        let active = closure.with_probed_absent(probed_absent);
        let token = Arc::new(());
        let active = Arc::new(active.with_cache_entry_token(token.clone()));
        let cached = Arc::new(
            active
                .without_materialization_lifeline()
                .with_cache_entry_token(token),
        );
        let estimated_bytes = resolution_entry_bytes(&key, &cached);
        if estimated_bytes > MAX_ENTRY_BYTES || estimated_bytes > MAX_TOTAL_BYTES {
            emit_resolution_cache_metric(
                ResolutionCacheMetric::Core,
                ResolutionCachePhase::Lookup,
                ResolutionCacheOutcome::Bypass,
                Some(ResolutionCacheReason::EntryTooLarge),
                estimated_bytes,
            );
            return active;
        }
        let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
        sweep_idle(&mut guard);
        retire_previous_generation(&mut guard, &key);
        let generation_is_current =
            key.generation_epoch
                .map(|epoch| {
                    guard.active_generation.as_ref().is_some_and(
                        |(active_epoch, active_generation)| {
                            *active_epoch == epoch && active_generation == &key.generation
                        },
                    )
                })
                .unwrap_or(true);
        if !generation_is_current {
            return active;
        }
        let seq = guard.next_seq;
        guard.next_seq += 1;
        if guard.slots.contains_key(&key) {
            remove_entry(&mut guard, &key);
        }
        while guard.slots.len() >= self.capacity
            || guard.total_bytes.saturating_add(estimated_bytes) > MAX_TOTAL_BYTES
        {
            if let Some(oldest) = guard
                .slots
                .iter()
                .min_by_key(|(_, entry)| entry.seq)
                .map(|(k, _)| k.clone())
            {
                remove_entry(&mut guard, &oldest);
            } else {
                break;
            }
        }
        if guard.slots.len() >= self.capacity
            || guard.total_bytes.saturating_add(estimated_bytes) > MAX_TOTAL_BYTES
        {
            return active;
        }
        guard.total_bytes = guard.total_bytes.saturating_add(estimated_bytes);
        guard.slots.insert(
            key,
            Entry {
                closure: cached,
                estimated_bytes,
                last_touched: Instant::now(),
                seq,
            },
        );
        active
    }

    /// Upgrade one exact cached closure with engine-owned verified source
    /// evidence without disturbing its positive/negative dependency proof.
    /// A concurrent replacement is left untouched.
    pub fn attach_verified_attestation_if_same(
        &self,
        key: &ResolutionCacheKey,
        expected: &Arc<ResolvedClosure>,
        upgraded: Arc<ResolvedClosure>,
    ) {
        let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
        let Some((previous_bytes, entry_token)) = guard.slots.get(key).and_then(|entry| {
            entry
                .closure
                .belongs_to_same_cache_entry(expected)
                .then(|| {
                    (
                        entry.estimated_bytes,
                        entry
                            .closure
                            .cache_entry_token
                            .as_ref()
                            .expect("cache entries always carry an identity token")
                            .clone(),
                    )
                })
        }) else {
            return;
        };
        let upgraded = Arc::new(
            upgraded
                .without_materialization_lifeline()
                .with_cache_entry_token(entry_token),
        );
        let upgraded_bytes = resolution_entry_bytes(key, &upgraded);
        let prospective = guard
            .total_bytes
            .saturating_sub(previous_bytes)
            .saturating_add(upgraded_bytes);
        if upgraded_bytes <= MAX_ENTRY_BYTES && prospective <= MAX_TOTAL_BYTES {
            guard.total_bytes = prospective;
            let entry = guard
                .slots
                .get_mut(key)
                .expect("entry checked while cache lock is held");
            entry.estimated_bytes = upgraded_bytes;
            entry.last_touched = Instant::now();
            entry.closure = upgraded;
        }
    }

    pub fn discard_if_same(&self, key: &ResolutionCacheKey, expected: &Arc<ResolvedClosure>) {
        let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
        if guard
            .slots
            .get(key)
            .is_some_and(|entry| entry.closure.belongs_to_same_cache_entry(expected))
        {
            remove_entry(&mut guard, key);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().slots.len()
    }
}

fn remove_entry(inner: &mut Inner, key: &ResolutionCacheKey) -> usize {
    let Some(entry) = inner.slots.remove(key) else {
        return 0;
    };
    inner.total_bytes = inner.total_bytes.saturating_sub(entry.estimated_bytes);
    entry.estimated_bytes
}

fn resolution_entry_bytes(key: &ResolutionCacheKey, closure: &ResolvedClosure) -> usize {
    closure
        .estimated_bytes()
        .saturating_add(key.generation.capacity())
        .saturating_add(key.canonical_ref.capacity())
        .saturating_add(key.plan_context_identity.capacity())
        .saturating_add(
            key.live_project_root
                .as_ref()
                .map(|path| path.as_os_str().as_encoded_bytes().len())
                .unwrap_or(0),
        )
        .saturating_add(
            serde_json::to_vec(&key.subject_authority)
                .map(|serialized| serialized.len())
                .unwrap_or(MAX_ENTRY_BYTES.saturating_add(1)),
        )
}

fn retire_previous_generation(inner: &mut Inner, key: &ResolutionCacheKey) {
    let Some(epoch) = key.generation_epoch else {
        return;
    };
    if let Some((active_epoch, active_generation)) = inner.active_generation.as_ref() {
        if *active_epoch > epoch || (*active_epoch == epoch && active_generation == &key.generation)
        {
            return;
        }
    }
    inner.active_generation = Some((epoch, key.generation.clone()));
    let stale = inner
        .slots
        .keys()
        .filter(|candidate| {
            candidate.generation_epoch != Some(epoch) || candidate.generation != key.generation
        })
        .cloned()
        .collect::<Vec<_>>();
    for stale_key in stale {
        let entry_bytes = remove_entry(inner, &stale_key);
        emit_resolution_cache_metric(
            ResolutionCacheMetric::Core,
            ResolutionCachePhase::Lookup,
            ResolutionCacheOutcome::Eviction,
            Some(ResolutionCacheReason::GenerationRetired),
            entry_bytes,
        );
    }
}

fn sweep_idle(inner: &mut Inner) {
    let now = Instant::now();
    let stale = inner
        .slots
        .iter()
        .filter(|(_, entry)| now.duration_since(entry.last_touched) >= IDLE_TTL)
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in stale {
        remove_entry(inner, &key);
    }
}

/// The project-space revalidation inputs of a cached entry — small enough to
/// clone out from under the cache lock so the disk I/O runs unlocked. Bundle
/// dependencies are omitted: they are immutable within the generation (in the
/// key), so they are never re-read.
struct RevalidationInputs {
    /// `(source_path, whole-file digest)` for each project-space positive dep.
    positives: Vec<(PathBuf, String)>,
    /// Recorded project-space absences that must still be absent.
    absences: Vec<PathBuf>,
}

fn revalidation_inputs(
    key: &ResolutionCacheKey,
    closure: &ResolvedClosure,
    probed_absent: &[ProbedAbsence],
    active_project_root: Option<&std::path::Path>,
) -> Option<RevalidationInputs> {
    if closure.subject_authority() != &key.subject_authority {
        return None;
    }
    if matches!(
        key.subject_authority,
        SubjectResolutionAuthority::Projectless
    ) {
        if closure.resolution_root().is_some()
            || active_project_root.is_some()
            || std::iter::once(&closure.output().root)
                .chain(closure.output().ancestors.iter())
                .chain(closure.output().referenced_items.iter())
                .any(|dependency| dependency.source_space == ItemSpace::Project)
            || probed_absent
                .iter()
                .any(|absence| absence.space == ItemSpace::Project)
        {
            return None;
        }
        return Some(RevalidationInputs {
            positives: Vec::new(),
            absences: Vec::new(),
        });
    }
    let source_root = closure.resolution_root()?;
    let active_root = active_project_root?;
    let relocate = |path: &std::path::Path| {
        path.strip_prefix(source_root)
            .ok()
            .map(|relative| active_root.join(relative))
    };
    let positives = std::iter::once(&closure.output().root)
        .chain(closure.output().ancestors.iter())
        .chain(closure.output().referenced_items.iter())
        .filter(|dependency| dependency.source_space == ItemSpace::Project)
        .map(|dependency| {
            relocate(&dependency.source_path)
                .map(|path| (path, dependency.source_content_digest.clone()))
        })
        .collect::<Option<Vec<_>>>()?;
    let absences = probed_absent
        .iter()
        .filter(|absence| absence.space == ItemSpace::Project)
        .map(|absence| relocate(&absence.path))
        .collect::<Option<Vec<_>>>()?;
    Some(RevalidationInputs {
        positives,
        absences,
    })
}

/// Prove a cached resolution is still current, from content only. Runs UNLOCKED
/// on the snapshotted inputs. Project positives must still hash to their
/// recorded whole-file digest; project absences must still be absent. Any
/// deviation — a changed, removed, or unreadable positive, or an appeared
/// shadow — fails closed.
fn revalidate(inputs: &RevalidationInputs) -> bool {
    for (source_path, digest) in &inputs.positives {
        match lillux::read_optional_regular_file_bounded_no_follow(
            source_path,
            ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES,
        ) {
            Ok(Some(content)) if &lillux::sha256_hex(&content) == digest => {}
            _ => return false,
        }
    }
    for path in &inputs.absences {
        // Match the resolver's own probe. A regular file at the path is an
        // item that would now shadow the cached winner → stale. A directory or
        // dangling symlink is not an item. A non-NotFound error (e.g. EACCES)
        // is where a fresh resolve would hard-fail, so fail closed rather than
        // read it as "still absent".
        match lillux::inspect_optional_entry_no_follow(path) {
            Ok(Some(lillux::PinnedEntryType::Regular | lillux::PinnedEntryType::Symlink)) => {
                return false;
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(_) => return false,
        }
    }
    true
}

fn revalidate_for_binding(
    binding: &ResolutionMaterializationBinding,
    inputs: &RevalidationInputs,
) -> bool {
    if let Some(materialization) = binding.pinned_materialization.as_ref() {
        return inputs.positives.iter().all(|(path, digest)| {
            materialization
                .validates_observed_file(path, digest)
                .unwrap_or(false)
        }) && inputs.absences.iter().all(|path| {
            materialization
                .validates_observed_absence(path)
                .unwrap_or(false)
        });
    }
    revalidate(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::resolution::{
        KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
    };
    use ryeos_state::objects::ProjectFile;
    use std::path::Path;

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir = std::env::temp_dir().join(format!(
            "rye_reso_cache_test_{}_{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) -> String {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
        lillux::signature::content_hash(content)
    }

    fn pinned_binding(
        authority: SubjectResolutionAuthority,
        active_root: &Path,
        files: impl IntoIterator<Item = (&'static str, String, u64)>,
    ) -> ResolutionMaterializationBinding {
        let snapshot_hash = authority
            .operational_generation()
            .expect("pinned test authority has a generation")
            .to_owned();
        let expected_tree = files
            .into_iter()
            .map(|(relative, blob_hash, size)| {
                (
                    relative.to_owned(),
                    ProjectFile {
                        blob_hash,
                        size,
                        normalized_mode: ProjectFile::REGULAR_MODE,
                    },
                )
            })
            .collect();
        let materialization =
            ryeos_state::PinnedProjectMaterialization::from_observed_tree_for_test(
                snapshot_hash,
                active_root,
                expected_tree,
            )
            .unwrap();
        ResolutionMaterializationBinding::admitted(
            authority,
            Some(active_root.to_path_buf()),
            Some(Arc::new(TempDirGuard::new(active_root.to_path_buf()))),
            Some(materialization),
        )
        .unwrap()
    }

    fn ancestor(space: ItemSpace, source_path: PathBuf, digest: String) -> ResolvedAncestor {
        ResolvedAncestor {
            requested_id: "x".into(),
            resolved_ref: "tool:x".into(),
            source_path,
            source_space: space,
            trust_class: match space {
                ItemSpace::Project => TrustClass::TrustedProject,
                ItemSpace::Bundle => TrustClass::TrustedBundle,
                ItemSpace::Node => TrustClass::TrustedNode,
            },
            signer_fingerprint: Some("fixture-signer".to_string()),
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: String::new(),
            source_content_digest: digest,
            raw_content_digest: String::new(),
        }
    }

    fn output_with_root(root: ResolvedAncestor) -> ResolutionOutput {
        ResolutionOutput {
            root,
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: Default::default(),
            effective_trust_class: TrustClass::TrustedProject,
            composed: KindComposedView::identity(serde_json::json!({})),
        }
    }

    fn key(project_root: &Path) -> ResolutionCacheKey {
        ResolutionCacheKey {
            generation: "generation-1".into(),
            generation_epoch: None,
            canonical_ref: "tool:x".into(),
            subject_authority: SubjectResolutionAuthority::LiveFs,
            live_project_root: Some(project_root.to_path_buf()),
            plan_context_identity: String::new(),
        }
    }

    fn generation_key(
        project_root: &Path,
        generation: &str,
        epoch: u64,
        identity: &str,
    ) -> ResolutionCacheKey {
        let mut key = key(project_root);
        key.generation = generation.to_owned();
        key.generation_epoch = Some(epoch);
        key.canonical_ref = format!("tool:{identity}");
        key
    }

    fn projectless_key() -> ResolutionCacheKey {
        ResolutionCacheKey {
            generation: "generation-1".into(),
            generation_epoch: None,
            canonical_ref: "tool:x".into(),
            subject_authority: SubjectResolutionAuthority::Projectless,
            live_project_root: None,
            plan_context_identity: String::new(),
        }
    }

    fn closure(output: ResolutionOutput, project_root: &Path) -> Arc<ResolvedClosure> {
        Arc::new(
            ResolvedClosure::new(
                output,
                SubjectResolutionAuthority::LiveFs,
                Some(project_root.to_path_buf()),
                None,
            )
            .unwrap(),
        )
    }

    #[test]
    fn projectless_cold_fill_becomes_a_warm_hit_without_any_project_root() {
        let cache = ResolutionCache::new(8);
        let key = projectless_key();
        let resolved = Arc::new(
            ResolvedClosure::new(
                output_with_root(ancestor(
                    ItemSpace::Bundle,
                    PathBuf::from("/bundle/.ai/tools/x.yaml"),
                    "bundle-digest".into(),
                )),
                SubjectResolutionAuthority::Projectless,
                None,
                None,
            )
            .unwrap(),
        );
        let ResolutionLookup::Build(fill) = cache.begin(&key, None) else {
            panic!("projectless cold lookup must own the fill");
        };
        let published = fill.complete(resolved, Vec::new()).unwrap().unwrap();
        assert_eq!(
            published.subject_authority(),
            &SubjectResolutionAuthority::Projectless
        );
        assert!(matches!(cache.begin(&key, None), ResolutionLookup::Hit(_)));
    }

    #[test]
    fn diagnostic_closure_revalidates_projectless_and_live_dependencies() {
        let projectless = ResolvedClosure::new_with_probes(
            output_with_root(ancestor(
                ItemSpace::Bundle,
                PathBuf::from("/bundle/.ai/tools/x.yaml"),
                "bundle-digest".into(),
            )),
            SubjectResolutionAuthority::Projectless,
            None,
            None,
            Vec::new(),
        )
        .unwrap();
        assert!(
            projectless
                .validates_current_diagnostic_authority()
                .unwrap()
        );

        let root = tempdir();
        let item = root.join(".ai/tools/x.yaml");
        let digest = write(&item, "name: x\n");
        let live = ResolvedClosure::new_with_probes(
            output_with_root(ancestor(ItemSpace::Project, item.clone(), digest)),
            SubjectResolutionAuthority::LiveFs,
            Some(root),
            None,
            Vec::new(),
        )
        .unwrap();
        assert!(live.validates_current_diagnostic_authority().unwrap());
        write(&item, "name: changed\n");
        assert!(!live.validates_current_diagnostic_authority().unwrap());
    }

    #[test]
    fn project_positive_dependency_change_invalidates() {
        let dir = tempdir();
        let path = dir.join("item.py");
        let digest = write(&path, "# original");
        let cache = ResolutionCache::new(8);
        let key = key(&dir);
        cache.insert(
            key.clone(),
            closure(
                output_with_root(ancestor(ItemSpace::Project, path.clone(), digest)),
                &dir,
            ),
            Vec::new(),
        );

        // Unchanged → hit.
        let (hit, outcome) = cache.get(&key, Some(&dir));
        assert!(hit.is_some());
        assert_eq!(outcome, LookupOutcome::Hit);

        // A single-byte change to the project source → stale, evicted.
        write(&path, "# changed");
        let (miss, outcome) = cache.get(&key, Some(&dir));
        assert!(miss.is_none());
        assert_eq!(outcome, LookupOutcome::Stale);
        assert_eq!(cache.len(), 0, "stale entry must be evicted");
    }

    #[test]
    fn appearing_shadow_at_probed_absent_path_invalidates() {
        // The case the whole design exists for: a BUNDLE winner while a
        // project slot is empty. The digest of the bundle winner never
        // changes, so only the recorded absence catches the new shadow.
        let dir = tempdir();
        let bundle_item = dir.join("bundle_item.py");
        let bundle_digest = write(&bundle_item, "# bundle winner");
        let project_slot = dir.join("project_slot.py"); // absent for now

        let cache = ResolutionCache::new(8);
        let key = key(&dir);
        cache.insert(
            key.clone(),
            closure(
                output_with_root(ancestor(ItemSpace::Bundle, bundle_item, bundle_digest)),
                &dir,
            ),
            vec![ProbedAbsence {
                space: ItemSpace::Project,
                path: project_slot.clone(),
            }],
        );

        // Absence still holds → hit.
        assert_eq!(cache.get(&key, Some(&dir)).1, LookupOutcome::Hit);

        // A project item appears where the winner was probed absent → the
        // resolution would now select a different (higher-precedence) winner.
        write(&project_slot, "# project shadow");
        assert_eq!(cache.get(&key, Some(&dir)).1, LookupOutcome::Stale);
    }

    #[test]
    fn bundle_dependencies_are_not_revalidated() {
        // A bundle-space positive whose file does not even exist on disk still
        // validates: bundle content is immutable within the generation, which
        // is in the key. This is the genuinely-free tier.
        let cache = ResolutionCache::new(8);
        let project_root = PathBuf::from("/unused-live-root");
        let key = key(&project_root);
        cache.insert(
            key.clone(),
            closure(
                output_with_root(ancestor(
                    ItemSpace::Bundle,
                    PathBuf::from("/nonexistent/bundle/item.py"),
                    "digest-that-matches-nothing".into(),
                )),
                &project_root,
            ),
            vec![ProbedAbsence {
                // A bundle-space absence is likewise not re-probed.
                space: ItemSpace::Bundle,
                path: PathBuf::from("/nonexistent/bundle/other.py"),
            }],
        );
        assert_eq!(cache.get(&key, Some(&project_root)).1, LookupOutcome::Hit);
    }

    #[test]
    fn generation_is_part_of_identity() {
        let cache = ResolutionCache::new(8);
        let project_root = PathBuf::from("/unused-live-root");
        let mut k = key(&project_root);
        cache.insert(
            k.clone(),
            closure(
                output_with_root(ancestor(
                    ItemSpace::Bundle,
                    PathBuf::from("/b/item.py"),
                    "d".into(),
                )),
                &project_root,
            ),
            Vec::new(),
        );
        // A bumped generation is a different key: miss, not a stale hit.
        k.generation = "generation-2".into();
        assert_eq!(cache.get(&k, Some(&project_root)).1, LookupOutcome::Miss);
    }

    #[test]
    fn eviction_drops_the_oldest_and_keeps_the_newest() {
        let cache = ResolutionCache::new(2);
        let project_root = PathBuf::from("/unused-live-root");
        let mut keys = Vec::new();
        for i in 0..3 {
            let mut k = key(&project_root);
            k.canonical_ref = format!("tool:x{i}");
            keys.push(k.clone());
            cache.insert(
                k,
                closure(
                    output_with_root(ancestor(
                        ItemSpace::Bundle,
                        PathBuf::from("/b/item.py"),
                        "d".into(),
                    )),
                    &project_root,
                ),
                Vec::new(),
            );
        }
        assert_eq!(cache.len(), 2, "capacity bound holds");
        // The two most-recently-inserted survive; the oldest was evicted.
        assert_eq!(
            cache.get(&keys[0], Some(&project_root)).1,
            LookupOutcome::Miss,
            "oldest evicted"
        );
        assert_eq!(
            cache.get(&keys[1], Some(&project_root)).1,
            LookupOutcome::Hit,
            "newer kept"
        );
        assert_eq!(
            cache.get(&keys[2], Some(&project_root)).1,
            LookupOutcome::Hit,
            "newest kept"
        );
    }

    #[test]
    fn oversize_resolution_is_returned_but_never_cached() {
        let project_root = PathBuf::from("/unused-live-root");
        let cache = ResolutionCache::new(8);
        let key = key(&project_root);
        let mut output = output_with_root(ancestor(
            ItemSpace::Bundle,
            PathBuf::from("/b/item.py"),
            "d".into(),
        ));
        output.root.raw_content = "x".repeat(MAX_ENTRY_BYTES.saturating_add(1));
        cache.insert(key.clone(), closure(output, &project_root), Vec::new());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get(&key, Some(&project_root)).1, LookupOutcome::Miss);
    }

    #[test]
    fn deleted_project_positive_invalidates() {
        let dir = tempdir();
        let path = dir.join("gone.py");
        let digest = write(&path, "# here");
        let cache = ResolutionCache::new(8);
        let key = key(&dir);
        cache.insert(
            key.clone(),
            closure(
                output_with_root(ancestor(ItemSpace::Project, path.clone(), digest)),
                &dir,
            ),
            Vec::new(),
        );
        assert_eq!(cache.get(&key, Some(&dir)).1, LookupOutcome::Hit);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            cache.get(&key, Some(&dir)).1,
            LookupOutcome::Stale,
            "deleted dep is stale"
        );
    }

    #[test]
    fn mixed_project_and_bundle_chain_revalidates_only_the_project_dep() {
        let dir = tempdir();
        let project_dep = dir.join("proj.py");
        let project_digest = write(&project_dep, "# v1");
        let cache = ResolutionCache::new(8);
        // root = project item; one bundle ancestor whose path does not exist.
        let mut output = output_with_root(ancestor(
            ItemSpace::Project,
            project_dep.clone(),
            project_digest,
        ));
        output.ancestors.push(ancestor(
            ItemSpace::Bundle,
            PathBuf::from("/nonexistent/bundle/parent.py"),
            "bundle-digest".into(),
        ));
        let key = key(&dir);
        cache.insert(key.clone(), closure(output, &dir), Vec::new());
        // Bundle ancestor is never re-read (would fail); project dep unchanged → hit.
        assert_eq!(cache.get(&key, Some(&dir)).1, LookupOutcome::Hit);
        // Change only the project dep → stale, proving the project dep IS checked.
        write(&project_dep, "# v2");
        assert_eq!(cache.get(&key, Some(&dir)).1, LookupOutcome::Stale);
    }

    #[test]
    fn a_directory_at_a_probed_absent_path_does_not_invalidate() {
        let dir = tempdir();
        let bundle_item = dir.join("b.py");
        let bundle_digest = write(&bundle_item, "# bundle");
        let slot = dir.join("slot"); // will become a directory, not an item file
        let cache = ResolutionCache::new(8);
        let key = key(&dir);
        cache.insert(
            key.clone(),
            closure(
                output_with_root(ancestor(ItemSpace::Bundle, bundle_item, bundle_digest)),
                &dir,
            ),
            vec![ProbedAbsence {
                space: ItemSpace::Project,
                path: slot.clone(),
            }],
        );
        assert_eq!(cache.get(&key, Some(&dir)).1, LookupOutcome::Hit);
        // A directory is not a resolvable item — the resolver probes is_file(),
        // so this must NOT invalidate.
        std::fs::create_dir(&slot).unwrap();
        assert_eq!(cache.get(&key, Some(&dir)).1, LookupOutcome::Hit);
    }

    #[test]
    fn pinned_identity_hits_across_distinct_materialization_paths() {
        let source_root = tempdir();
        let active_root = tempdir();
        let source_item = source_root.join(".ai/tools/x.yaml");
        let active_item = active_root.join(".ai/tools/x.yaml");
        let digest = write(&source_item, "name: x\n");
        write(&active_item, "name: x\n");
        let snapshot_hash = "a".repeat(64);
        let authority = SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: snapshot_hash.clone(),
        };
        let key = ResolutionCacheKey {
            generation: "generation-1".into(),
            generation_epoch: None,
            canonical_ref: "tool:x".into(),
            subject_authority: authority.clone(),
            live_project_root: None,
            plan_context_identity: "engine\u{1f}parser\u{1f}trust".into(),
        };
        let lifeline = Arc::new(TempDirGuard::new(source_root.clone()));
        let closure = Arc::new(
            ResolvedClosure::new(
                output_with_root(ancestor(ItemSpace::Project, source_item, digest.clone())),
                authority,
                Some(source_root),
                Some(lifeline),
            )
            .unwrap(),
        );
        let cache = ResolutionCache::new(8);
        cache.insert(key.clone(), closure, Vec::new());
        let binding = pinned_binding(
            key.subject_authority.clone(),
            &active_root,
            [(".ai/tools/x.yaml", digest, "name: x\n".len() as u64)],
        );

        assert_eq!(
            cache.get_admitted(&key, &binding).unwrap().1,
            LookupOutcome::Hit,
            "temporary checkout path is not pinned project identity"
        );

        let mut other_generation = key.clone();
        other_generation.subject_authority = SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: "b".repeat(64),
        };
        assert_eq!(
            cache
                .get_admitted(&other_generation, &binding)
                .unwrap_err()
                .to_string(),
            "resolution cache key differs from admitted subject authority",
            "distinct immutable generations must fail authority binding before lookup"
        );
    }

    #[test]
    fn cow_identity_is_stable_but_revalidates_the_active_workspace() {
        let source_root = tempdir();
        let active_root = tempdir();
        let source_item = source_root.join(".ai/tools/x.yaml");
        let active_item = active_root.join(".ai/tools/x.yaml");
        let digest = write(&source_item, "name: x\n");
        write(&active_item, "name: x\n");
        let generation = "c".repeat(64);
        let authority = SubjectResolutionAuthority::CowWorkspace {
            base_snapshot_hash: generation.clone(),
            current_operational_generation: generation,
        };
        let key = ResolutionCacheKey {
            generation: "generation-1".into(),
            generation_epoch: None,
            canonical_ref: "tool:x".into(),
            subject_authority: authority.clone(),
            live_project_root: None,
            plan_context_identity: "engine\u{1f}parser\u{1f}trust".into(),
        };
        let lifeline = Arc::new(TempDirGuard::new(source_root.clone()));
        let closure = Arc::new(
            ResolvedClosure::new(
                output_with_root(ancestor(ItemSpace::Project, source_item, digest)),
                authority,
                Some(source_root),
                Some(lifeline),
            )
            .unwrap(),
        );
        let cache = ResolutionCache::new(8);
        cache.insert(key.clone(), closure, Vec::new());

        assert_eq!(cache.get(&key, Some(&active_root)).1, LookupOutcome::Hit);
        write(&active_item, "name: changed\n");
        assert_eq!(
            cache.get(&key, Some(&active_root)).1,
            LookupOutcome::Stale,
            "an unsealed COW workspace is never treated as immutable"
        );
    }

    #[test]
    fn single_flight_failure_is_shared_exactly_and_reopens_build_slot() {
        let dir = tempdir();
        let cache = ResolutionCache::new(8);
        let key = key(&dir);
        let ResolutionLookup::Build(fill) = cache.begin(&key, Some(&dir)) else {
            panic!("first lookup must own the fill");
        };
        let ResolutionLookup::Wait(wait) = cache.begin(&key, Some(&dir)) else {
            panic!("concurrent lookup must wait for the fill");
        };
        let published = fill.fail_message("exact resolution failure");
        let waited = wait.wait_blocking().unwrap_err();
        assert!(published.shares_identity(&waited));
        assert!(matches!(
            cache.begin(&key, Some(&dir)),
            ResolutionLookup::Build(_)
        ));
    }

    #[test]
    fn single_flight_waiter_consumes_the_exact_non_cacheable_result() {
        let cache = ResolutionCache::new(8);
        let key = projectless_key();
        let ResolutionLookup::Build(fill) = cache.begin(&key, None) else {
            panic!("first lookup must own the fill");
        };
        let ResolutionLookup::Wait(wait) = cache.begin(&key, None) else {
            panic!("concurrent lookup must wait for the fill");
        };
        let mut output = output_with_root(ancestor(
            ItemSpace::Bundle,
            PathBuf::from("/bundle/.ai/tools/x.yaml"),
            "bundle-digest".into(),
        ));
        output.root.raw_content = "x".repeat(MAX_ENTRY_BYTES);
        let resolved = Arc::new(
            ResolvedClosure::new(output, SubjectResolutionAuthority::Projectless, None, None)
                .unwrap(),
        );

        let published = fill.complete(resolved, Vec::new()).unwrap().unwrap();
        assert_eq!(cache.len(), 0, "oversized results must not be retained");
        let waited = wait
            .wait_blocking()
            .unwrap()
            .expect("waiter must receive the leader's admitted result");
        assert!(
            waited.belongs_to_same_cache_entry(&published),
            "waiters must not rebuild a non-cacheable leader result"
        );
    }

    #[test]
    fn positive_dependency_mutation_during_fill_is_not_published() {
        let dir = tempdir();
        let path = dir.join("item.yaml");
        let digest = write(&path, "name: original\n");
        let cache = ResolutionCache::new(8);
        let key = key(&dir);
        let ResolutionLookup::Build(fill) = cache.begin(&key, Some(&dir)) else {
            panic!("first lookup must own the fill");
        };
        let resolved = closure(
            output_with_root(ancestor(ItemSpace::Project, path.clone(), digest)),
            &dir,
        );

        write(&path, "name: changed-before-publication\n");
        assert!(
            fill.complete(resolved, Vec::new()).unwrap().is_none(),
            "a fill whose positive authority changed must be discarded"
        );
        assert_eq!(cache.len(), 0);
        assert!(matches!(
            cache.begin(&key, Some(&dir)),
            ResolutionLookup::Build(_)
        ));
    }

    #[test]
    fn negative_shadow_appearing_during_fill_is_not_published() {
        let dir = tempdir();
        let bundle_item = dir.join("bundle_item.yaml");
        let project_slot = dir.join("project_slot.yaml");
        let digest = write(&bundle_item, "name: bundle\n");
        let cache = ResolutionCache::new(8);
        let key = key(&dir);
        let ResolutionLookup::Build(fill) = cache.begin(&key, Some(&dir)) else {
            panic!("first lookup must own the fill");
        };
        let resolved = closure(
            output_with_root(ancestor(ItemSpace::Bundle, bundle_item, digest)),
            &dir,
        );

        write(&project_slot, "name: project-shadow\n");
        assert!(
            fill.complete(
                resolved,
                vec![ProbedAbsence {
                    space: ItemSpace::Project,
                    path: project_slot,
                }],
            )
            .unwrap()
            .is_none(),
            "a fill whose negative dependency was shadowed must be discarded"
        );
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn pending_fill_keys_are_bounded() {
        let dir = tempdir();
        let cache = ResolutionCache::new(MAX_PENDING + 1);
        let mut fills = Vec::new();
        for index in 0..MAX_PENDING {
            let key = generation_key(&dir, "generation-1", 1, &format!("item-{index}"));
            let ResolutionLookup::Build(fill) = cache.begin(&key, Some(&dir)) else {
                panic!("pending slot {index} must be admitted");
            };
            fills.push(fill);
        }
        let overflow = generation_key(&dir, "generation-1", 1, "overflow");
        assert!(matches!(
            cache.begin(&overflow, Some(&dir)),
            ResolutionLookup::Bypass
        ));
        drop(fills);
    }

    #[test]
    fn newer_generation_retires_old_and_old_fill_cannot_publish() {
        let dir = tempdir();
        let path = dir.join("item.yaml");
        let digest = write(&path, "name: item\n");
        let cache = ResolutionCache::new(8);
        let old = generation_key(&dir, "generation-1", 1, "item");
        let new = generation_key(&dir, "generation-2", 2, "item");
        cache.insert(
            old.clone(),
            closure(
                output_with_root(ancestor(ItemSpace::Project, path.clone(), digest.clone())),
                &dir,
            ),
            Vec::new(),
        );
        cache.insert(
            new.clone(),
            closure(
                output_with_root(ancestor(ItemSpace::Project, path.clone(), digest.clone())),
                &dir,
            ),
            Vec::new(),
        );
        cache.insert(
            old.clone(),
            closure(
                output_with_root(ancestor(ItemSpace::Project, path, digest)),
                &dir,
            ),
            Vec::new(),
        );
        assert_eq!(cache.get(&old, Some(&dir)).1, LookupOutcome::Miss);
        assert_eq!(cache.get(&new, Some(&dir)).1, LookupOutcome::Hit);
    }

    #[test]
    fn pinned_cross_materialization_relocates_negative_probes() {
        let source_root = tempdir();
        let active_root = tempdir();
        let source_item = source_root.join(".ai/tools/x.yaml");
        let source_probe = source_root.join(".ai/tools/shadow.yaml");
        let active_probe = active_root.join(".ai/tools/shadow.yaml");
        let digest = write(&source_item, "name: x\n");
        let active_digest = write(&active_root.join(".ai/tools/x.yaml"), "name: x\n");
        let authority = SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: "a".repeat(64),
        };
        let key = ResolutionCacheKey {
            generation: "generation-1".into(),
            generation_epoch: None,
            canonical_ref: "tool:x".into(),
            subject_authority: authority.clone(),
            live_project_root: None,
            plan_context_identity: String::new(),
        };
        let lifeline = Arc::new(TempDirGuard::new(source_root.clone()));
        cache_insert_pinned_fixture(
            &source_root,
            &source_item,
            digest,
            authority,
            key.clone(),
            lifeline,
            source_probe,
            &active_root,
            active_probe,
            active_digest,
        );
    }

    fn cache_insert_pinned_fixture(
        source_root: &Path,
        source_item: &Path,
        digest: String,
        authority: SubjectResolutionAuthority,
        key: ResolutionCacheKey,
        lifeline: Arc<TempDirGuard>,
        source_probe: PathBuf,
        active_root: &Path,
        active_probe: PathBuf,
        active_digest: String,
    ) {
        let cache = ResolutionCache::new(8);
        let closure = Arc::new(
            ResolvedClosure::new(
                output_with_root(ancestor(
                    ItemSpace::Project,
                    source_item.to_path_buf(),
                    digest,
                )),
                authority,
                Some(source_root.to_path_buf()),
                Some(lifeline),
            )
            .unwrap(),
        );
        cache.insert(
            key.clone(),
            closure,
            vec![ProbedAbsence {
                space: ItemSpace::Project,
                path: source_probe,
            }],
        );
        let binding = pinned_binding(
            key.subject_authority.clone(),
            active_root,
            [(".ai/tools/x.yaml", active_digest, "name: x\n".len() as u64)],
        );
        let (hit, outcome) = cache.get_admitted(&key, &binding).unwrap();
        assert_eq!(outcome, LookupOutcome::Hit);
        assert_eq!(hit.unwrap().probed_absent()[0].path, active_probe);
    }
}
