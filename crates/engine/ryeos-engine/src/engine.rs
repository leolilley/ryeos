use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AI_DIR;
use crate::canonical_ref::CanonicalRef;
use crate::composers::ComposerRegistry;
use crate::contracts::{
    EngineContext, ExecutionCompletion, ExecutionHints, ExecutionPlan, PlanContext, ProjectContext,
    ResolvedItem, SubjectResolutionAuthority, VerifiedItem,
};
use crate::effective_validators::EffectiveValidatorRegistry;
use crate::error::EngineError;
use crate::item_resolution::{ResolutionRoot, ResolutionRoots};
use crate::kind_registry::KindRegistry;
use crate::launch_preparers::LaunchPreparerRegistry;
use crate::parsers::ParserDispatcher;
use crate::protocols::ProtocolRegistry;
use crate::runtime_registry::RuntimeRegistry;
use crate::trust::TrustStore;

/// Request for an effective, composed item value.
#[derive(Debug, Clone)]
pub struct EffectiveItemRequest {
    pub item_ref: CanonicalRef,
    pub expected_kind: Option<String>,
    pub project_root: Option<PathBuf>,
    pub subject_resolution_authority: SubjectResolutionAuthority,
}

/// Source metadata for an effective item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveItemSource {
    pub path: PathBuf,
    /// Whole-file SHA-256 of the exact root bytes used by resolution.
    pub content_hash: String,
    /// The installed bundle root (parent of `.ai/`) when the item
    /// came from an installed bundle space. `None` for project-space
    /// items, or when the resolver cannot determine
    /// the bundle boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_root: Option<PathBuf>,
}

/// Diagnostic emitted while producing an effective item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveItemDiagnostic {
    pub level: String,
    pub message: String,
}

/// Engine-owned effective item response. This is valid for executable
/// and non-executable kinds; callers decide whether to execute,
/// render, inspect, or otherwise consume the composed value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveItem {
    pub requested_ref: String,
    pub canonical_ref: String,
    pub kind: String,
    pub trusted: bool,
    pub trust_class: crate::resolution::TrustClass,
    pub root_trust_class: crate::resolution::TrustClass,
    pub source: EffectiveItemSource,
    pub provenance: crate::resolution::ResolutionProvenance,
    pub composed_value: Value,
    pub derived: std::collections::HashMap<String, Value>,
    pub policy_facts: std::collections::HashMap<String, Value>,
    pub diagnostics: Vec<EffectiveItemDiagnostic>,
}

/// Trust, parser dispatch, and the downstream cache fingerprint captured for
/// one request under one checked installed-bundle generation.
#[derive(Debug, Clone)]
pub struct EffectiveRequestSnapshot {
    pub trust_store: TrustStore,
    pub parser_dispatcher: ParserDispatcher,
    pub registry_fingerprint: String,
    /// Effective item-trust identity, including the caller-overlay identity
    /// even when that overlay adds no new signer.
    pub effective_trust_identity: String,
    /// Process-local identity of the immutable admitted engine/bundle
    /// generation backing this snapshot.
    pub request_engine_generation_identity: String,
    /// Typed content authority whose verified trust/parser values this
    /// snapshot represents. No disposable project pathname is retained.
    pub subject_resolution_authority: SubjectResolutionAuthority,
}

impl EffectiveRequestSnapshot {
    fn estimated_size_bytes(&self) -> usize {
        self.trust_store
            .estimated_size_bytes()
            .saturating_add(self.parser_dispatcher.parser_tools.estimated_size_bytes())
            .saturating_add(self.registry_fingerprint.capacity())
            .saturating_add(self.effective_trust_identity.capacity())
            .saturating_add(self.request_engine_generation_identity.capacity())
            .saturating_add(
                serde_json::to_vec(&self.subject_resolution_authority)
                    .map(|bytes| bytes.len())
                    .unwrap_or(IMMUTABLE_REQUEST_CACHE_MAX_ENTRY_BYTES.saturating_add(1)),
            )
    }
}

/// Reduced authority identity projected from one coherent request snapshot.
///
/// The complete snapshot is also path-independent: parser descriptors are
/// parsed values and dispatch through installed handler identities. This view
/// exists for downstream keys that do not need the dispatcher itself.
#[derive(Debug, Clone)]
pub struct EffectiveRequestAuthoritySnapshot {
    pub trust_store: TrustStore,
    pub registry_fingerprint: String,
    pub effective_trust_identity: String,
    pub request_engine_generation_identity: String,
    pub subject_resolution_authority: SubjectResolutionAuthority,
}

/// Proof-bearing handle for path-independent immutable request authority.
///
/// Construction is restricted to
/// [`Engine::admit_request_authority_snapshot`], which validates an
/// opaque state-issued materialization proof before consulting the
/// content-addressed cache. Callers may inspect the snapshot needed to derive
/// downstream cache keys, but cannot mint this handle from a pathname and
/// claimed digest.
#[derive(Debug, Clone)]
pub struct AdmittedRequestAuthoritySnapshot {
    request: Arc<EffectiveRequestSnapshot>,
    authority: EffectiveRequestAuthoritySnapshot,
    project_root: PathBuf,
    materialization: ryeos_state::PinnedProjectMaterialization,
}

impl AdmittedRequestAuthoritySnapshot {
    fn snapshot(&self) -> &EffectiveRequestAuthoritySnapshot {
        &self.authority
    }

    fn request_snapshot(&self) -> &EffectiveRequestSnapshot {
        self.request.as_ref()
    }

    fn request_snapshot_arc(&self) -> Arc<EffectiveRequestSnapshot> {
        Arc::clone(&self.request)
    }

    fn content_proof_generation(&self, project_root: &Path) -> Result<String, EngineError> {
        self.validate_root_binding(project_root)?;
        let generation = self
            .authority
            .subject_resolution_authority
            .operational_generation()
            .ok_or_else(|| {
                EngineError::Internal(
                    "admitted content proof has no operational generation".to_string(),
                )
            })?;
        if self.materialization.snapshot_hash() != generation {
            return Err(EngineError::Internal(
                "admitted content proof generation differs from its materialization".to_string(),
            ));
        }
        Ok(generation.to_string())
    }

    pub fn authority_snapshot_for_root(
        &self,
        project_root: &Path,
    ) -> Result<&EffectiveRequestAuthoritySnapshot, EngineError> {
        self.validate_root_binding(project_root)?;
        Ok(&self.authority)
    }

    pub fn project_content_for_root(
        &self,
        project_root: &Path,
    ) -> Result<&dyn crate::project_content::AuthoritativeProjectContent, EngineError> {
        self.validate_root_binding(project_root)?;
        Ok(&self.materialization)
    }

    pub fn validate_project_file_for_root(
        &self,
        project_root: &Path,
        source_path: &Path,
        content_hash: &str,
    ) -> Result<bool, EngineError> {
        let content = self.project_content_for_root(project_root)?;
        let relative = source_path.strip_prefix(project_root).map_err(|_| {
            EngineError::Internal(format!(
                "project dependency {} is outside admitted root {}",
                source_path.display(),
                project_root.display()
            ))
        })?;
        content.validates_file(relative, content_hash)
    }

    pub fn validate_project_absence_for_root(
        &self,
        project_root: &Path,
        source_path: &Path,
    ) -> Result<bool, EngineError> {
        let content = self.project_content_for_root(project_root)?;
        let relative = source_path.strip_prefix(project_root).map_err(|_| {
            EngineError::Internal(format!(
                "project absence {} is outside admitted root {}",
                source_path.display(),
                project_root.display()
            ))
        })?;
        content.validates_absence(relative)
    }

    fn validate_resolution_dependencies(
        &self,
        project_root: &Path,
        output: &crate::resolution::ResolutionOutput,
        probed_absent: &[crate::contracts::ProbedAbsence],
    ) -> Result<(), EngineError> {
        for dependency in std::iter::once(&output.root)
            .chain(output.ancestors.iter())
            .chain(output.referenced_items.iter())
            .filter(|dependency| dependency.source_space == crate::contracts::ItemSpace::Project)
        {
            if !self.validate_project_file_for_root(
                project_root,
                &dependency.source_path,
                &dependency.source_content_digest,
            )? {
                return Err(EngineError::Internal(format!(
                    "project dependency {} differs from admitted content authority",
                    dependency.source_path.display()
                )));
            }
        }
        for absence in probed_absent
            .iter()
            .filter(|absence| absence.space == crate::contracts::ItemSpace::Project)
        {
            if !self.validate_project_absence_for_root(project_root, &absence.path)? {
                return Err(EngineError::Internal(format!(
                    "project absence {} differs from admitted content authority",
                    absence.path.display()
                )));
            }
        }
        Ok(())
    }

    fn validate_root_binding(&self, project_root: &Path) -> Result<(), EngineError> {
        if self.project_root != project_root
            || !self
                .materialization
                .owns_path(project_root)
                .map_err(|error| EngineError::Internal(error.to_string()))?
        {
            return Err(EngineError::Internal(
                "admitted request authority was paired with a different materialization"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

fn authority_snapshot_from_request(
    request: &EffectiveRequestSnapshot,
) -> EffectiveRequestAuthoritySnapshot {
    EffectiveRequestAuthoritySnapshot {
        trust_store: request.trust_store.clone(),
        registry_fingerprint: request.registry_fingerprint.clone(),
        effective_trust_identity: request.effective_trust_identity.clone(),
        request_engine_generation_identity: request.request_engine_generation_identity.clone(),
        subject_resolution_authority: request.subject_resolution_authority.clone(),
    }
}

fn project_root_from_context(context: &PlanContext) -> Option<&Path> {
    match &context.project_context {
        ProjectContext::LocalPath { path } => Some(path.as_path()),
        ProjectContext::None
        | ProjectContext::ProjectRef { .. }
        | ProjectContext::SnapshotHash { .. } => None,
    }
}

fn resolution_error_to_engine(
    error: crate::resolution::ResolutionError,
    requested_root: &CanonicalRef,
) -> EngineError {
    use crate::resolution::ResolutionError;

    match error {
        ResolutionError::IntegrityFailure { item_ref, reason } => {
            EngineError::EffectiveItemUntrusted {
                canonical_ref: item_ref,
                fingerprint: reason,
            }
        }
        ResolutionError::MissingItem { item_ref, .. } => EngineError::EffectiveItemNotFound {
            canonical_ref: item_ref,
        },
        ResolutionError::ComposedValueContractViolation {
            item_ref, report, ..
        } => EngineError::ComposedValueContractViolation {
            canonical_ref: item_ref,
            report,
        },
        other => EngineError::EffectiveItemCompositionFailed {
            canonical_ref: requested_root.to_string(),
            reason: other.to_string(),
        },
    }
}

const STATIC_VERIFICATION_CACHE_CAPACITY: usize = 512;
const STATIC_VERIFICATION_CACHE_MAX_PENDING: usize = 512;
const STATIC_VERIFICATION_CACHE_IDLE_TTL: Duration = Duration::from_secs(10 * 60);
const ENGINE_RESOLVED_SUBJECT_PROOF_CAPACITY: usize = 2048;
const ENGINE_RESOLVED_SUBJECT_PROOF_IDLE_TTL: Duration = Duration::from_secs(10 * 60);
const IMMUTABLE_REQUEST_CACHE_CAPACITY: usize = 256;
const IMMUTABLE_REQUEST_CACHE_MAX_PENDING: usize = 256;
const IMMUTABLE_REQUEST_CACHE_IDLE_TTL: Duration = Duration::from_secs(10 * 60);
const IMMUTABLE_REQUEST_CACHE_MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const IMMUTABLE_REQUEST_CACHE_MAX_TOTAL_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Default)]
struct ImmutableRequestSnapshotCache {
    slots: HashMap<String, ImmutableRequestSnapshotCacheEntry>,
    lru: VecDeque<String>,
    pending: HashMap<String, Arc<ImmutableRequestSnapshotPending>>,
    total_bytes: usize,
}

#[derive(Debug)]
struct ImmutableRequestSnapshotCacheEntry {
    request: Arc<EffectiveRequestSnapshot>,
    estimated_bytes: usize,
    last_touched: Instant,
}

#[derive(Debug, Default)]
struct ImmutableRequestSnapshotPending {
    result: Mutex<Option<Result<Arc<EffectiveRequestSnapshot>, Arc<EngineError>>>>,
    ready: Condvar,
}

struct ImmutableRequestSnapshotFillGuard {
    cache: Arc<Mutex<ImmutableRequestSnapshotCache>>,
    key: String,
    pending: Arc<ImmutableRequestSnapshotPending>,
    completed: bool,
}

impl ImmutableRequestSnapshotFillGuard {
    fn finish(mut self, request: Arc<EffectiveRequestSnapshot>) {
        complete_immutable_request_pending(&self.cache, &self.key, &self.pending, Ok(request));
        self.completed = true;
    }

    fn fail(mut self, error: EngineError) -> Arc<EngineError> {
        let error = Arc::new(error);
        complete_immutable_request_pending(
            &self.cache,
            &self.key,
            &self.pending,
            Err(error.clone()),
        );
        self.completed = true;
        error
    }
}

impl Drop for ImmutableRequestSnapshotFillGuard {
    fn drop(&mut self) {
        if !self.completed {
            complete_immutable_request_pending(
                &self.cache,
                &self.key,
                &self.pending,
                Err(Arc::new(EngineError::Internal(
                    "immutable request cache fill ended without publishing its result".to_owned(),
                ))),
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ImmutableRequestCacheOutcome {
    Hit,
    Miss,
    Bypass,
    Eviction,
}

impl ImmutableRequestCacheOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Bypass => "bypass",
            Self::Eviction => "eviction",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ImmutableRequestCacheReason {
    Ready,
    SingleFlight,
    Cold,
    PendingCapacity,
    Capacity,
    IdleTtl,
}

impl ImmutableRequestCacheReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::SingleFlight => "single_flight",
            Self::Cold => "cold",
            Self::PendingCapacity => "pending_capacity",
            Self::Capacity => "capacity",
            Self::IdleTtl => "idle_ttl",
        }
    }
}

fn emit_immutable_request_cache_metric(
    outcome: ImmutableRequestCacheOutcome,
    reason: ImmutableRequestCacheReason,
) {
    ryeos_tracing::record_cache_metric(ryeos_tracing::CacheMetricSample {
        metric: "immutable_request_snapshot_cache",
        namespace: None,
        outcome: outcome.as_str(),
        reason: Some(reason.as_str()),
        source_bytes: 0,
        entry_bytes: 0,
        wait_microseconds: 0,
    });
    tracing::debug!(
        target: "ryeos.metrics",
        metric = "immutable_request_snapshot_cache",
        outcome = outcome.as_str(),
        reason = reason.as_str(),
        "immutable request snapshot cache metric"
    );
}

fn complete_immutable_request_pending(
    cache: &Arc<Mutex<ImmutableRequestSnapshotCache>>,
    key: &str,
    pending: &Arc<ImmutableRequestSnapshotPending>,
    result: Result<Arc<EffectiveRequestSnapshot>, Arc<EngineError>>,
) {
    let mut state = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *pending
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
    if state
        .pending
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, pending))
    {
        state.pending.remove(key);
    }
    drop(state);
    pending.ready.notify_all();
}

fn sweep_immutable_request_cache(state: &mut ImmutableRequestSnapshotCache) {
    let now = Instant::now();
    let stale = state
        .slots
        .iter()
        .filter(|(_, entry)| {
            now.saturating_duration_since(entry.last_touched) >= IMMUTABLE_REQUEST_CACHE_IDLE_TTL
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in stale {
        remove_immutable_request_entry(state, &key);
        emit_immutable_request_cache_metric(
            ImmutableRequestCacheOutcome::Eviction,
            ImmutableRequestCacheReason::IdleTtl,
        );
    }
}

fn remove_immutable_request_entry(state: &mut ImmutableRequestSnapshotCache, key: &str) -> usize {
    let Some(entry) = state.slots.remove(key) else {
        return 0;
    };
    state.total_bytes = state.total_bytes.saturating_sub(entry.estimated_bytes);
    if let Some(position) = state.lru.iter().position(|candidate| candidate == key) {
        state.lru.remove(position);
    }
    entry.estimated_bytes
}

fn touch_immutable_request_lru(state: &mut ImmutableRequestSnapshotCache, key: &str) {
    if let Some(position) = state.lru.iter().position(|candidate| candidate == key) {
        state.lru.remove(position);
    }
    state.lru.push_back(key.to_owned());
}

#[derive(Default)]
struct StaticVerificationCache {
    slots: HashMap<String, StaticVerificationCacheEntry>,
    lru: VecDeque<String>,
    pending: HashMap<String, Arc<StaticVerificationPending>>,
}

struct StaticVerificationCacheEntry {
    evidence: Arc<StaticVerificationEvidence>,
    last_touched: Instant,
}

#[derive(Default)]
struct StaticVerificationPending {
    result: Mutex<Option<Result<Arc<StaticVerificationEvidence>, Arc<EngineError>>>>,
    ready: Condvar,
}

struct StaticVerificationFillGuard {
    key: String,
    pending: Arc<StaticVerificationPending>,
    completed: bool,
}

impl StaticVerificationFillGuard {
    fn finish(mut self, evidence: Arc<StaticVerificationEvidence>) {
        complete_static_verification_pending(&self.key, &self.pending, Ok(evidence));
        self.completed = true;
    }

    fn fail(mut self, error: EngineError) -> Arc<EngineError> {
        let error = Arc::new(error);
        complete_static_verification_pending(&self.key, &self.pending, Err(error.clone()));
        self.completed = true;
        error
    }
}

impl Drop for StaticVerificationFillGuard {
    fn drop(&mut self) {
        if !self.completed {
            complete_static_verification_pending(
                &self.key,
                &self.pending,
                Err(Arc::new(EngineError::Internal(
                    "static verification cache fill ended without publishing its result".to_owned(),
                ))),
            );
        }
    }
}

#[derive(Debug, Clone)]
struct StaticVerificationEvidence {
    signer: Option<crate::contracts::SignerFingerprint>,
    trust_class: crate::contracts::TrustClass,
    pinned_version: Option<crate::contracts::PinnedVersion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticVerificationCacheOutcome {
    Hit,
    Miss,
    Bypass,
}

impl StaticVerificationCacheOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Bypass => "bypass",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticVerificationCacheReason {
    Ready,
    SingleFlight,
    PendingCapacity,
    Cold,
}

impl StaticVerificationCacheReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::SingleFlight => "single_flight",
            Self::PendingCapacity => "pending_capacity",
            Self::Cold => "cold",
        }
    }
}

fn emit_static_verification_cache_metric(
    outcome: StaticVerificationCacheOutcome,
    reason: StaticVerificationCacheReason,
) {
    ryeos_tracing::record_cache_metric(ryeos_tracing::CacheMetricSample {
        metric: "static_verification_cache",
        namespace: None,
        outcome: outcome.as_str(),
        reason: Some(reason.as_str()),
        source_bytes: 0,
        entry_bytes: 0,
        wait_microseconds: 0,
    });
    tracing::debug!(
        target: "ryeos.metrics",
        metric = "static_verification_cache",
        outcome = outcome.as_str(),
        reason = reason.as_str(),
        "static verification cache metric"
    );
}

fn complete_static_verification_pending(
    key: &str,
    pending: &Arc<StaticVerificationPending>,
    result: Result<Arc<StaticVerificationEvidence>, Arc<EngineError>>,
) {
    let mut cache = static_verification_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *pending
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
    if cache
        .pending
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, pending))
    {
        cache.pending.remove(key);
    }
    drop(cache);
    pending.ready.notify_all();
}

fn sweep_static_verification_cache(cache: &mut StaticVerificationCache) {
    let now = Instant::now();
    let stale = cache
        .slots
        .iter()
        .filter(|(_, entry)| {
            now.duration_since(entry.last_touched) >= STATIC_VERIFICATION_CACHE_IDLE_TTL
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in stale {
        cache.slots.remove(&key);
        if let Some(position) = cache.lru.iter().position(|candidate| candidate == &key) {
            cache.lru.remove(position);
        }
    }
}

fn touch_static_verification_lru(cache: &mut StaticVerificationCache, key: &str) {
    if let Some(position) = cache.lru.iter().position(|candidate| candidate == key) {
        cache.lru.remove(position);
    }
    cache.lru.push_back(key.to_owned());
}

fn static_verification_cache() -> &'static Mutex<StaticVerificationCache> {
    static CACHE: OnceLock<Mutex<StaticVerificationCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(StaticVerificationCache::default()))
}

#[derive(Default)]
struct EngineResolvedSubjectProofs {
    slots: HashMap<String, Instant>,
    lru: VecDeque<String>,
}

fn engine_resolved_subject_proofs() -> &'static Mutex<EngineResolvedSubjectProofs> {
    static PROOFS: OnceLock<Mutex<EngineResolvedSubjectProofs>> = OnceLock::new();
    PROOFS.get_or_init(|| Mutex::new(EngineResolvedSubjectProofs::default()))
}

fn read_current_subject_source(item: &ResolvedItem) -> Result<String, EngineError> {
    crate::item_resolution::read_item_source_no_follow(&item.source_path)
}

fn relocate_verified_subject_for_current_root(
    verified: &mut VerifiedItem,
    current_project_root: Option<&Path>,
    authority: &SubjectResolutionAuthority,
) -> Result<(), EngineError> {
    if !matches!(
        authority,
        SubjectResolutionAuthority::PinnedGeneration { .. }
            | SubjectResolutionAuthority::CowWorkspace { .. }
    ) {
        return Ok(());
    }
    let Some(current_root) = current_project_root else {
        return Ok(());
    };
    let Some(admitted_root) = verified.resolved.materialized_project_root.as_deref() else {
        return Err(EngineError::Internal(
            "pinned verified subject has no admitted materialized root".to_string(),
        ));
    };
    if admitted_root == current_root {
        return Ok(());
    }
    let relocate = |path: &Path| -> Result<PathBuf, EngineError> {
        let relative = path.strip_prefix(admitted_root).map_err(|_| {
            EngineError::Internal(format!(
                "pinned verified project path {} is outside admitted root {}",
                path.display(),
                admitted_root.display()
            ))
        })?;
        Ok(current_root.join(relative))
    };
    if verified.resolved.source_space == crate::contracts::ItemSpace::Project {
        verified.resolved.source_path = relocate(&verified.resolved.source_path)?;
    }
    for candidate in &mut verified.resolved.shadowed {
        if candidate.space == crate::contracts::ItemSpace::Project {
            candidate.path = relocate(&candidate.path)?;
        }
    }
    for absence in &mut verified.resolved.probed_absent {
        if absence.space == crate::contracts::ItemSpace::Project {
            absence.path = relocate(&absence.path)?;
        }
    }
    verified.resolved.materialized_project_root = Some(current_root.to_path_buf());
    Ok(())
}

/// Opaque engine-produced proof carrying one exact verified subject.
///
/// This is static verification evidence, not execution authority. Its fields
/// are private and it is not serializable; only `Engine` can construct or
/// consume it.
pub struct VerifiedArtifactAttestation {
    verified_subject: Arc<VerifiedItem>,
    source_bytes: Arc<[u8]>,
    subject_digest: String,
    engine_generation_identity: String,
    trust_identity: String,
    request_registry_fingerprint: String,
    subject_resolution_authority: SubjectResolutionAuthority,
    /// Present only when this subject was selected and read through an opaque
    /// admitted project-content authority. Pathname-minted evidence cannot be
    /// upgraded into this proof.
    admitted_content_generation: Option<String>,
    resolution_closure_digest: Option<String>,
}

/// Opaque engine-produced resolution closure. Only the engine's canonical
/// resolution pipeline can construct this value.
pub struct VerifiedResolutionClosure {
    output: crate::resolution::ResolutionOutput,
    probed_absent: Vec<crate::contracts::ProbedAbsence>,
    attestation: VerifiedArtifactAttestation,
}

impl VerifiedResolutionClosure {
    pub fn into_parts(
        self,
    ) -> (
        crate::resolution::ResolutionOutput,
        Vec<crate::contracts::ProbedAbsence>,
        VerifiedArtifactAttestation,
    ) {
        (self.output, self.probed_absent, self.attestation)
    }
}

impl std::fmt::Debug for VerifiedArtifactAttestation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedArtifactAttestation")
            .field(
                "canonical_ref",
                &self.verified_subject.resolved.canonical_ref,
            )
            .field(
                "subject_resolution_authority",
                &self.subject_resolution_authority,
            )
            .finish_non_exhaustive()
    }
}

impl VerifiedArtifactAttestation {
    /// Exact static evidence carried by this engine-produced attestation.
    ///
    /// This remains evidence rather than execution authority: admission must
    /// still call [`Engine::consume_verified_attestation`] under the current
    /// engine, trust, and typed subject authority.
    pub fn verified_subject(&self) -> &VerifiedItem {
        self.verified_subject.as_ref()
    }

    /// Whole admitted source bytes read by the engine at verification time.
    /// Keeping these bytes lets immutable cache hits seal the exact source
    /// without reopening a disposable materialization path.
    pub fn source_bytes(&self) -> &[u8] {
        self.source_bytes.as_ref()
    }
}

/// Concrete native engine.
///
/// Holds the kind registry and metadata parser registry. Exposes the
/// four pipeline methods directly — no trait boundary, no dyn dispatch
/// at the seam. The seam is the data contracts.
#[derive(Debug, Clone)]
pub struct Engine {
    pub kinds: KindRegistry,
    pub parser_dispatcher: ParserDispatcher,
    /// Combined item trust for the current project/request.
    pub trust_store: TrustStore,
    /// Persistent node trust used exclusively for installed bundle
    /// schemas, handlers, protocols, and native executable manifests. Project
    /// keys and caller-scoped overlays never enter this store.
    pub node_trust_store: TrustStore,
    /// Per-kind composer registry — owned by the engine so boot
    /// validation and the daemon-side resolution pipeline see the
    /// **same** instance (no split-brain between launcher and
    /// runtime construction sites).
    pub composers: ComposerRegistry,

    /// Boot-bound semantic validators for complete effective programs.
    pub effective_validators: EffectiveValidatorRegistry,

    /// Catalog of verified `kind: runtime` items, scanned at engine
    /// init via `RuntimeRegistry::build_from_bundles`. Empty by
    /// default so test sites that construct an engine directly without
    /// a runtimes scan still compile.
    pub runtimes: RuntimeRegistry,

    /// Boot-bound runtime→launch-preparer registry. Handler preparation is
    /// always resolved through this verified binding rather than looking up a
    /// handler dynamically at launch time.
    pub launch_preparers: LaunchPreparerRegistry,

    /// Protocol registry — loaded from base roots at engine init.
    /// Protocol descriptors declare wire contracts for subprocess
    /// terminators. Empty by default for test compatibility.
    pub protocols: ProtocolRegistry,

    /// Operator-supplied allowlist + snapshot for host-env passthrough
    /// (`${VAR}` in tool env values). Populated once at daemon bootstrap
    /// from `RYEOS_TOOL_ENV_PASSTHROUGH`. Empty by default for test
    /// compatibility.
    pub host_env: crate::runtime::HostEnvBindings,

    /// System bundle roots (parents of `AI_DIR`)
    pub bundle_roots: Vec<PathBuf>,

    /// Immutable signed-registration identities corresponding one-to-one with
    /// `bundle_roots`. Production engines populate this from the retained node
    /// generation; directory basenames are never treated as bundle identity.
    registered_bundle_roots: Vec<crate::item_resolution::RegisteredBundleRoot>,

    /// Operator-owned `.ai/` root. This is intentionally excluded from
    /// ordinary item resolution and is admitted only for signed launch-config
    /// inputs, between an active project and installed bundles.
    node_config_root: Option<PathBuf>,

    /// Generation guard shared with launch preparation. It is inert for
    /// directly-constructed test engines and active for node engines.
    isolation_generation: std::sync::Arc<crate::isolation::IsolationRuntime>,

    /// Base item trust for a project-scoped engine, excluding the project's
    /// mutable trust directory. This lets every request re-read project trust
    /// and observe both additions and removals.
    request_trust_base: Option<TrustStore>,

    /// Distinguishes caller-supplied trust authority even when every supplied
    /// signer was already present in the persistent trust base.
    request_trust_overlay_identity: Option<String>,

    /// Shared only by clones of this admitted engine generation. Cache keys
    /// also include the effective generation/trust identities.
    parser_overlay_cache: std::sync::Arc<crate::parser_overlay_cache::ParserOverlayCache>,
    /// Immutable pinned-generation request state. Trust and parser registries
    /// retain verified values and installed handler identities, not disposable
    /// project paths, so the complete snapshot is content-addressed.
    immutable_request_snapshot_cache: Arc<Mutex<ImmutableRequestSnapshotCache>>,
}

/// Read-only engine view bound to one verified installed-bundle generation.
///
/// A caller resolving a coherent batch (for example a surface and all of its
/// views) uses this view so the generation is locked and verified once around
/// the whole batch instead of once per item.
pub struct CheckedEngineGeneration<'a> {
    engine: &'a Engine,
}

fn parallel_map_ordered<T, U>(items: &[T], operation: impl Fn(&T) -> U + Sync) -> Vec<U>
where
    T: Sync,
    U: Send,
{
    if items.len() <= 1 {
        return items.iter().map(operation).collect();
    }
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(8)
        .min(items.len());
    let chunk_size = items.len().div_ceil(worker_count);
    std::thread::scope(|scope| {
        let operation = &operation;
        items
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || chunk.iter().map(operation).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

impl CheckedEngineGeneration<'_> {
    pub fn resolve(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
    ) -> Result<ResolvedItem, EngineError> {
        self.engine.resolve_current(ctx, item_ref)
    }

    pub fn effective_item(
        &self,
        request: EffectiveItemRequest,
    ) -> Result<EffectiveItem, EngineError> {
        self.engine.effective_item_current(request)
    }

    pub fn verify(
        &self,
        ctx: &PlanContext,
        item: ResolvedItem,
    ) -> Result<VerifiedItem, EngineError> {
        self.engine.verify(ctx, item)
    }

    pub fn build_plan(
        &self,
        ctx: &PlanContext,
        item: &VerifiedItem,
        parameters: &Value,
        hints: &ExecutionHints,
        sealed_content: Option<&dyn crate::project_content::SealedDependencyBytes>,
    ) -> Result<ExecutionPlan, EngineError> {
        self.engine
            .build_plan_current(ctx, item, parameters, hints, sealed_content)
    }

    /// Resolve independent canonical items concurrently while retaining this
    /// generation and preserving input order.
    pub fn resolve_many(
        &self,
        ctx: &PlanContext,
        item_refs: &[CanonicalRef],
    ) -> Vec<Result<ResolvedItem, EngineError>> {
        parallel_map_ordered(item_refs, |item_ref| {
            self.engine.resolve_current(ctx, item_ref)
        })
    }

    /// Compose independent effective items concurrently while retaining this
    /// generation and preserving input order.
    pub fn effective_items(
        &self,
        requests: &[EffectiveItemRequest],
    ) -> Vec<Result<EffectiveItem, EngineError>> {
        parallel_map_ordered(requests, |request| {
            self.engine.effective_item_current(request.clone())
        })
    }
}

impl Engine {
    pub fn new(
        kinds: KindRegistry,
        parser_dispatcher: ParserDispatcher,
        bundle_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            kinds,
            parser_dispatcher,
            trust_store: TrustStore::empty(),
            node_trust_store: TrustStore::empty(),
            composers: ComposerRegistry::new(),
            effective_validators: EffectiveValidatorRegistry::default(),
            runtimes: RuntimeRegistry::default(),
            launch_preparers: LaunchPreparerRegistry::default(),
            protocols: ProtocolRegistry::empty(),
            host_env: crate::runtime::HostEnvBindings::default(),
            bundle_roots,
            registered_bundle_roots: Vec::new(),
            node_config_root: None,
            isolation_generation: std::sync::Arc::new(
                crate::isolation::IsolationRuntime::disabled_for_authoring(),
            ),
            request_trust_base: None,
            request_trust_overlay_identity: None,
            parser_overlay_cache: std::sync::Arc::new(
                crate::parser_overlay_cache::ParserOverlayCache::default(),
            ),
            immutable_request_snapshot_cache: Arc::new(Mutex::new(
                ImmutableRequestSnapshotCache::default(),
            )),
        }
    }

    pub fn with_isolation_generation(
        mut self,
        isolation: std::sync::Arc<crate::isolation::IsolationRuntime>,
    ) -> Self {
        self.isolation_generation = isolation;
        self
    }

    pub fn with_registered_bundle_roots(
        mut self,
        registered: Vec<crate::item_resolution::RegisteredBundleRoot>,
    ) -> Self {
        self.registered_bundle_roots = registered;
        self
    }

    /// Return the exact retained content root for a named bundle in this
    /// engine generation. Callers must obtain `name` from typed resolution
    /// provenance; a matching source pathname is not authority.
    pub fn registered_bundle_root(&self, name: &str) -> Option<&std::path::Path> {
        self.registered_bundle_roots
            .iter()
            .find(|bundle| bundle.name == name)
            .map(|bundle| bundle.canonical_root.as_path())
    }

    /// Stable identity for this engine's complete admitted installed-bundle
    /// generation. Executor verification caches bind to the whole generation,
    /// rather than only the root that happened to publish a matching binary,
    /// so an all-roots ambiguity check can never be bypassed by a cache hit.
    pub fn registered_bundle_generation_fingerprint(&self) -> String {
        match self.isolation_generation.registered_generation_identity() {
            Some(identity) => lillux::cas::sha256_hex(
                format!("ryeos:admitted-process-generation:v1:{identity}").as_bytes(),
            ),
            // Directly-constructed fixture engines have no retained daemon
            // generation. Their cache namespace remains isolated by their
            // registry/handler/root identity and all signed manifest refs.
            None => self.request_engine_generation_identity(),
        }
    }

    /// Monotonic daemon generation used only for cache retirement ordering.
    /// Authority remains the full fingerprint above; fixture engines have no
    /// daemon epoch and therefore rely on normal bounded eviction.
    pub fn registered_bundle_generation_epoch(&self) -> Option<u64> {
        self.isolation_generation.registered_generation_identity()
    }

    /// Assert the daemon-only executor-cache binding invariant without
    /// widening normal engine authority APIs. Fixture/standalone engines have
    /// no registry-owned root identities and use the per-ref probe namespace.
    pub fn debug_assert_executor_cache_generation_identity(&self) {
        debug_assert!(
            self.registered_bundle_roots.is_empty()
                || self
                    .isolation_generation
                    .registered_generation_identity()
                    .is_some(),
            "registry-owned daemon bundle roots must carry their retained generation identity"
        );
    }

    pub fn with_node_config_root(mut self, node_config_root: PathBuf) -> Self {
        self.node_config_root = Some(node_config_root);
        self
    }

    /// Typed node-local config root. Consumers must use this explicit
    /// authority and never infer node precedence from a display label.
    pub fn node_config_root(&self) -> Option<PathBuf> {
        self.node_config_root.clone()
    }

    fn checked_bundle_generation<T>(
        &self,
        operation: impl FnOnce() -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        self.with_checked_bundle_generation(|_| operation())
    }

    /// Run a coherent read batch against one verified installed-bundle
    /// generation. Supported bundle mutations are excluded for the duration;
    /// concurrent read batches remain independent.
    pub fn with_checked_bundle_generation<T, E>(
        &self,
        operation: impl FnOnce(&CheckedEngineGeneration<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<EngineError>,
    {
        let _operation_guard = self
            .isolation_generation
            .begin_registered_generation_operation()
            .map_err(E::from)?;
        self.isolation_generation
            .ensure_registered_generation_current()
            .map_err(E::from)?;
        let generation = CheckedEngineGeneration { engine: self };
        let value = operation(&generation)?;
        self.isolation_generation
            .ensure_registered_generation_current()
            .map_err(E::from)?;
        Ok(value)
    }

    pub fn with_trust_store(mut self, trust_store: TrustStore) -> Self {
        self.trust_store = trust_store;
        self.request_trust_base = None;
        self.request_trust_overlay_identity = None;
        self
    }

    pub fn with_node_trust_store(mut self, trust_store: TrustStore) -> Self {
        self.node_trust_store = trust_store;
        self
    }

    /// Derive a project-scoped engine from this already-admitted node
    /// generation.
    ///
    /// Installed schemas, handlers, runtimes, protocols, host bindings, and
    /// the isolation generation are immutable for a daemon generation and are
    /// cloned from the admitted engine. Only item trust is rebuilt from the
    /// pinned project root plus an optional caller-scoped overlay. This avoids
    /// re-admitting node executors while serving a request and guarantees the
    /// project engine cannot observe a different installed-bundle generation.
    pub fn for_project_root(
        &self,
        project_root: &Path,
        trust_overlay: Option<&TrustStore>,
    ) -> Result<Self, EngineError> {
        let mut request_trust_base = self.node_trust_store.clone();
        if let Some(overlay) = trust_overlay {
            request_trust_base.extend_from(overlay);
        }
        let trust_store = request_trust_base
            .with_project_keys(project_root)?
            .into_owned();
        let mut engine = self.clone();
        engine.trust_store = trust_store;
        engine.request_trust_base = Some(request_trust_base);
        engine.request_trust_overlay_identity = trust_overlay.map(TrustStore::fingerprint);
        Ok(engine)
    }

    fn effective_trust_store(
        &self,
        project_root: Option<&Path>,
    ) -> Result<Cow<'_, TrustStore>, EngineError> {
        match project_root {
            Some(root) => {
                let base = self
                    .request_trust_base
                    .as_ref()
                    .unwrap_or(&self.trust_store);
                Ok(base.with_project_keys(root)?)
            }
            None => Ok(Cow::Borrowed(&self.trust_store)),
        }
    }

    /// Reconstruct only the current trust-policy view for an already-admitted
    /// execution.
    ///
    /// Recovery consumes sealed parser/kind/runtime semantics and must not
    /// rebuild a complete request snapshot from today's registries. This
    /// method therefore reloads only operator/project signer authority. For a
    /// content-addressed project, project keys come from the exact admitted
    /// materialization rather than its disposable pathname.
    pub fn effective_trust_store_for_current_policy(
        &self,
        project_root: Option<&Path>,
        subject_resolution_authority: &SubjectResolutionAuthority,
        materialization: Option<&ryeos_state::PinnedProjectMaterialization>,
    ) -> Result<TrustStore, EngineError> {
        self.checked_bundle_generation(|| {
            subject_resolution_authority
                .validate_for_materialized_root(project_root)
                .map_err(|error| EngineError::Internal(error.to_string()))?;
            match subject_resolution_authority.operational_generation() {
                Some(generation) => {
                    let root = project_root.ok_or_else(|| {
                        EngineError::Internal(
                            "content-addressed trust policy has no project root".to_string(),
                        )
                    })?;
                    let materialization = materialization.ok_or_else(|| {
                        EngineError::Internal(
                            "content-addressed trust policy has no admitted materialization"
                                .to_string(),
                        )
                    })?;
                    if materialization.snapshot_hash() != generation
                        || !materialization
                            .owns_path(root)
                            .map_err(|error| EngineError::Internal(error.to_string()))?
                    {
                        return Err(EngineError::Internal(
                            "content-addressed trust policy materialization contradicts its authority"
                                .to_string(),
                        ));
                    }
                    let trust_base = self
                        .request_trust_base
                        .as_ref()
                        .unwrap_or(&self.trust_store);
                    trust_base.with_project_keys_from_content(materialization)
                }
                None => {
                    if materialization.is_some() {
                        return Err(EngineError::Internal(
                            "non-content-addressed trust policy received a pinned materialization"
                                .to_string(),
                        ));
                    }
                    self.effective_trust_store(project_root)
                        .map(Cow::into_owned)
                }
            }
        })
    }

    /// Capture the trust store, parser dispatcher, and downstream registry
    /// fingerprint from one coherent request snapshot.
    ///
    /// Project trust is always re-read before the parser overlay cache is
    /// consulted. The entire operation runs inside the installed-bundle
    /// generation guard.
    pub fn effective_request_snapshot(
        &self,
        project_root: Option<&Path>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<EffectiveRequestSnapshot, EngineError> {
        self.checked_bundle_generation(|| {
            self.effective_request_snapshot_current(project_root, subject_resolution_authority)
        })
    }

    /// Reuse the complete immutable request snapshot admitted through the
    /// opaque state proof. Parser descriptors are parsed values and dispatch
    /// through the installed handler registry; they retain no disposable
    /// project pathname.
    pub fn effective_request_snapshot_under_admitted_authority(
        &self,
        project_root: &Path,
        admitted: &AdmittedRequestAuthoritySnapshot,
    ) -> Result<Arc<EffectiveRequestSnapshot>, EngineError> {
        self.checked_bundle_generation(|| {
            admitted.validate_root_binding(project_root)?;
            let authority = admitted.snapshot();
            authority
                .subject_resolution_authority
                .validate_for_materialized_root(Some(project_root))
                .map_err(|error| EngineError::Internal(error.to_string()))?;
            if authority.request_engine_generation_identity
                != self.request_engine_generation_identity()
            {
                return Err(EngineError::Internal(
                    "admitted request authority belongs to a retired engine generation".to_string(),
                ));
            }
            if authority
                .subject_resolution_authority
                .operational_generation()
                .is_none()
            {
                return Err(EngineError::Internal(
                    "admitted request snapshot has no content-addressed operational generation"
                        .to_string(),
                ));
            }
            Ok(admitted.request_snapshot_arc())
        })
    }

    fn immutable_request_cache_key(
        &self,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<String, EngineError> {
        let base = self
            .request_trust_base
            .as_ref()
            .unwrap_or(&self.trust_store)
            .fingerprint();
        let authority = serde_json::to_string(subject_resolution_authority).map_err(|error| {
            EngineError::Internal(format!(
                "serialize admitted request subject authority: {error}"
            ))
        })?;
        Ok([
            self.request_engine_generation_identity(),
            authority,
            base,
            self.request_trust_overlay_identity
                .clone()
                .unwrap_or_else(|| "-".to_string()),
        ]
        .join("\u{1f}"))
    }

    /// Admit one immutable request snapshot into the warm authority cache.
    /// The opaque state proof is the only insertion path; a caller holding only
    /// a pathname and claimed snapshot hash can perform a cold verification but
    /// cannot poison the content-addressed fast path.
    pub fn admit_request_authority_snapshot(
        &self,
        project_root: &Path,
        subject_resolution_authority: &SubjectResolutionAuthority,
        materialization: &ryeos_state::PinnedProjectMaterialization,
    ) -> Result<AdmittedRequestAuthoritySnapshot, EngineError> {
        let Some(operational_generation) = subject_resolution_authority.operational_generation()
        else {
            return Err(EngineError::Internal(
                "request authority admission requires a content-addressed operational generation"
                    .to_string(),
            ));
        };
        if materialization.snapshot_hash() != operational_generation
            || !materialization
                .owns_path(project_root)
                .map_err(|error| EngineError::Internal(error.to_string()))?
        {
            return Err(EngineError::Internal(
                "pinned request authority proof contradicts its snapshot or project root"
                    .to_string(),
            ));
        }
        self.checked_bundle_generation(|| {
            let key = self.immutable_request_cache_key(subject_resolution_authority)?;
            let pending_owner = 'pending_owner: {
                let lookup = {
                    let mut cache = self
                        .immutable_request_snapshot_cache
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    sweep_immutable_request_cache(&mut cache);
                    if let Some(request) = cache.slots.get_mut(&key).map(|entry| {
                        entry.last_touched = Instant::now();
                        entry.request.clone()
                    }) {
                        touch_immutable_request_lru(&mut cache, &key);
                        Some(Ok(request))
                    } else if let Some(pending) = cache.pending.get(&key) {
                        Some(Err(pending.clone()))
                    } else if cache.pending.len() >= IMMUTABLE_REQUEST_CACHE_MAX_PENDING {
                        None
                    } else {
                        let pending = Arc::new(ImmutableRequestSnapshotPending::default());
                        cache.pending.insert(key.clone(), pending.clone());
                        break 'pending_owner Some(pending);
                    }
                };
                match lookup {
                    Some(Ok(request)) => {
                        emit_immutable_request_cache_metric(
                            ImmutableRequestCacheOutcome::Hit,
                            ImmutableRequestCacheReason::Ready,
                        );
                        let authority = authority_snapshot_from_request(request.as_ref());
                        return Ok(AdmittedRequestAuthoritySnapshot {
                            request,
                            authority,
                            project_root: project_root.to_path_buf(),
                            materialization: materialization.clone(),
                        });
                    }
                    Some(Err(pending)) => {
                        let mut completed = pending
                            .result
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        while completed.is_none() {
                            completed = pending
                                .ready
                                .wait(completed)
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                        }
                        match completed
                            .as_ref()
                            .expect("completed immutable request fill has an outcome")
                        {
                            Ok(request) => {
                                let request = request.clone();
                                emit_immutable_request_cache_metric(
                                    ImmutableRequestCacheOutcome::Hit,
                                    ImmutableRequestCacheReason::SingleFlight,
                                );
                                let authority = authority_snapshot_from_request(request.as_ref());
                                return Ok(AdmittedRequestAuthoritySnapshot {
                                    request,
                                    authority,
                                    project_root: project_root.to_path_buf(),
                                    materialization: materialization.clone(),
                                });
                            }
                            Err(error) => return Err(EngineError::Shared(error.clone())),
                        }
                    }
                    None => None,
                }
            };
            if pending_owner.is_none() {
                emit_immutable_request_cache_metric(
                    ImmutableRequestCacheOutcome::Bypass,
                    ImmutableRequestCacheReason::PendingCapacity,
                );
                let request = Arc::new(self.effective_request_snapshot_from_materialization(
                    materialization,
                    subject_resolution_authority,
                )?);
                let authority = authority_snapshot_from_request(request.as_ref());
                return Ok(AdmittedRequestAuthoritySnapshot {
                    request,
                    authority,
                    project_root: project_root.to_path_buf(),
                    materialization: materialization.clone(),
                });
            }
            let pending_owner = pending_owner.expect("checked above");
            let fill_guard = ImmutableRequestSnapshotFillGuard {
                cache: Arc::clone(&self.immutable_request_snapshot_cache),
                key: key.clone(),
                pending: pending_owner,
                completed: false,
            };
            let request = match self.effective_request_snapshot_from_materialization(
                materialization,
                subject_resolution_authority,
            ) {
                Ok(request) => Arc::new(request),
                Err(error) => return Err(EngineError::Shared(fill_guard.fail(error))),
            };
            let authority = authority_snapshot_from_request(request.as_ref());
            let estimated_bytes = request.estimated_size_bytes();
            {
                let mut cache = self
                    .immutable_request_snapshot_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if estimated_bytes <= IMMUTABLE_REQUEST_CACHE_MAX_ENTRY_BYTES {
                    while cache.slots.len() >= IMMUTABLE_REQUEST_CACHE_CAPACITY
                        || cache.total_bytes.saturating_add(estimated_bytes)
                            > IMMUTABLE_REQUEST_CACHE_MAX_TOTAL_BYTES
                    {
                        let Some(oldest) = cache.lru.pop_front() else {
                            break;
                        };
                        remove_immutable_request_entry(&mut cache, &oldest);
                        emit_immutable_request_cache_metric(
                            ImmutableRequestCacheOutcome::Eviction,
                            ImmutableRequestCacheReason::Capacity,
                        );
                    }
                }
                if estimated_bytes <= IMMUTABLE_REQUEST_CACHE_MAX_ENTRY_BYTES
                    && cache.total_bytes.saturating_add(estimated_bytes)
                        <= IMMUTABLE_REQUEST_CACHE_MAX_TOTAL_BYTES
                {
                    cache.total_bytes = cache.total_bytes.saturating_add(estimated_bytes);
                    cache.slots.insert(
                        key.clone(),
                        ImmutableRequestSnapshotCacheEntry {
                            request: Arc::clone(&request),
                            estimated_bytes,
                            last_touched: Instant::now(),
                        },
                    );
                    touch_immutable_request_lru(&mut cache, &key);
                } else {
                    emit_immutable_request_cache_metric(
                        ImmutableRequestCacheOutcome::Bypass,
                        ImmutableRequestCacheReason::Capacity,
                    );
                }
            }
            fill_guard.finish(Arc::clone(&request));
            emit_immutable_request_cache_metric(
                ImmutableRequestCacheOutcome::Miss,
                ImmutableRequestCacheReason::Cold,
            );
            Ok(AdmittedRequestAuthoritySnapshot {
                request,
                authority,
                project_root: project_root.to_path_buf(),
                materialization: materialization.clone(),
            })
        })
    }

    /// Return the coherent trust/parser/generation identity needed at an
    /// admission boundary. The proof-less API always rebuilds from current
    /// filesystem state; only an opaque admitted handle may use the
    /// path-independent immutable cache.
    pub fn effective_request_authority_snapshot(
        &self,
        project_root: Option<&Path>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<EffectiveRequestAuthoritySnapshot, EngineError> {
        self.checked_bundle_generation(|| {
            self.effective_request_authority_snapshot_current(
                project_root,
                subject_resolution_authority,
            )
        })
    }

    fn effective_request_authority_snapshot_current(
        &self,
        project_root: Option<&Path>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<EffectiveRequestAuthoritySnapshot, EngineError> {
        subject_resolution_authority
            .validate_for_materialized_root(project_root)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        let snapshot =
            self.effective_request_snapshot_current(project_root, subject_resolution_authority)?;
        Ok(authority_snapshot_from_request(&snapshot))
    }

    fn effective_request_snapshot_current(
        &self,
        project_root: Option<&Path>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<EffectiveRequestSnapshot, EngineError> {
        subject_resolution_authority
            .validate_for_materialized_root(project_root)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        let trust_store = self.effective_trust_store(project_root)?.into_owned();
        let parser_dispatcher =
            self.effective_parser_dispatcher_with_trust(project_root, &trust_store)?;
        let registry_fingerprint =
            self.fingerprint_for(parser_dispatcher.parser_tools.fingerprint());
        let effective_trust_identity = self.effective_trust_identity(&trust_store);
        let request_engine_generation_identity = self.request_engine_generation_identity();
        Ok(EffectiveRequestSnapshot {
            trust_store,
            parser_dispatcher,
            registry_fingerprint,
            effective_trust_identity,
            request_engine_generation_identity,
            subject_resolution_authority: subject_resolution_authority.clone(),
        })
    }

    /// Build the complete project request snapshot from the authoritative CAS
    /// closure retained by a state-issued materialization proof. No project
    /// pathname is reopened, so a concurrent checkout mutation cannot poison a
    /// content-addressed snapshot key.
    fn effective_request_snapshot_from_materialization(
        &self,
        materialization: &ryeos_state::PinnedProjectMaterialization,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<EffectiveRequestSnapshot, EngineError> {
        subject_resolution_authority
            .validate_for_materialized_root(Some(materialization.path()))
            .map_err(|error| EngineError::Internal(error.to_string()))?;

        let trust_base = self
            .request_trust_base
            .as_ref()
            .unwrap_or(&self.trust_store);
        let trust_store = trust_base.with_project_keys_from_content(materialization)?;
        let parser_tools = self
            .parser_dispatcher
            .parser_tools
            .with_project_overlay_from_content(materialization, &trust_store, &self.kinds)?;
        let parser_dispatcher = self.parser_dispatcher.with_parser_tools(parser_tools);
        let registry_fingerprint =
            self.fingerprint_for(parser_dispatcher.parser_tools.fingerprint());
        let effective_trust_identity = self.effective_trust_identity(&trust_store);
        let request_engine_generation_identity = self.request_engine_generation_identity();
        Ok(EffectiveRequestSnapshot {
            trust_store,
            parser_dispatcher,
            registry_fingerprint,
            effective_trust_identity,
            request_engine_generation_identity,
            subject_resolution_authority: subject_resolution_authority.clone(),
        })
    }

    fn effective_trust_identity(&self, effective: &TrustStore) -> String {
        let base = self
            .request_trust_base
            .as_ref()
            .unwrap_or(&self.trust_store);
        let mut identity = Vec::new();
        append_identity_field(&mut identity, effective.fingerprint().as_bytes());
        append_identity_field(&mut identity, base.fingerprint().as_bytes());
        match &self.request_trust_overlay_identity {
            Some(overlay) => {
                identity.push(1);
                append_identity_field(&mut identity, overlay.as_bytes());
            }
            None => identity.push(0),
        }
        lillux::cas::sha256_hex(&identity)
    }

    /// Install the catalog of `kind: runtime` items, normally built
    /// once at daemon startup by scanning bundle roots. Optional —
    /// `Engine::new` initializes the field to an empty registry.
    pub fn with_runtimes(mut self, runtimes: RuntimeRegistry) -> Self {
        self.runtimes = runtimes;
        self
    }

    pub fn with_launch_preparers(mut self, launch_preparers: LaunchPreparerRegistry) -> Self {
        self.launch_preparers = launch_preparers;
        self
    }

    /// Install the daemon's composer registry. Boot uses this same
    /// instance for validation; the launcher pulls it back off the
    /// engine when running the resolution pipeline so the two sides
    /// can never diverge.
    pub fn with_composers(mut self, composers: ComposerRegistry) -> Self {
        self.composers = composers;
        self
    }

    pub fn with_effective_validators(
        mut self,
        effective_validators: EffectiveValidatorRegistry,
    ) -> Self {
        self.effective_validators = effective_validators;
        self
    }

    /// Install the protocol registry, loaded from base roots at engine
    /// init. Empty by default for test compatibility.
    pub fn with_protocols(mut self, protocols: ProtocolRegistry) -> Self {
        self.protocols = protocols;
        self
    }

    /// Install the host-env passthrough bindings. Populated once at
    /// daemon bootstrap from `RYEOS_TOOL_ENV_PASSTHROUGH`. Empty by
    /// default for test compatibility.
    pub fn with_host_env(mut self, host_env: crate::runtime::HostEnvBindings) -> Self {
        self.host_env = host_env;
        self
    }

    /// Resolve a canonical ref to a concrete item.
    pub fn resolve(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
    ) -> Result<ResolvedItem, EngineError> {
        self.checked_bundle_generation(|| self.resolve_current(ctx, item_ref))
    }

    fn resolve_current(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
    ) -> Result<ResolvedItem, EngineError> {
        ctx.subject_resolution_authority
            .validate_for_project_context(&ctx.project_context)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        // Materialize project context
        let project_root = match &ctx.project_context {
            crate::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
            _ => None,
        };
        let request_snapshot = self.effective_request_snapshot_current(
            project_root.as_deref(),
            &ctx.subject_resolution_authority,
        )?;
        self.resolve_current_with_request_snapshot(ctx, item_ref, project_root, &request_snapshot)
    }

    fn resolve_current_with_request_snapshot(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
        project_root: Option<PathBuf>,
        request_snapshot: &EffectiveRequestSnapshot,
    ) -> Result<ResolvedItem, EngineError> {
        self.resolve_current_with_request_snapshot_and_source(
            ctx,
            item_ref,
            project_root,
            request_snapshot,
            None,
        )
        .map(|(resolved, _source)| resolved)
    }

    fn resolve_current_with_request_snapshot_and_source(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
        project_root: Option<PathBuf>,
        request_snapshot: &EffectiveRequestSnapshot,
        project_content: Option<&dyn crate::project_content::AuthoritativeProjectContent>,
    ) -> Result<(ResolvedItem, String), EngineError> {
        if request_snapshot.subject_resolution_authority != ctx.subject_resolution_authority {
            return Err(EngineError::Internal(
                "request snapshot authority differs from resolution context".to_string(),
            ));
        }

        // Kind schemas are system-only — no project overlay
        let kind_schema =
            self.kinds
                .get(&item_ref.kind)
                .ok_or_else(|| EngineError::UnsupportedKind {
                    kind: item_ref.kind.clone(),
                })?;

        // Build resolution roots (system-first order)
        let roots = self.resolution_roots(project_root.clone());

        tracing::debug!(item_ref = %item_ref, "resolving item");

        // Resolve to file path + space + matched extension (with clash diagnostics)
        let result = match (project_root.as_deref(), project_content) {
            (Some(project_root), Some(project_content)) => {
                crate::item_resolution::resolve_item_full_under_project_authority(
                    &roots,
                    kind_schema,
                    item_ref,
                    project_root,
                    project_content,
                )?
            }
            (_, None) => crate::item_resolution::resolve_item_full(&roots, kind_schema, item_ref)?,
            (None, Some(_)) => {
                return Err(EngineError::Internal(
                    "authoritative project content has no project root".to_string(),
                ));
            }
        };

        // Read file content
        let content = match (project_root.as_deref(), project_content) {
            (Some(project_root), Some(project_content)) => {
                crate::item_resolution::read_resolved_source_under_project_authority(
                    &result,
                    project_root,
                    project_content,
                )?
            }
            (_, None) => crate::item_resolution::read_item_source_no_follow(&result.winner_path)?,
            (None, Some(_)) => unreachable!("checked above"),
        };

        // Compute content hash
        let hash = crate::item_resolution::content_hash(&content);

        // Parse signature header using the matched extension's envelope
        let signature_header = kind_schema.spec_for(&result.matched_ext).and_then(|spec| {
            crate::item_resolution::parse_signature_header(&content, &spec.signature)
        });

        // Build ResolvedSourceFormat from the matched extension
        let source_format = kind_schema
            .resolved_format_for(&result.matched_ext)
            .ok_or_else(|| {
                EngineError::Internal(format!(
                    "matched extension {} has no source format in schema",
                    result.matched_ext
                ))
            })?;

        // Pin the exact signature-stripped bytes consumed by runtimes. Hook
        // occurrence identities use this digest, not the whole signed-file
        // digest carried in `content_hash`.
        let raw_content = lillux::signature::strip_signature_lines_with_envelope(
            &content,
            &source_format.signature.prefix,
            source_format.signature.suffix.as_deref(),
        );
        let raw_content_digest = crate::item_resolution::content_hash(&raw_content);

        // Parse raw document via the **effective** parser dispatcher
        // — the boot dispatcher overlaid by this project's
        // `.ai/parsers/` if any. Then apply extraction rules from
        // the schema.
        let parsed = request_snapshot.parser_dispatcher.dispatch(
            &source_format.parser,
            &content,
            Some(&result.winner_path),
            &source_format.signature,
        )?;
        // Path-anchoring validator runs BEFORE metadata extraction
        // populates the typed slots — a failure here is a structural
        // mismatch between metadata and on-disk location, not a parse
        // error. Item rejected at load time, daemon stays kind-agnostic.
        crate::kind_registry::validate_metadata_anchoring(
            &parsed,
            &kind_schema.extraction_rules,
            &kind_schema.directory,
            &result.winner_ai_root,
            &result.winner_path,
        )
        .map_err(|source| EngineError::MetadataAnchoringFailed {
            canonical_ref: item_ref.to_string(),
            source: Box::new(source),
        })?;

        let metadata = crate::kind_registry::apply_extraction_rules(
            &parsed,
            &kind_schema.extraction_rules,
            &result.winner_path,
            &kind_schema.directory,
        );

        tracing::debug!(
            item_ref = %item_ref,
            source_path = %result.winner_path.display(),
            space = %result.winner_space.as_str(),
            resolved_from = %result.winner_label,
            shadowed = result.shadowed.len(),
            "resolved item"
        );

        let resolved = ResolvedItem {
            canonical_ref: item_ref.clone(),
            kind: item_ref.kind.clone(),
            source_path: result.winner_path,
            source_space: result.winner_space,
            source_root: result.winner_root_identity,
            resolved_from: result.winner_label,
            shadowed: result.shadowed,
            probed_absent: result.probed_absent,
            materialized_project_root: project_root.clone(),
            subject_resolution_authority: ctx.subject_resolution_authority.clone(),
            raw_content_digest,
            content_hash: hash,
            signature_header,
            source_format,
            metadata,
        };
        self.record_engine_resolved_subject(&resolved, project_root.as_deref())?;
        Ok((resolved, content))
    }

    /// Resolve and verify one dispatch hop under an opaque admitted project
    /// snapshot. Parser/trust state comes from the content-addressed request
    /// snapshot, while the selected project source and every
    /// precedence-affecting project absence must agree with that authority.
    pub fn resolve_verified_under_admitted_authority(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
        project_root: &Path,
        admitted: &AdmittedRequestAuthoritySnapshot,
    ) -> Result<VerifiedItem, EngineError> {
        self.resolve_verified_source_under_admitted_authority(ctx, item_ref, project_root, admitted)
            .map(|(verified, _source)| verified)
    }

    fn resolve_verified_source_under_admitted_authority(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
        project_root: &Path,
        admitted: &AdmittedRequestAuthoritySnapshot,
    ) -> Result<(VerifiedItem, String), EngineError> {
        self.checked_bundle_generation(|| {
            admitted.validate_root_binding(project_root)?;
            let request_snapshot = admitted.request_snapshot();
            let project_content = admitted.project_content_for_root(project_root)?;
            let (resolved, source) = self.resolve_current_with_request_snapshot_and_source(
                ctx,
                item_ref,
                Some(project_root.to_path_buf()),
                request_snapshot,
                Some(project_content),
            )?;
            let request_authority = authority_snapshot_from_request(request_snapshot);
            let verified = self.verify_static_cached_with_source_under_authority(
                ctx,
                resolved,
                &source,
                &request_authority,
            )?;
            Ok((verified, source))
        })
    }

    /// Resolve, verify, and attest one canonical subject directly from an
    /// admitted project-content authority. No caller-supplied `ResolvedItem`
    /// and no project-path read participates in the attestation.
    pub fn resolve_attested_under_admitted_authority(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
        project_root: &Path,
        admitted: &AdmittedRequestAuthoritySnapshot,
    ) -> Result<VerifiedArtifactAttestation, EngineError> {
        let (verified_subject, source) = self.resolve_verified_source_under_admitted_authority(
            ctx,
            item_ref,
            project_root,
            admitted,
        )?;
        let request_authority = admitted.authority_snapshot_for_root(project_root)?;
        let subject_digest = self.attested_subject_digest(
            &verified_subject.resolved,
            Some(project_root),
            &ctx.subject_resolution_authority,
        )?;
        Ok(VerifiedArtifactAttestation {
            verified_subject: Arc::new(verified_subject),
            source_bytes: Arc::from(source.into_bytes()),
            subject_digest,
            engine_generation_identity: self.request_engine_generation_identity(),
            trust_identity: request_authority.effective_trust_identity.clone(),
            request_registry_fingerprint: request_authority.registry_fingerprint.clone(),
            subject_resolution_authority: ctx.subject_resolution_authority.clone(),
            admitted_content_generation: Some(admitted.content_proof_generation(project_root)?),
            resolution_closure_digest: None,
        })
    }

    /// Verify trust and integrity on a resolved item.
    ///
    /// Trust is the configured store plus keys explicitly declared by this
    /// request's project root.
    #[tracing::instrument(
        name = "engine:verify_item",
        skip(self, ctx, item),
        fields(canonical_ref = %item.canonical_ref)
    )]
    pub fn verify(
        &self,
        ctx: &PlanContext,
        item: ResolvedItem,
    ) -> Result<VerifiedItem, EngineError> {
        self.verify_static_cached(ctx, item)
    }

    /// Resolve, verify, and attest one live/projectless canonical subject
    /// without exposing a caller-assembled resolved/verified pair across the
    /// admission boundary.
    ///
    /// Mutable authority is deliberately handled by [`Engine::verify_attested`]:
    /// it re-resolves the canonical ref, compares the exact subject digest, and
    /// verifies the source bytes it seals. Content-addressed pinned/COW callers
    /// must use [`Engine::resolve_attested_under_admitted_authority`] instead.
    pub fn resolve_attested(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
    ) -> Result<VerifiedArtifactAttestation, EngineError> {
        if ctx
            .subject_resolution_authority
            .operational_generation()
            .is_some()
        {
            return Err(EngineError::Internal(
                "content-addressed subject attestation requires admitted project authority"
                    .to_string(),
            ));
        }
        let resolved = self.resolve(ctx, item_ref)?;
        self.verify_attested(ctx, resolved)
    }

    /// Verify one exact subject and seal the resulting evidence into an
    /// engine-owned attestation for a later admission boundary.
    pub fn verify_attested(
        &self,
        ctx: &PlanContext,
        item: ResolvedItem,
    ) -> Result<VerifiedArtifactAttestation, EngineError> {
        ctx.subject_resolution_authority
            .validate_for_project_context(&ctx.project_context)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        if item.subject_resolution_authority != ctx.subject_resolution_authority {
            return Err(EngineError::Internal(
                "resolved item carries different subject authority from its planning context"
                    .to_string(),
            ));
        }
        let project_root = project_root_from_context(ctx);
        if matches!(
            ctx.subject_resolution_authority,
            SubjectResolutionAuthority::LiveFs | SubjectResolutionAuthority::CowWorkspace { .. }
        ) {
            // A mutable project can change resolution precedence while an old
            // public `ResolvedItem` remains in the bounded proof cache. Always
            // resolve the canonical subject again before minting opaque
            // admission evidence; proof-cache membership is only a safe fast
            // path for immutable projectless/pinned authorities.
            let current = self.resolve(ctx, &item.canonical_ref)?;
            if self.resolved_subject_digest(&current, project_root)?
                != self.resolved_subject_digest(&item, project_root)?
            {
                return Err(EngineError::Internal(
                    "verified artifact attestation subject was not produced by the current mutable resolution"
                        .to_string(),
                ));
            }
        } else {
            self.ensure_engine_resolved_subject(&item, ctx)?;
        }
        let source = read_current_subject_source(&item)?;
        let verified_subject = Arc::new(self.verify_static_cached_with_source(ctx, item, &source)?);
        let request_authority = self.effective_request_authority_snapshot(
            project_root,
            &ctx.subject_resolution_authority,
        )?;
        let subject_digest = self.attested_subject_digest(
            &verified_subject.resolved,
            project_root,
            &ctx.subject_resolution_authority,
        )?;
        Ok(VerifiedArtifactAttestation {
            verified_subject,
            source_bytes: Arc::from(source.into_bytes()),
            subject_digest,
            engine_generation_identity: self.request_engine_generation_identity(),
            trust_identity: request_authority.effective_trust_identity,
            request_registry_fingerprint: request_authority.registry_fingerprint,
            subject_resolution_authority: ctx.subject_resolution_authority.clone(),
            admitted_content_generation: None,
            resolution_closure_digest: None,
        })
    }

    /// Apply the current generation/trust gate and consume an exact opaque
    /// attestation. No caller-supplied `ResolvedItem` participates here.
    pub fn consume_verified_attestation(
        &self,
        ctx: &PlanContext,
        attestation: &VerifiedArtifactAttestation,
        expected_subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<VerifiedItem, EngineError> {
        let request_authority = self.effective_request_authority_snapshot(
            project_root_from_context(ctx),
            expected_subject_resolution_authority,
        )?;
        self.consume_verified_attestation_under_authority(
            ctx,
            attestation,
            expected_subject_resolution_authority,
            &request_authority,
            true,
        )
    }

    /// Consume attestation evidence under an immutable authority handle minted
    /// from a state-issued materialization proof. This is the path-independent
    /// warm boundary; it cannot be reached with a caller-constructed snapshot.
    pub fn consume_verified_attestation_under_admitted_authority(
        &self,
        ctx: &PlanContext,
        attestation: &VerifiedArtifactAttestation,
        expected_subject_resolution_authority: &SubjectResolutionAuthority,
        admitted: &AdmittedRequestAuthoritySnapshot,
    ) -> Result<VerifiedItem, EngineError> {
        let project_root = project_root_from_context(ctx)
            .or(attestation
                .verified_subject
                .resolved
                .materialized_project_root
                .as_deref())
            .ok_or_else(|| {
                EngineError::Internal(
                    "admitted pinned attestation has no materialized project root".to_string(),
                )
            })?;
        admitted.validate_root_binding(project_root)?;
        let expected_content_generation = admitted.content_proof_generation(project_root)?;
        if attestation.admitted_content_generation.as_deref()
            != Some(expected_content_generation.as_str())
        {
            return Err(EngineError::Internal(
                "verified artifact attestation was not minted from the admitted project content authority"
                    .to_string(),
            ));
        }
        self.consume_verified_attestation_under_authority(
            ctx,
            attestation,
            expected_subject_resolution_authority,
            admitted.snapshot(),
            false,
        )
    }

    fn consume_verified_attestation_under_authority(
        &self,
        ctx: &PlanContext,
        attestation: &VerifiedArtifactAttestation,
        expected_subject_resolution_authority: &SubjectResolutionAuthority,
        request_authority: &EffectiveRequestAuthoritySnapshot,
        revalidate_mutable_path: bool,
    ) -> Result<VerifiedItem, EngineError> {
        expected_subject_resolution_authority
            .validate_for_project_context(&ctx.project_context)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        if &request_authority.subject_resolution_authority != expected_subject_resolution_authority
        {
            return Err(EngineError::Internal(
                "request authority differs from expected subject authority".to_string(),
            ));
        }
        if &attestation.subject_resolution_authority != expected_subject_resolution_authority {
            return Err(EngineError::Internal(
                "verified artifact attestation carries different subject authority".to_string(),
            ));
        }
        if &ctx.subject_resolution_authority != expected_subject_resolution_authority {
            return Err(EngineError::Internal(
                "planning context carries different subject authority from the admitted attestation"
                    .to_string(),
            ));
        }
        let project_root = project_root_from_context(ctx);
        if attestation.engine_generation_identity != self.request_engine_generation_identity()
            || attestation.trust_identity != request_authority.effective_trust_identity
            || attestation.request_registry_fingerprint != request_authority.registry_fingerprint
        {
            return Err(EngineError::Internal(
                "verified artifact attestation is stale under current engine/trust authority"
                    .to_string(),
            ));
        }
        if revalidate_mutable_path
            && matches!(
                expected_subject_resolution_authority,
                SubjectResolutionAuthority::LiveFs
                    | SubjectResolutionAuthority::CowWorkspace { .. }
            )
        {
            let current =
                self.resolve(ctx, &attestation.verified_subject.resolved.canonical_ref)?;
            let current_digest = self.attested_subject_digest(
                &current,
                project_root,
                expected_subject_resolution_authority,
            )?;
            if current_digest != attestation.subject_digest {
                return Err(EngineError::Internal(
                    "verified artifact attestation source or resolution precedence changed before admission"
                        .to_string(),
                ));
            }
        }
        let current_digest = self.attested_subject_digest(
            &attestation.verified_subject.resolved,
            project_root,
            expected_subject_resolution_authority,
        )?;
        if current_digest != attestation.subject_digest {
            return Err(EngineError::Internal(
                "verified artifact attestation subject identity is inconsistent".to_string(),
            ));
        }
        let mut verified = attestation.verified_subject.as_ref().clone();
        relocate_verified_subject_for_current_root(
            &mut verified,
            project_root,
            expected_subject_resolution_authority,
        )?;
        Ok(verified)
    }

    /// Re-issue engine-owned static evidence after applying the same current
    /// generation/trust/typed-authority gate as admission. This is used by an
    /// immutable resolution-cache hit to cross a later admission boundary
    /// without reopening the already-attested source pathname.
    pub fn reissue_verified_attestation(
        &self,
        ctx: &PlanContext,
        attestation: &VerifiedArtifactAttestation,
        expected_subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<VerifiedArtifactAttestation, EngineError> {
        self.consume_verified_attestation(ctx, attestation, expected_subject_resolution_authority)?;
        Ok(VerifiedArtifactAttestation {
            verified_subject: attestation.verified_subject.clone(),
            source_bytes: attestation.source_bytes.clone(),
            subject_digest: attestation.subject_digest.clone(),
            engine_generation_identity: attestation.engine_generation_identity.clone(),
            trust_identity: attestation.trust_identity.clone(),
            request_registry_fingerprint: attestation.request_registry_fingerprint.clone(),
            subject_resolution_authority: attestation.subject_resolution_authority.clone(),
            admitted_content_generation: attestation.admitted_content_generation.clone(),
            resolution_closure_digest: attestation.resolution_closure_digest.clone(),
        })
    }

    /// Re-issue cached evidence under a proof-bearing immutable request
    /// authority without reopening the disposable materialization.
    pub fn reissue_verified_attestation_under_admitted_authority(
        &self,
        ctx: &PlanContext,
        attestation: &VerifiedArtifactAttestation,
        expected_subject_resolution_authority: &SubjectResolutionAuthority,
        admitted: &AdmittedRequestAuthoritySnapshot,
    ) -> Result<VerifiedArtifactAttestation, EngineError> {
        self.consume_verified_attestation_under_admitted_authority(
            ctx,
            attestation,
            expected_subject_resolution_authority,
            admitted,
        )?;
        Ok(VerifiedArtifactAttestation {
            verified_subject: attestation.verified_subject.clone(),
            source_bytes: attestation.source_bytes.clone(),
            subject_digest: attestation.subject_digest.clone(),
            engine_generation_identity: attestation.engine_generation_identity.clone(),
            trust_identity: attestation.trust_identity.clone(),
            request_registry_fingerprint: attestation.request_registry_fingerprint.clone(),
            subject_resolution_authority: attestation.subject_resolution_authority.clone(),
            admitted_content_generation: attestation.admitted_content_generation.clone(),
            resolution_closure_digest: attestation.resolution_closure_digest.clone(),
        })
    }

    /// Bind static subject evidence to the exact resolution/composition
    /// closure and its negative probes. This remains evidence, not execution
    /// authority; admission still applies current principal and policy gates.
    fn bind_verified_attestation_to_resolution(
        &self,
        ctx: &PlanContext,
        mut attestation: VerifiedArtifactAttestation,
        output: &crate::resolution::ResolutionOutput,
        probed_absent: &[crate::contracts::ProbedAbsence],
        resolution_root: Option<&Path>,
        expected_subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<VerifiedArtifactAttestation, EngineError> {
        self.consume_verified_attestation(
            ctx,
            &attestation,
            expected_subject_resolution_authority,
        )?;
        attestation.resolution_closure_digest = Some(self.resolution_closure_digest(
            output,
            probed_absent,
            resolution_root,
            expected_subject_resolution_authority,
        )?);
        Ok(attestation)
    }

    /// Run the canonical engine-owned resolution pipeline and bind its exact
    /// positive/negative closure to the root's opaque static attestation.
    pub fn resolve_verified_resolution_closure(
        &self,
        ctx: &PlanContext,
        root_attestation: &VerifiedArtifactAttestation,
        materialized_project_root: Option<PathBuf>,
    ) -> Result<VerifiedResolutionClosure, EngineError> {
        let subject_resolution_authority = &ctx.subject_resolution_authority;
        let verified_root =
            self.consume_verified_attestation(ctx, root_attestation, subject_resolution_authority)?;
        let request_snapshot = self.effective_request_snapshot(
            materialized_project_root.as_deref(),
            subject_resolution_authority,
        )?;
        let roots = self.resolution_roots(materialized_project_root.clone());
        let (output, probed_absent) = crate::resolution::run_resolution_pipeline_with_probes(
            &verified_root.resolved.canonical_ref,
            &self.kinds,
            &request_snapshot.parser_dispatcher,
            &roots,
            &request_snapshot.trust_store,
            &self.composers,
        )
        .map_err(|error| {
            resolution_error_to_engine(error, &verified_root.resolved.canonical_ref)
        })?;
        if output.root.resolved_ref != verified_root.resolved.canonical_ref.to_string()
            || output.root.source_space != verified_root.resolved.source_space
            || output.root.raw_content_digest != verified_root.resolved.raw_content_digest
            || output.root.source_content_digest != verified_root.resolved.content_hash
        {
            return Err(EngineError::Internal(
                "canonical resolution closure root differs from its verified root subject"
                    .to_string(),
            ));
        }
        let attestation = self.bind_verified_attestation_to_resolution(
            ctx,
            self.reissue_verified_attestation(ctx, root_attestation, subject_resolution_authority)?,
            &output,
            &probed_absent,
            materialized_project_root.as_deref(),
            subject_resolution_authority,
        )?;
        Ok(VerifiedResolutionClosure {
            output,
            probed_absent,
            attestation,
        })
    }

    /// Resolve and attest one root entirely under an opaque, state-issued
    /// project-content authority. Project parser/trust inputs come from the
    /// admitted request snapshot, and every project resolution positive and
    /// absence is checked against the same authoritative tree before the
    /// closure can cross admission.
    pub fn resolve_verified_resolution_closure_under_admitted_authority(
        &self,
        ctx: &PlanContext,
        root_attestation: &VerifiedArtifactAttestation,
        materialized_project_root: PathBuf,
        admitted: &AdmittedRequestAuthoritySnapshot,
    ) -> Result<VerifiedResolutionClosure, EngineError> {
        let subject_resolution_authority = &ctx.subject_resolution_authority;
        let verified_root = self.consume_verified_attestation_under_admitted_authority(
            ctx,
            root_attestation,
            subject_resolution_authority,
            admitted,
        )?;
        let request_snapshot = self.effective_request_snapshot_under_admitted_authority(
            &materialized_project_root,
            admitted,
        )?;
        let roots = self.resolution_roots(Some(materialized_project_root.clone()));
        let project_content = admitted.project_content_for_root(&materialized_project_root)?;
        let (output, probed_absent) =
            crate::resolution::run_resolution_pipeline_with_probes_under_project_authority(
                &verified_root.resolved.canonical_ref,
                &self.kinds,
                &request_snapshot.parser_dispatcher,
                &roots,
                &request_snapshot.trust_store,
                &self.composers,
                &materialized_project_root,
                project_content,
            )
            .map_err(|error| {
                resolution_error_to_engine(error, &verified_root.resolved.canonical_ref)
            })?;
        if output.root.resolved_ref != verified_root.resolved.canonical_ref.to_string()
            || output.root.source_space != verified_root.resolved.source_space
            || output.root.raw_content_digest != verified_root.resolved.raw_content_digest
            || output.root.source_content_digest != verified_root.resolved.content_hash
        {
            return Err(EngineError::Internal(
                "canonical resolution closure root differs from its verified root subject"
                    .to_string(),
            ));
        }
        admitted.validate_resolution_dependencies(
            &materialized_project_root,
            &output,
            &probed_absent,
        )?;
        let mut attestation = self.reissue_verified_attestation_under_admitted_authority(
            ctx,
            root_attestation,
            subject_resolution_authority,
            admitted,
        )?;
        attestation.resolution_closure_digest = Some(self.resolution_closure_digest(
            &output,
            &probed_absent,
            Some(&materialized_project_root),
            subject_resolution_authority,
        )?);
        Ok(VerifiedResolutionClosure {
            output,
            probed_absent,
            attestation,
        })
    }

    pub fn validate_attested_resolution_closure(
        &self,
        attestation: &VerifiedArtifactAttestation,
        output: &crate::resolution::ResolutionOutput,
        probed_absent: &[crate::contracts::ProbedAbsence],
        resolution_root: Option<&Path>,
        expected_subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<(), EngineError> {
        let expected = self.resolution_closure_digest(
            output,
            probed_absent,
            resolution_root,
            expected_subject_resolution_authority,
        )?;
        if attestation.resolution_closure_digest.as_deref() != Some(expected.as_str()) {
            return Err(EngineError::Internal(
                "verified artifact attestation does not bind the admitted resolution closure"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn verify_static_cached(
        &self,
        ctx: &PlanContext,
        item: ResolvedItem,
    ) -> Result<VerifiedItem, EngineError> {
        let source = read_current_subject_source(&item)?;
        self.verify_static_cached_with_source(ctx, item, &source)
    }

    fn verify_static_cached_with_source(
        &self,
        ctx: &PlanContext,
        item: ResolvedItem,
        source: &str,
    ) -> Result<VerifiedItem, EngineError> {
        ctx.subject_resolution_authority
            .validate_for_project_context(&ctx.project_context)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        if item.subject_resolution_authority != ctx.subject_resolution_authority {
            return Err(EngineError::Internal(
                "resolved item authority differs from verification context".to_string(),
            ));
        }
        let project_root = match &ctx.project_context {
            crate::contracts::ProjectContext::LocalPath { path } => Some(path.as_path()),
            _ => None,
        };
        let request_authority = self.effective_request_authority_snapshot(
            project_root,
            &ctx.subject_resolution_authority,
        )?;
        self.verify_static_cached_with_source_under_authority(ctx, item, source, &request_authority)
    }

    fn verify_static_cached_with_source_under_authority(
        &self,
        ctx: &PlanContext,
        item: ResolvedItem,
        source: &str,
        request_authority: &EffectiveRequestAuthoritySnapshot,
    ) -> Result<VerifiedItem, EngineError> {
        if request_authority.subject_resolution_authority != ctx.subject_resolution_authority {
            return Err(EngineError::Internal(
                "request verification authority differs from planning context".to_string(),
            ));
        }
        let project_root = match &ctx.project_context {
            crate::contracts::ProjectContext::LocalPath { path } => Some(path.as_path()),
            _ => None,
        };
        let current_content_hash = crate::item_resolution::content_hash(source);
        if current_content_hash != item.content_hash {
            return Err(EngineError::ContentHashMismatch {
                canonical_ref: item.canonical_ref.to_string(),
                expected: item.content_hash.clone(),
                actual: current_content_hash,
            });
        }
        let subject_digest = self.resolved_subject_digest(&item, project_root)?;
        let key_material = serde_json::json!({
            "schema_version": 1,
            "engine_generation_identity": self.request_engine_generation_identity(),
            "trust_identity": &request_authority.effective_trust_identity,
            "subject_digest": subject_digest,
        });
        let key = lillux::canonical_json(&key_material)
            .map(|canonical| lillux::sha256_hex(canonical.as_bytes()))
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        let pending_owner = 'pending_owner: {
            let lookup = {
                let mut cache = static_verification_cache()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                sweep_static_verification_cache(&mut cache);
                if let Some(evidence) = cache.slots.get_mut(&key).map(|entry| {
                    entry.last_touched = Instant::now();
                    entry.evidence.clone()
                }) {
                    touch_static_verification_lru(&mut cache, &key);
                    Some(Ok(evidence))
                } else if let Some(pending) = cache.pending.get(&key) {
                    Some(Err(pending.clone()))
                } else if cache.pending.len() >= STATIC_VERIFICATION_CACHE_MAX_PENDING {
                    None
                } else {
                    let pending = Arc::new(StaticVerificationPending::default());
                    cache.pending.insert(key.clone(), pending.clone());
                    break 'pending_owner Some(pending);
                }
            };
            match lookup {
                Some(Ok(evidence)) => {
                    let verified = VerifiedItem {
                        resolved: item,
                        signer: evidence.signer.clone(),
                        trust_class: evidence.trust_class,
                        pinned_version: evidence.pinned_version.clone(),
                    };
                    tracing::debug!(
                        item_ref = %verified.resolved.canonical_ref,
                        "static item verification attestation cache hit"
                    );
                    emit_static_verification_cache_metric(
                        StaticVerificationCacheOutcome::Hit,
                        StaticVerificationCacheReason::Ready,
                    );
                    return Ok(verified);
                }
                Some(Err(pending)) => {
                    let mut completed = pending
                        .result
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    while completed.is_none() {
                        completed = pending
                            .ready
                            .wait(completed)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    match completed
                        .as_ref()
                        .expect("completed static verification fill has an outcome")
                    {
                        Ok(evidence) => {
                            let verified = VerifiedItem {
                                resolved: item,
                                signer: evidence.signer.clone(),
                                trust_class: evidence.trust_class,
                                pinned_version: evidence.pinned_version.clone(),
                            };
                            emit_static_verification_cache_metric(
                                StaticVerificationCacheOutcome::Hit,
                                StaticVerificationCacheReason::SingleFlight,
                            );
                            return Ok(verified);
                        }
                        Err(error) => return Err(EngineError::Shared(error.clone())),
                    }
                }
                None => None,
            }
        };
        if pending_owner.is_none() {
            emit_static_verification_cache_metric(
                StaticVerificationCacheOutcome::Bypass,
                StaticVerificationCacheReason::PendingCapacity,
            );
            return crate::trust::verify_resolved_item_content_with_hash(
                item,
                source,
                &current_content_hash,
                &request_authority.trust_store,
            );
        }
        let pending_owner = pending_owner.expect("checked above");
        let fill_guard = StaticVerificationFillGuard {
            key: key.clone(),
            pending: pending_owner,
            completed: false,
        };
        let verified = match crate::trust::verify_resolved_item_content_with_hash(
            item,
            source,
            &current_content_hash,
            &request_authority.trust_store,
        ) {
            Ok(verified) => verified,
            Err(error) => return Err(EngineError::Shared(fill_guard.fail(error))),
        };
        let evidence = Arc::new(StaticVerificationEvidence {
            signer: verified.signer.clone(),
            trust_class: verified.trust_class,
            pinned_version: verified.pinned_version.clone(),
        });
        {
            let mut cache = static_verification_cache()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !cache.slots.contains_key(&key) {
                while cache.slots.len() >= STATIC_VERIFICATION_CACHE_CAPACITY {
                    let Some(oldest) = cache.lru.pop_front() else {
                        break;
                    };
                    cache.slots.remove(&oldest);
                }
                cache.lru.push_back(key.clone());
                cache.slots.insert(
                    key.clone(),
                    StaticVerificationCacheEntry {
                        evidence: evidence.clone(),
                        last_touched: Instant::now(),
                    },
                );
            }
        }
        fill_guard.finish(evidence);
        emit_static_verification_cache_metric(
            StaticVerificationCacheOutcome::Miss,
            StaticVerificationCacheReason::Cold,
        );
        tracing::debug!(
            item_ref = %verified.resolved.canonical_ref,
            trust_class = ?verified.trust_class,
            "verified item"
        );
        Ok(verified)
    }

    fn attested_subject_digest(
        &self,
        item: &ResolvedItem,
        project_root: Option<&Path>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<String, EngineError> {
        let resolution_root_identity = match subject_resolution_authority {
            SubjectResolutionAuthority::Projectless => serde_json::Value::Null,
            SubjectResolutionAuthority::LiveFs => {
                serde_json::to_value(project_root.ok_or_else(|| {
                    EngineError::Internal("live attestation has no canonical project root".into())
                })?)
                .map_err(|error| EngineError::Internal(error.to_string()))?
            }
            SubjectResolutionAuthority::PinnedGeneration { snapshot_hash } => {
                serde_json::json!({"snapshot_hash": snapshot_hash})
            }
            SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash,
                current_operational_generation,
            } => serde_json::json!({
                "base_snapshot_hash": base_snapshot_hash,
                "current_operational_generation": current_operational_generation,
            }),
        };
        let stable_subject_root = match subject_resolution_authority {
            SubjectResolutionAuthority::Projectless => None,
            SubjectResolutionAuthority::LiveFs => project_root,
            SubjectResolutionAuthority::PinnedGeneration { .. }
            | SubjectResolutionAuthority::CowWorkspace { .. } => {
                item.materialized_project_root.as_deref().or(project_root)
            }
        };
        let value = serde_json::json!({
            "schema_version": 1,
            "verified_subject_digest": self.resolved_subject_digest(item, stable_subject_root)?,
            "subject_resolution_authority": subject_resolution_authority,
            "resolution_root_identity": resolution_root_identity,
        });
        let canonical = lillux::canonical_json(&value)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    fn resolution_closure_digest(
        &self,
        output: &crate::resolution::ResolutionOutput,
        probed_absent: &[crate::contracts::ProbedAbsence],
        resolution_root: Option<&Path>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<String, EngineError> {
        let stable_path = |path: &Path| {
            resolution_root
                .and_then(|root| path.strip_prefix(root).ok())
                .map(|relative| serde_json::json!({"project_relative": relative}))
                .unwrap_or_else(|| serde_json::json!({"exact": path}))
        };
        let stable_item = |item: &crate::resolution::ResolvedAncestor| {
            let mut value = serde_json::to_value(item)
                .map_err(|error| EngineError::Internal(error.to_string()))?;
            let object = value.as_object_mut().ok_or_else(|| {
                EngineError::Internal(
                    "resolution ancestor did not serialize to an object".to_string(),
                )
            })?;
            object.insert("source_path".to_string(), stable_path(&item.source_path));
            Ok::<_, EngineError>(value)
        };
        let root = stable_item(&output.root)?;
        let ancestors = output
            .ancestors
            .iter()
            .map(stable_item)
            .collect::<Result<Vec<_>, _>>()?;
        let referenced_items = output
            .referenced_items
            .iter()
            .map(stable_item)
            .collect::<Result<Vec<_>, _>>()?;
        let references_edges = output
            .references_edges
            .iter()
            .map(|edge| {
                let mut value = serde_json::to_value(edge)
                    .map_err(|error| EngineError::Internal(error.to_string()))?;
                let object = value.as_object_mut().ok_or_else(|| {
                    EngineError::Internal(
                        "resolution edge did not serialize to an object".to_string(),
                    )
                })?;
                object.insert(
                    "from_source_path".to_string(),
                    stable_path(&edge.from_source_path),
                );
                object.insert(
                    "to_source_path".to_string(),
                    stable_path(&edge.to_source_path),
                );
                Ok::<_, EngineError>(value)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let absences = probed_absent
            .iter()
            .map(|absence| {
                serde_json::json!({
                    "space": absence.space,
                    "path": stable_path(&absence.path),
                })
            })
            .collect::<Vec<_>>();
        let value = serde_json::json!({
            "schema_version": 1,
            "engine_generation_identity": self.request_engine_generation_identity(),
            "subject_resolution_authority": subject_resolution_authority,
            "root": root,
            "ancestors": ancestors,
            "references_edges": references_edges,
            "referenced_items": referenced_items,
            "step_outputs": &output.step_outputs,
            "effective_trust_class": output.effective_trust_class,
            "composed": &output.composed,
            "probed_absent": absences,
        });
        let canonical = lillux::canonical_json(&value)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    fn resolved_subject_digest(
        &self,
        item: &ResolvedItem,
        project_root: Option<&Path>,
    ) -> Result<String, EngineError> {
        let source_path =
            self.stable_subject_path(&item.source_path, item.source_space, project_root);
        let shadowed = item
            .shadowed
            .iter()
            .map(|candidate| {
                serde_json::json!({
                    "label": &candidate.label,
                    "space": candidate.space,
                    "path": self.stable_subject_path(
                        &candidate.path,
                        candidate.space,
                        project_root,
                    ),
                })
            })
            .collect::<Vec<_>>();
        let probed_absent = item
            .probed_absent
            .iter()
            .map(|absence| {
                serde_json::json!({
                    "space": absence.space,
                    "path": self.stable_subject_path(
                        &absence.path,
                        absence.space,
                        project_root,
                    ),
                })
            })
            .collect::<Vec<_>>();
        let materialized_project_root = match item.materialized_project_root.as_deref() {
            None => serde_json::Value::Null,
            Some(materialized) if Some(materialized) == project_root => {
                serde_json::json!({"matches_context_root": true})
            }
            Some(materialized) => {
                serde_json::json!({"matches_context_root": false, "exact": materialized})
            }
        };
        let value = serde_json::json!({
            "schema_version": 1,
            "canonical_ref": item.canonical_ref.to_string(),
            "kind": &item.kind,
            "source_path": source_path,
            "source_space": item.source_space,
            "resolved_from": &item.resolved_from,
            "shadowed": shadowed,
            "probed_absent": probed_absent,
            "materialized_project_root": materialized_project_root,
            "subject_resolution_authority": &item.subject_resolution_authority,
            "raw_content_digest": &item.raw_content_digest,
            "content_hash": &item.content_hash,
            "signature_header": &item.signature_header,
            "source_format": &item.source_format,
            "metadata": &item.metadata,
        });
        let canonical = lillux::canonical_json(&value)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    fn engine_resolved_subject_proof_key(
        &self,
        item: &ResolvedItem,
        project_root: Option<&Path>,
    ) -> Result<String, EngineError> {
        let value = serde_json::json!({
            "schema_version": 1,
            "engine_generation_identity": self.request_engine_generation_identity(),
            "subject_digest": self.resolved_subject_digest(item, project_root)?,
        });
        let canonical = lillux::canonical_json(&value)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    fn record_engine_resolved_subject(
        &self,
        item: &ResolvedItem,
        project_root: Option<&Path>,
    ) -> Result<(), EngineError> {
        let key = self.engine_resolved_subject_proof_key(item, project_root)?;
        let now = Instant::now();
        let mut proofs = engine_resolved_subject_proofs()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stale = proofs
            .slots
            .iter()
            .filter(|(_, touched)| {
                now.duration_since(**touched) >= ENGINE_RESOLVED_SUBJECT_PROOF_IDLE_TTL
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for stale_key in stale {
            proofs.slots.remove(&stale_key);
            if let Some(position) = proofs
                .lru
                .iter()
                .position(|candidate| candidate == &stale_key)
            {
                proofs.lru.remove(position);
            }
        }
        if !proofs.slots.contains_key(&key) {
            while proofs.slots.len() >= ENGINE_RESOLVED_SUBJECT_PROOF_CAPACITY {
                let Some(oldest) = proofs.lru.pop_front() else {
                    break;
                };
                proofs.slots.remove(&oldest);
            }
        } else if let Some(position) = proofs.lru.iter().position(|candidate| candidate == &key) {
            proofs.lru.remove(position);
        }
        proofs.slots.insert(key.clone(), now);
        proofs.lru.push_back(key);
        Ok(())
    }

    fn ensure_engine_resolved_subject(
        &self,
        item: &ResolvedItem,
        context: &PlanContext,
    ) -> Result<(), EngineError> {
        let project_root = project_root_from_context(context);
        let key = self.engine_resolved_subject_proof_key(item, project_root)?;
        let proof_is_current = {
            let mut proofs = engine_resolved_subject_proofs()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match proofs.slots.get(&key).copied() {
                Some(touched) if touched.elapsed() < ENGINE_RESOLVED_SUBJECT_PROOF_IDLE_TTL => {
                    proofs.slots.insert(key.clone(), Instant::now());
                    if let Some(position) =
                        proofs.lru.iter().position(|candidate| candidate == &key)
                    {
                        proofs.lru.remove(position);
                    }
                    proofs.lru.push_back(key.clone());
                    true
                }
                Some(_) => {
                    proofs.slots.remove(&key);
                    if let Some(position) =
                        proofs.lru.iter().position(|candidate| candidate == &key)
                    {
                        proofs.lru.remove(position);
                    }
                    false
                }
                None => false,
            }
        };
        if proof_is_current {
            return Ok(());
        }
        // Bounded proof-cache eviction is never an authority failure. Re-run
        // the canonical resolver and require an exact subject digest before
        // minting the opaque attestation.
        let current = self.resolve(context, &item.canonical_ref)?;
        let current_digest = self.resolved_subject_digest(&current, project_root)?;
        let supplied_digest = self.resolved_subject_digest(item, project_root)?;
        if current_digest != supplied_digest {
            return Err(EngineError::Internal(
                "verified artifact attestation subject was not produced by the current engine resolution"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn stable_subject_path(
        &self,
        path: &Path,
        space: crate::contracts::ItemSpace,
        project_root: Option<&Path>,
    ) -> serde_json::Value {
        if space == crate::contracts::ItemSpace::Project {
            if let Some(relative) = project_root.and_then(|root| path.strip_prefix(root).ok()) {
                return serde_json::json!({"space": "project", "relative": relative});
            }
        } else if let Some((index, relative)) =
            self.bundle_roots
                .iter()
                .enumerate()
                .find_map(|(index, root)| {
                    path.strip_prefix(root)
                        .ok()
                        .map(|relative| (index, relative))
                })
        {
            return serde_json::json!({
                "space": "bundle",
                "root_index": index,
                "relative": relative,
            });
        }
        // Synthetic/test engines and explicitly external sources retain their
        // exact path. This cannot create a false cross-root cache hit.
        serde_json::json!({"space": space, "exact": path})
    }

    /// Resolve, verify, compose, and return an effective item value.
    ///
    /// Unlike [`Engine::build_plan`], this is intentionally
    /// non-executing and works for non-executable kinds such as
    /// `surface` and `client`. It reuses the same resolution pipeline
    /// and composer registry that launch paths use, so service/API/CLI
    /// consumers do not grow parallel item semantics.
    pub fn effective_item(
        &self,
        request: EffectiveItemRequest,
    ) -> Result<EffectiveItem, EngineError> {
        self.checked_bundle_generation(|| self.effective_item_current(request))
    }

    /// Resolve and compose an exact item closure for a downstream admission
    /// boundary. Unlike [`EffectiveItem`], this retains the complete verified
    /// root/ancestor/reference bytes and provenance required to seal and later
    /// finalize the same program without re-resolving its canonical name.
    pub fn effective_resolution_output(
        &self,
        request: EffectiveItemRequest,
    ) -> Result<crate::resolution::ResolutionOutput, EngineError> {
        self.checked_bundle_generation(|| self.effective_resolution_output_current(&request))
    }

    fn effective_resolution_output_current(
        &self,
        request: &EffectiveItemRequest,
    ) -> Result<crate::resolution::ResolutionOutput, EngineError> {
        let ref_str = request.item_ref.to_string();
        if let Some(expected) = &request.expected_kind
            && expected != &request.item_ref.kind
        {
            return Err(EngineError::EffectiveItemWrongKind {
                canonical_ref: ref_str,
                expected: expected.clone(),
                found: request.item_ref.kind.clone(),
            });
        }
        let roots = self.resolution_roots(request.project_root.clone());
        let request_snapshot = self.effective_request_snapshot_current(
            request.project_root.as_deref(),
            &request.subject_resolution_authority,
        )?;
        crate::resolution::run_effective_item_pipeline(
            &request.item_ref,
            &self.kinds,
            &request_snapshot.parser_dispatcher,
            &roots,
            &request_snapshot.trust_store,
            &self.composers,
        )
        .map_err(|error| resolution_error_to_engine(error, &request.item_ref))
    }

    fn effective_item_current(
        &self,
        request: EffectiveItemRequest,
    ) -> Result<EffectiveItem, EngineError> {
        let output = self.effective_resolution_output_current(&request)?;

        let trust_class = output.effective_trust_class;
        let trusted = matches!(
            trust_class,
            crate::resolution::TrustClass::TrustedBundle
                | crate::resolution::TrustClass::TrustedProject
        );
        let provenance = output.provenance();

        let bundle_root = match &output.root.source_root {
            crate::contracts::ItemSourceRoot::Bundle { name } => Some(
                self.registered_bundle_root(name)
                    .ok_or_else(|| {
                        EngineError::Internal(format!(
                            "effective item names bundle root {name}, but that root is absent from the admitted generation"
                        ))
                    })?
                    .to_path_buf(),
            ),
            crate::contracts::ItemSourceRoot::Project
            | crate::contracts::ItemSourceRoot::Node
            | crate::contracts::ItemSourceRoot::Search { .. } => None,
        };

        // Build diagnostics from the resolution output.
        let mut diagnostics = Vec::new();

        // Shadowing diagnostics: if ancestors exist, note the extends
        // chain.
        if !output.ancestors.is_empty() {
            diagnostics.push(EffectiveItemDiagnostic {
                level: "info".into(),
                message: format!(
                    "extends chain: {} -> {}",
                    output.root.resolved_ref,
                    output
                        .ancestors
                        .iter()
                        .map(|a| a.resolved_ref.as_str())
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ),
            });
        }

        Ok(EffectiveItem {
            requested_ref: request.item_ref.to_string(),
            canonical_ref: output.root.resolved_ref.clone(),
            kind: request.item_ref.kind,
            trusted,
            trust_class,
            root_trust_class: output.root.trust_class,
            source: EffectiveItemSource {
                path: output.root.source_path,
                content_hash: output.root.source_content_digest,
                bundle_root,
            },
            provenance,
            composed_value: output.composed.composed,
            derived: output.composed.derived,
            policy_facts: output.composed.policy_facts,
            diagnostics,
        })
    }

    /// Build a normalized execution plan from a verified item.
    ///
    /// Checks execution scope on the principal before building.
    /// Uses system-only kind schemas and system+user trust.
    /// `sealed_content`, when present, answers dependency verification for
    /// paths an admitted realization covers; live bytes answer the rest.
    pub fn build_plan(
        &self,
        ctx: &PlanContext,
        item: &VerifiedItem,
        parameters: &Value,
        hints: &ExecutionHints,
        sealed_content: Option<&dyn crate::project_content::SealedDependencyBytes>,
    ) -> Result<ExecutionPlan, EngineError> {
        self.checked_bundle_generation(|| {
            self.build_plan_current(ctx, item, parameters, hints, sealed_content)
        })
    }

    fn build_plan_current(
        &self,
        ctx: &PlanContext,
        item: &VerifiedItem,
        parameters: &Value,
        hints: &ExecutionHints,
        sealed_content: Option<&dyn crate::project_content::SealedDependencyBytes>,
    ) -> Result<ExecutionPlan, EngineError> {
        crate::scope::check_execution_scope(&ctx.requested_by)?;

        tracing::debug!(
            item_ref = %item.resolved.canonical_ref,
            "building execution plan"
        );

        let project_root = match &ctx.project_context {
            crate::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
            _ => None,
        };
        let roots = self.resolution_roots(project_root.clone());
        let request_snapshot = self.effective_request_snapshot_current(
            project_root.as_deref(),
            &ctx.subject_resolution_authority,
        )?;

        crate::plan_builder::build_plan(crate::plan_builder::BuildPlanInput {
            item,
            root_source: None,
            parameters,
            hints,
            ctx,
            kinds: &self.kinds,
            parsers: &request_snapshot.parser_dispatcher,
            roots: &roots,
            registry_fingerprint: &request_snapshot.registry_fingerprint,
            trust_store: &request_snapshot.trust_store,
            node_trust_store: &self.node_trust_store,
            host_env: &self.host_env,
            project_authority: None,
            sealed_content,
        })
    }

    /// Compile a direct plan from an engine-verified subject carrier while
    /// reading the root program only from already-captured bytes. Executor
    /// chain artifacts are still resolved and sealed under the current
    /// installed bundle generation; the captured root pathname is audit data,
    /// not a recovery input.
    pub fn build_plan_from_captured_root(
        &self,
        ctx: &PlanContext,
        item: &VerifiedItem,
        root_source: &str,
        parameters: &Value,
        hints: &ExecutionHints,
        sealed_content: Option<&dyn crate::project_content::SealedDependencyBytes>,
    ) -> Result<ExecutionPlan, EngineError> {
        self.checked_bundle_generation(|| {
            crate::scope::check_execution_scope(&ctx.requested_by)?;
            let project_root = match &ctx.project_context {
                crate::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
                _ => None,
            };
            let roots = self.resolution_roots(project_root.clone());
            let request_snapshot = self.effective_request_snapshot_current(
                project_root.as_deref(),
                &ctx.subject_resolution_authority,
            )?;
            crate::plan_builder::build_plan(crate::plan_builder::BuildPlanInput {
                item,
                root_source: Some(root_source),
                parameters,
                hints,
                ctx,
                kinds: &self.kinds,
                parsers: &request_snapshot.parser_dispatcher,
                roots: &roots,
                registry_fingerprint: &request_snapshot.registry_fingerprint,
                trust_store: &request_snapshot.trust_store,
                node_trust_store: &self.node_trust_store,
                host_env: &self.host_env,
                project_authority: None,
                sealed_content,
            })
        })
    }

    /// Build an execution plan whose root, executor chain, project config, and
    /// precedence probes are all sourced from one admitted project-content
    /// authority. `sealed_content`, when present, overrides that authority for
    /// dependency paths an admitted realization covers.
    pub fn build_plan_under_admitted_authority(
        &self,
        ctx: &PlanContext,
        item: &VerifiedItem,
        parameters: &Value,
        hints: &ExecutionHints,
        project_root: &Path,
        admitted: &AdmittedRequestAuthoritySnapshot,
        sealed_content: Option<&dyn crate::project_content::SealedDependencyBytes>,
    ) -> Result<ExecutionPlan, EngineError> {
        self.checked_bundle_generation(|| {
            crate::scope::check_execution_scope(&ctx.requested_by)?;
            admitted.validate_root_binding(project_root)?;
            if item.resolved.subject_resolution_authority != ctx.subject_resolution_authority {
                return Err(EngineError::Internal(
                    "verified plan root carries different admitted subject authority".to_string(),
                ));
            }
            if item.resolved.source_space == crate::contracts::ItemSpace::Project
                && !admitted.validate_project_file_for_root(
                    project_root,
                    &item.resolved.source_path,
                    &item.resolved.content_hash,
                )?
            {
                return Err(EngineError::Internal(
                    "verified plan root differs from admitted project content".to_string(),
                ));
            }
            for absence in item
                .resolved
                .probed_absent
                .iter()
                .filter(|absence| absence.space == crate::contracts::ItemSpace::Project)
            {
                if !admitted.validate_project_absence_for_root(project_root, &absence.path)? {
                    return Err(EngineError::Internal(format!(
                        "verified plan root absence {} differs from admitted project content",
                        absence.path.display()
                    )));
                }
            }
            let request_snapshot =
                self.effective_request_snapshot_under_admitted_authority(project_root, admitted)?;
            let roots = self.resolution_roots(Some(project_root.to_path_buf()));
            let project_content = admitted.project_content_for_root(project_root)?;
            crate::plan_builder::build_plan(crate::plan_builder::BuildPlanInput {
                item,
                root_source: None,
                parameters,
                hints,
                ctx,
                kinds: &self.kinds,
                parsers: &request_snapshot.parser_dispatcher,
                roots: &roots,
                registry_fingerprint: &request_snapshot.registry_fingerprint,
                trust_store: &request_snapshot.trust_store,
                node_trust_store: &self.node_trust_store,
                host_env: &self.host_env,
                project_authority: Some((project_root, project_content)),
                sealed_content,
            })
        })
    }

    /// Resolve which execution routine a root item's executor chain terminal
    /// selects, without building a subprocess plan.
    ///
    /// The dispatcher uses this to branch subprocess vs method-dispatch on the
    /// terminal's typed `terminal_executor:` descriptor (never on the alias
    /// name or terminal ref). Acquires the same per-request roots / effective
    /// parsers / trust store as `build_plan`.
    pub fn resolve_terminal_executor(
        &self,
        root_source_path: &std::path::Path,
        root_executor_id: &str,
        root_kind: &str,
        project_root: Option<PathBuf>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<crate::plan_builder::ResolvedTerminalExecutor, EngineError> {
        self.checked_bundle_generation(|| {
            self.resolve_terminal_executor_current(
                root_source_path,
                root_executor_id,
                root_kind,
                project_root,
                subject_resolution_authority,
            )
        })
    }

    /// Resolve the typed executor terminal under admitted project content.
    pub fn resolve_terminal_executor_under_admitted_authority(
        &self,
        root_source_path: &Path,
        root_executor_id: &str,
        root_kind: &str,
        project_root: &Path,
        admitted: &AdmittedRequestAuthoritySnapshot,
    ) -> Result<crate::plan_builder::ResolvedTerminalExecutor, EngineError> {
        self.checked_bundle_generation(|| {
            admitted.validate_root_binding(project_root)?;
            let request_snapshot =
                self.effective_request_snapshot_under_admitted_authority(project_root, admitted)?;
            let roots = self.resolution_roots(Some(project_root.to_path_buf()));
            let project_content = admitted.project_content_for_root(project_root)?;
            crate::plan_builder::resolve_terminal_executor_under_project_authority(
                root_executor_id,
                root_source_path,
                root_kind,
                &self.kinds,
                &request_snapshot.parser_dispatcher,
                &roots,
                &request_snapshot.trust_store,
                project_root,
                project_content,
            )
        })
    }

    fn resolve_terminal_executor_current(
        &self,
        root_source_path: &std::path::Path,
        root_executor_id: &str,
        root_kind: &str,
        project_root: Option<PathBuf>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<crate::plan_builder::ResolvedTerminalExecutor, EngineError> {
        let roots = self.resolution_roots(project_root.clone());
        let request_snapshot = self.effective_request_snapshot_current(
            project_root.as_deref(),
            subject_resolution_authority,
        )?;
        crate::plan_builder::resolve_terminal_executor(
            root_executor_id,
            root_source_path,
            root_kind,
            &self.kinds,
            &request_snapshot.parser_dispatcher,
            &roots,
            &request_snapshot.trust_store,
        )
    }

    /// Execute a plan via Lillux subprocess dispatch.
    pub fn execute_plan(
        &self,
        ctx: &EngineContext,
        plan: ExecutionPlan,
    ) -> Result<ExecutionCompletion, EngineError> {
        self.checked_bundle_generation(|| {
            tracing::debug!(plan_id = %plan.plan_id, "executing plan");
            let result = crate::dispatch::execute_plan(&plan, ctx);
            if let Ok(ref completion) = result {
                tracing::info!(plan_id = %plan.plan_id, status = ?completion.status, "plan execution completed");
            }
            result
        })
    }

    /// Spawn a plan's subprocess without waiting.
    /// Returns a handle the daemon can use to persist pid/pgid before waiting.
    pub fn spawn_plan(
        &self,
        ctx: &EngineContext,
        plan: &ExecutionPlan,
    ) -> Result<crate::dispatch::SpawnedExecutionAwaitingAttachment, EngineError> {
        self.checked_bundle_generation(|| {
            tracing::debug!(plan_id = %plan.plan_id, "spawning plan");
            crate::dispatch::spawn_plan(plan, ctx)
        })
    }

    /// Build resolution roots for a given project root (project-first order).
    pub fn resolution_roots(&self, project_root: Option<PathBuf>) -> ResolutionRoots {
        if !self.registered_bundle_roots.is_empty() {
            return ResolutionRoots::from_registered(project_root, &self.registered_bundle_roots);
        }
        let system_ai: Vec<PathBuf> = self.bundle_roots.iter().map(|p| p.join(AI_DIR)).collect();
        let project_ai = project_root.map(|p| p.join(AI_DIR));
        ResolutionRoots::from_flat(project_ai, system_ai)
    }

    /// Add node-local configuration to launch-config lookup only. Keeping this
    /// separate prevents mutable node state from becoming a general item root.
    pub fn launch_config_roots(&self, roots: &ResolutionRoots) -> ResolutionRoots {
        let mut ordered = roots.ordered.clone();
        let Some(node_config_root) = &self.node_config_root else {
            return ResolutionRoots { ordered };
        };
        let node_config_ai_root = node_config_root.join(crate::AI_DIR);
        if ordered
            .iter()
            .any(|root| root.ai_root == node_config_ai_root)
        {
            return ResolutionRoots { ordered };
        }
        let position = ordered
            .iter()
            .position(|root| root.space == crate::contracts::ItemSpace::Bundle)
            .unwrap_or(ordered.len());
        ordered.insert(
            position,
            ResolutionRoot {
                space: crate::contracts::ItemSpace::Node,
                identity: crate::contracts::ItemSourceRoot::Node,
                label: "node-config".to_string(),
                ai_root: node_config_ai_root,
                content_root: Some(node_config_root.clone()),
            },
        );
        ResolutionRoots { ordered }
    }

    /// Composite cache fingerprint over the kind registry and the
    /// **boot-time** parser tool registry. Use
    /// `effective_registry_fingerprint(project_root, authority)` for
    /// per-request fingerprints that include the project's parser overlay.
    pub fn registry_fingerprint(&self) -> String {
        self.fingerprint_for(self.parser_dispatcher.parser_tools.fingerprint())
    }

    /// Per-request composite fingerprint that folds in the **effective**
    /// parser registry — i.e. the boot registry overlaid by the
    /// project's `.ai/parsers/`. Plan caches must key on this so a
    /// project-local parser change invalidates downstream entries.
    ///
    pub fn effective_registry_fingerprint(
        &self,
        project_root: Option<&Path>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<String, EngineError> {
        Ok(self
            .effective_request_snapshot(project_root, subject_resolution_authority)?
            .registry_fingerprint)
    }

    /// Compose the engine's composite fingerprint over the kind
    /// registry, the supplied parser-tools fingerprint, and the
    /// composer set. Pub-crate so callers (notably `build_plan`) can
    /// derive a fingerprint from a `ParserDispatcher` they already
    /// loaded — preserving the single-snapshot guarantee.
    pub(crate) fn fingerprint_for(&self, parser_tools_fp: &str) -> String {
        // Composers contribute a stable digest of their registered
        // kinds: changing the composer set must invalidate any cache
        // keyed off the fingerprint.
        let mut composer_kinds: Vec<&str> = self.composers.kinds().collect();
        composer_kinds.sort();
        let composer_fp = lillux::cas::sha256_hex(composer_kinds.join(",").as_bytes());
        let combined = format!(
            "{}|{}|{}",
            self.kinds.fingerprint(),
            parser_tools_fp,
            composer_fp,
        );
        lillux::cas::sha256_hex(combined.as_bytes())
    }

    /// Build the effective parser dispatcher for a request.
    ///
    /// Without a project root, returns a clone of the boot dispatcher
    /// (cheap — `ParserRegistry` is `HashMap`-cloning, the handler
    /// registry is held by `Arc`).
    ///
    /// With a project root, applies `with_project_overlay` against
    /// the project's `.ai/parsers/` so descriptors declared inside
    /// the project shadow base entries with the same canonical ref.
    pub fn effective_parser_dispatcher(
        &self,
        project_root: Option<&Path>,
        subject_resolution_authority: &SubjectResolutionAuthority,
    ) -> Result<ParserDispatcher, EngineError> {
        Ok(self
            .effective_request_snapshot(project_root, subject_resolution_authority)?
            .parser_dispatcher)
    }

    fn effective_parser_dispatcher_with_trust(
        &self,
        project_root: Option<&Path>,
        trust_store: &TrustStore,
    ) -> Result<ParserDispatcher, EngineError> {
        match project_root {
            None => Ok(self.parser_dispatcher.clone()),
            Some(path) => {
                // The `parser` kind is load-bearing: it tells the
                // overlay loader which directory to scan, which file
                // extensions to accept, and which signature envelope
                // to verify with. A manually-constructed engine that
                // forgot to register it would otherwise *silently*
                // lose its project overlays — turning a project's
                // `.ai/parsers/` into a no-op the moment a project
                // root is supplied. Fail loud instead so the
                // misconfiguration surfaces at the first
                // `resolve` / `build_plan` instead of as a confusing
                // "ParserNotRegistered" two layers down. Production
                // boots register the parser kind via
                // `KindRegistry::load_base`, so this only fires for
                // test fixtures and embeddings.
                if self.kinds.get("parser").is_none() {
                    return Err(EngineError::SchemaLoaderError {
                        reason: "parser kind schema not registered — \
                                 required for parser overlay loading"
                            .into(),
                    });
                }
                let overlay_root =
                    crate::parsers::ParserRegistry::project_overlay_root(path, &self.kinds)?;
                if !overlay_root.exists() {
                    tracing::debug!(
                        project_root = %path.display(),
                        rebuild_reason = "no_overlay",
                        "using base parser dispatcher"
                    );
                    return Ok(self.parser_dispatcher.clone());
                }

                let metadata =
                    crate::parser_overlay_cache::fingerprint_parser_overlay(&overlay_root)?;
                let base_trust = self
                    .request_trust_base
                    .as_ref()
                    .unwrap_or(&self.trust_store);
                let key = crate::parser_overlay_cache::ParserOverlayCacheKey {
                    project_root: path.to_path_buf(),
                    overlay_fingerprint: metadata.fingerprint,
                    effective_trust_fingerprint: trust_store.fingerprint(),
                    base_trust_fingerprint: base_trust.fingerprint(),
                    caller_overlay_identity: self.request_trust_overlay_identity.clone(),
                    generation_fingerprint: self.request_engine_generation_identity(),
                };
                self.parser_overlay_cache.get_or_build(
                    key,
                    metadata.cacheable,
                    metadata.total_file_bytes,
                    || {
                        let overlaid = self.parser_dispatcher.parser_tools.with_project_overlay(
                            path,
                            trust_store,
                            &self.kinds,
                        )?;
                        Ok(self.parser_dispatcher.with_parser_tools(overlaid))
                    },
                )
            }
        }
    }

    fn request_engine_generation_identity(&self) -> String {
        let mut generation = Vec::new();
        if let Some(identity) = self.isolation_generation.registered_generation_identity() {
            append_identity_field(&mut generation, &identity.to_le_bytes());
        }
        append_identity_field(&mut generation, self.registry_fingerprint().as_bytes());
        append_identity_field(
            &mut generation,
            self.parser_dispatcher.handler_cache_identity().as_bytes(),
        );
        for registered in &self.registered_bundle_roots {
            append_identity_field(&mut generation, registered.name.as_bytes());
            append_identity_field(
                &mut generation,
                registered.canonical_root.as_os_str().as_encoded_bytes(),
            );
        }
        lillux::cas::sha256_hex(&generation)
    }
}

fn append_identity_field(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
    bytes.extend_from_slice(field);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        EffectivePrincipal, ExecutionHints, ItemSpace, Principal, ProjectContext, TrustClass,
    };
    use crate::trust::{TrustStore, TrustedSigner};
    use base64::Engine as _;
    use lillux::crypto::SigningKey;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct CountingGenerationLifeline {
        begins: AtomicUsize,
        checks: AtomicUsize,
    }

    impl crate::isolation::IsolationGenerationLifeline for CountingGenerationLifeline {
        fn begin_operation(&self) -> Result<Box<dyn Send + Sync>, String> {
            self.begins.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(()))
        }

        fn ensure_current(&self) -> Result<(), String> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn test_trust_store() -> TrustStore {
        let sk = test_signing_key();
        let vk = sk.verifying_key();
        let fp = crate::trust::compute_fingerprint(&vk);
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp,
            verifying_key: vk,
            label: None,
        }])
    }

    fn sign_schema_yaml(yaml: &str) -> String {
        // composed_value_contract is now mandatory on every kind
        // schema; inject an empty mapping for tests that don't
        // exercise contract semantics.
        let yaml_owned = if yaml.contains("composed_value_contract") {
            yaml.to_string()
        } else {
            {
                let with_contract = format!(
                    "{yaml}composed_value_contract:\n  root_type: mapping\n  required: {{}}\n"
                );
                if with_contract.contains("composer:") {
                    with_contract
                } else {
                    format!("{with_contract}composer: handler:ryeos/core/identity\n")
                }
            }
        };
        let yaml_owned = if yaml_owned.contains("effective_trust:") {
            yaml_owned
        } else {
            format!("{yaml_owned}effective_trust:\n  include_references: false\n")
        };
        let yaml_owned = if yaml_owned.contains("resolution:") {
            yaml_owned
        } else {
            format!("{yaml_owned}resolution: []\n")
        };
        lillux::signature::sign_content(&yaml_owned, &test_signing_key(), "#", None)
    }

    const TOOL_SCHEMA_YAML: &str = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser: parser:ryeos/core/python/tool-header
    signature:
      prefix: \"#\"
      after_shebang: true
";

    fn write_signed_tool_schema(kinds_dir: &Path) {
        let tool_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("tool.kind-schema.yaml"),
            sign_schema_yaml(TOOL_SCHEMA_YAML),
        )
        .unwrap();
        // The `parser` kind is load-bearing for any engine that may
        // be asked to resolve with a project root: `Engine::
        // effective_parser_dispatcher` requires it. Co-write it here
        // so every test fixture that ships a tool schema also ships
        // the minimum kind set a real engine needs.
        write_signed_parser_kind_schema(kinds_dir);
    }

    fn test_engine() -> Engine {
        Engine::new(
            KindRegistry::empty(),
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
    }

    fn test_plan_context() -> PlanContext {
        PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        }
    }

    fn immutable_request_fixture() -> Arc<EffectiveRequestSnapshot> {
        Arc::new(EffectiveRequestSnapshot {
            trust_store: TrustStore::empty(),
            parser_dispatcher: crate::parsers::dispatcher::ParserDispatcher::new(
                crate::parsers::registry::ParserRegistry::empty(),
                Arc::new(crate::handlers::registry::HandlerRegistry::empty()),
            ),
            registry_fingerprint: "registry".to_string(),
            effective_trust_identity: "trust".to_string(),
            request_engine_generation_identity: "engine".to_string(),
            subject_resolution_authority: SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: "a".repeat(64),
            },
        })
    }

    #[test]
    fn immutable_request_pending_publishes_one_shared_result_and_cleans_up() {
        let cache = Arc::new(Mutex::new(ImmutableRequestSnapshotCache::default()));
        let pending = Arc::new(ImmutableRequestSnapshotPending::default());
        cache
            .lock()
            .unwrap()
            .pending
            .insert("key".to_string(), Arc::clone(&pending));
        let request = immutable_request_fixture();
        let guard = ImmutableRequestSnapshotFillGuard {
            cache: Arc::clone(&cache),
            key: "key".to_string(),
            pending: Arc::clone(&pending),
            completed: false,
        };
        guard.finish(Arc::clone(&request));
        assert!(cache.lock().unwrap().pending.is_empty());
        let published = pending
            .result
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|result| result.as_ref().ok().cloned())
            .unwrap();
        assert!(Arc::ptr_eq(&request, &published));
    }

    #[test]
    fn immutable_request_single_flight_wakes_a_concurrent_waiter_with_the_exact_result() {
        let cache = Arc::new(Mutex::new(ImmutableRequestSnapshotCache::default()));
        let pending = Arc::new(ImmutableRequestSnapshotPending::default());
        cache
            .lock()
            .unwrap()
            .pending
            .insert("concurrent-key".to_string(), Arc::clone(&pending));
        let waiter_pending = Arc::clone(&pending);
        let waiter = std::thread::spawn(move || {
            let mut completed = waiter_pending.result.lock().unwrap();
            while completed.is_none() {
                completed = waiter_pending.ready.wait(completed).unwrap();
            }
            completed.as_ref().unwrap().as_ref().unwrap().clone()
        });
        let request = immutable_request_fixture();
        ImmutableRequestSnapshotFillGuard {
            cache: Arc::clone(&cache),
            key: "concurrent-key".to_string(),
            pending,
            completed: false,
        }
        .finish(Arc::clone(&request));
        let waited = waiter.join().unwrap();
        assert!(Arc::ptr_eq(&request, &waited));
        assert!(cache.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn static_verification_single_flight_wakes_a_concurrent_waiter_with_exact_evidence() {
        let key = format!(
            "static-concurrent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let pending = Arc::new(StaticVerificationPending::default());
        static_verification_cache()
            .lock()
            .unwrap()
            .pending
            .insert(key.clone(), Arc::clone(&pending));
        let waiter_pending = Arc::clone(&pending);
        let waiter = std::thread::spawn(move || {
            let mut completed = waiter_pending.result.lock().unwrap();
            while completed.is_none() {
                completed = waiter_pending.ready.wait(completed).unwrap();
            }
            completed.as_ref().unwrap().as_ref().unwrap().clone()
        });
        let evidence = Arc::new(StaticVerificationEvidence {
            signer: None,
            trust_class: TrustClass::Unsigned,
            pinned_version: None,
        });
        StaticVerificationFillGuard {
            key,
            pending,
            completed: false,
        }
        .finish(Arc::clone(&evidence));
        let waited = waiter.join().unwrap();
        assert!(Arc::ptr_eq(&evidence, &waited));
    }

    #[test]
    fn immutable_request_failed_fill_publishes_one_shared_error() {
        let cache = Arc::new(Mutex::new(ImmutableRequestSnapshotCache::default()));
        let pending = Arc::new(ImmutableRequestSnapshotPending::default());
        cache
            .lock()
            .unwrap()
            .pending
            .insert("key".to_string(), Arc::clone(&pending));
        drop(ImmutableRequestSnapshotFillGuard {
            cache: Arc::clone(&cache),
            key: "key".to_string(),
            pending: Arc::clone(&pending),
            completed: false,
        });
        assert!(cache.lock().unwrap().pending.is_empty());
        let result = pending.result.lock().unwrap();
        let Some(Err(first)) = result.as_ref() else {
            panic!("failed fill must publish an error");
        };
        let second = first.clone();
        assert!(Arc::ptr_eq(first, &second));
    }

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir =
            std::env::temp_dir().join(format!("rye_engine_test_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn engine_construction() {
        let engine = test_engine();
        // The composite fingerprint is sha256(kinds_fp | parser_tools_fp);
        // both inputs are deterministic so the fingerprint must be
        // non-empty and stable across runs.
        let fp = engine.registry_fingerprint();
        assert!(!fp.is_empty());
        assert_eq!(fp, test_engine().registry_fingerprint());
    }

    #[test]
    fn node_config_root_is_added_only_to_launch_config_precedence() {
        let engine = test_engine().with_node_config_root(PathBuf::from("/node-config"));
        let ordinary = engine.resolution_roots(Some(PathBuf::from("/project")));
        assert_eq!(ordinary.ordered.len(), engine.bundle_roots.len() + 1);
        assert!(
            !ordinary
                .ordered
                .iter()
                .any(|root| root.ai_root == Path::new("/node-config/.ai"))
        );
        assert!(
            !ordinary
                .ordered
                .iter()
                .any(|root| root.space == crate::contracts::ItemSpace::Node)
        );

        let launch = engine.launch_config_roots(&ordinary);
        assert_eq!(launch.ordered[0].label, "project");
        assert_eq!(launch.ordered[1].label, "node-config");
        assert_eq!(launch.ordered[1].space, crate::contracts::ItemSpace::Node);
    }

    #[test]
    fn checked_generation_batches_multiple_resolutions_under_one_guard() {
        let lifeline = std::sync::Arc::new(CountingGenerationLifeline::default());
        let isolation = crate::isolation::IsolationRuntime::disabled_for_authoring()
            .retain_registered_generation(lifeline.clone(), TrustStore::empty(), vec![]);
        let engine = test_engine().with_isolation_generation(std::sync::Arc::new(isolation));
        let ctx = test_plan_context();
        let item_ref = CanonicalRef::parse("tool:missing").unwrap();

        engine
            .with_checked_bundle_generation(|generation| -> Result<(), EngineError> {
                let results = generation.resolve_many(&ctx, &[item_ref.clone(), item_ref]);
                assert_eq!(results.len(), 2);
                assert!(results.into_iter().all(|result| result.is_err()));
                Ok(())
            })
            .unwrap();

        assert_eq!(lifeline.begins.load(Ordering::SeqCst), 1);
        assert_eq!(lifeline.checks.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retained_generation_identity_does_not_alias_same_registered_paths() {
        let roots = vec![crate::item_resolution::RegisteredBundleRoot {
            name: "same-name".to_string(),
            canonical_root: PathBuf::from("/same/canonical/root"),
        }];
        let first = crate::isolation::IsolationRuntime::disabled_for_authoring()
            .retain_registered_generation(
                std::sync::Arc::new(CountingGenerationLifeline::default()),
                TrustStore::empty(),
                roots.clone(),
            );
        let second = crate::isolation::IsolationRuntime::disabled_for_authoring()
            .retain_registered_generation(
                std::sync::Arc::new(CountingGenerationLifeline::default()),
                TrustStore::empty(),
                roots.clone(),
            );
        let first = test_engine()
            .with_registered_bundle_roots(roots.clone())
            .with_isolation_generation(std::sync::Arc::new(first));
        let second = test_engine()
            .with_registered_bundle_roots(roots)
            .with_isolation_generation(std::sync::Arc::new(second));

        assert_ne!(
            first.registered_bundle_generation_fingerprint(),
            second.registered_bundle_generation_fingerprint(),
        );
    }

    #[test]
    fn resolve_rejects_unknown_kind() {
        let engine = test_engine();
        let ctx = test_plan_context();
        let r = CanonicalRef::parse("tool:ryeos/bash/bash").unwrap();
        let err = engine.resolve(&ctx, &r).unwrap_err();
        assert!(
            matches!(err, EngineError::UnsupportedKind { ref kind } if kind == "tool"),
            "expected UnsupportedKind, got: {err:?}"
        );
    }

    #[test]
    fn resolution_roots_with_project() {
        let engine = test_engine();
        let roots = engine.resolution_roots(Some(PathBuf::from("/workspace/project")));
        assert!(roots.ordered.iter().any(|r| r.space == ItemSpace::Project));
        let project_root = roots
            .ordered
            .iter()
            .find(|r| r.space == ItemSpace::Project)
            .unwrap();
        assert_eq!(
            project_root.ai_root,
            PathBuf::from("/workspace/project/.ai")
        );
    }

    #[test]
    fn registered_resolution_roots_keep_project_ai_root_first() {
        let engine = test_engine().with_registered_bundle_roots(vec![
            crate::item_resolution::RegisteredBundleRoot {
                name: "core".to_owned(),
                canonical_root: PathBuf::from("/bundles/core"),
            },
        ]);
        let roots = engine.resolution_roots(Some(PathBuf::from("/workspace/project")));

        assert_eq!(roots.ordered.len(), 2);
        assert_eq!(roots.ordered[0].space, ItemSpace::Project);
        assert_eq!(roots.ordered[0].label, "project");
        assert_eq!(
            roots.ordered[0].ai_root,
            PathBuf::from("/workspace/project/.ai")
        );
        assert_eq!(roots.ordered[1].space, ItemSpace::Bundle);
        assert_eq!(roots.ordered[1].label, "bundle:core");
        assert_eq!(roots.ordered[1].ai_root, PathBuf::from("/bundles/core/.ai"));
    }

    #[test]
    fn resolution_roots_without_project() {
        let engine = test_engine();
        let roots = engine.resolution_roots(None);
        assert!(!roots.ordered.iter().any(|r| r.space == ItemSpace::Project));
    }

    #[test]
    fn resolve_finds_item() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("hello.py"),
            "# ryeos:signed:2026-04-10T00:00:00Z:abc123:sigdata:fp_test\n# ryeos-tool:\n#   note: hello\nprint('hello')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir.clone(),
            },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();

        assert_eq!(resolved.kind, "tool");
        assert_eq!(resolved.source_space, ItemSpace::Project);
        assert_eq!(resolved.source_format.extension, ".py");
        assert_eq!(
            resolved.source_format.parser,
            "parser:ryeos/core/python/tool-header"
        );
        assert!(resolved.signature_header.is_some());
        let sig = resolved.signature_header.unwrap();
        assert_eq!(sig.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(sig.content_hash, "abc123");
        assert_eq!(sig.signer_fingerprint, "fp_test");
        assert_eq!(resolved.materialized_project_root, Some(project_dir));
        assert!(!resolved.content_hash.is_empty());
        assert_eq!(
            resolved.raw_content_digest,
            crate::item_resolution::content_hash(
                "# ryeos-tool:\n#   note: hello\nprint('hello')\n"
            )
        );
        assert_ne!(resolved.raw_content_digest, resolved.content_hash);
    }

    fn signed_tool_content(
        body: &str,
        signing_key: &lillux::crypto::SigningKey,
        fingerprint: &str,
    ) -> String {
        use lillux::crypto::Signer;
        use sha2::{Digest, Sha256};

        let hash = {
            let h = Sha256::digest(body.as_bytes());
            let mut out = String::with_capacity(64);
            for byte in h.iter() {
                use std::fmt::Write;
                let _ = write!(&mut out, "{byte:02x}");
            }
            out
        };
        let sig: lillux::crypto::Signature = signing_key.sign(hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        format!("# ryeos:signed:2026-04-10T00:00:00Z:{hash}:{sig_b64}:{fingerprint}\n{body}")
    }

    #[test]
    fn resolve_then_verify_trusted() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let signing_key = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fp = crate::trust::compute_fingerprint(&verifying_key);

        let body = "# ryeos-tool:\n#   note: hello\nprint('hello')\n";
        let content = signed_tool_content(body, &signing_key, &fp);
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.py"), &content).unwrap();

        let trust_store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key,
            label: None,
        }]);

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(trust_store);

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();

        assert_eq!(verified.trust_class, TrustClass::Trusted);
        assert_eq!(verified.signer.as_ref().unwrap().0, fp);
    }

    #[test]
    fn resolve_then_verify_unsigned() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: hello\nprint('hello')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();

        assert_eq!(verified.trust_class, TrustClass::Unsigned);
        assert!(verified.signer.is_none());
    }

    #[test]
    fn static_verification_cache_rehashes_live_source_before_hit() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        let tool_path = tool_dir.join("hello.py");
        fs::write(
            &tool_path,
            "# ryeos-tool:\n#   note: hello\nprint('hello')\n",
        )
        .unwrap();
        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        );
        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };
        let resolved = engine
            .resolve(&ctx, &CanonicalRef::parse("tool:hello").unwrap())
            .unwrap();
        engine.verify(&ctx, resolved.clone()).unwrap();
        fs::write(
            &tool_path,
            "# ryeos-tool:\n#   note: replaced\nprint('changed')\n",
        )
        .unwrap();
        assert!(matches!(
            engine.verify(&ctx, resolved),
            Err(EngineError::ContentHashMismatch { .. })
        ));
    }

    #[test]
    fn verified_attestation_refuses_different_subject_authority() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: hello\nprint('hello')\n",
        )
        .unwrap();
        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        );
        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };
        let resolved = engine
            .resolve(&ctx, &CanonicalRef::parse("tool:hello").unwrap())
            .unwrap();
        let attestation = engine.verify_attested(&ctx, resolved).unwrap();
        assert!(
            engine
                .consume_verified_attestation(
                    &ctx,
                    &attestation,
                    &SubjectResolutionAuthority::PinnedGeneration {
                        snapshot_hash: "a".repeat(64),
                    },
                )
                .is_err()
        );

        let mut substituted_context = ctx.clone();
        substituted_context.subject_resolution_authority =
            SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: "a".repeat(64),
            };
        assert!(
            engine
                .consume_verified_attestation(
                    &substituted_context,
                    &attestation,
                    &SubjectResolutionAuthority::LiveFs,
                )
                .is_err()
        );
    }

    #[test]
    fn verified_attestation_refuses_caller_modified_resolution_metadata() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: hello\nprint('hello')\n",
        )
        .unwrap();
        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        );
        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };
        let mut resolved = engine
            .resolve(&ctx, &CanonicalRef::parse("tool:hello").unwrap())
            .unwrap();
        resolved
            .metadata
            .extra
            .insert("fabricated".to_owned(), serde_json::json!(true));
        assert!(engine.verify_attested(&ctx, resolved).is_err());
    }

    #[test]
    fn resolve_then_verify_untrusted_signer() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let signing_key = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let fp = crate::trust::compute_fingerprint(&signing_key.verifying_key());

        let body = "# ryeos-tool:\n#   note: hello\nprint('hello')\n";
        let content = signed_tool_content(body, &signing_key, &fp);
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.py"), &content).unwrap();

        // Engine with EMPTY trust store
        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();

        assert_eq!(verified.trust_class, TrustClass::Untrusted);
        assert_eq!(verified.signer.as_ref().unwrap().0, fp);
    }

    #[test]
    fn resolve_ignores_project_kind_overlay() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Project overlay: tool → .yaml only — should be IGNORED
        let overlay_dir = project_dir
            .join(crate::AI_DIR)
            .join(crate::KIND_SCHEMAS_DIR)
            .join("tool");
        fs::create_dir_all(&overlay_dir).unwrap();
        let overlay_yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".yaml\"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: \"#\"
";
        fs::write(
            overlay_dir.join("tool.kind-schema.yaml"),
            sign_schema_yaml(overlay_yaml),
        )
        .unwrap();

        // Write a .py tool file (should resolve because system schema has .py)
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: hello\nprint('hello')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(ts);

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir.clone(),
            },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        // .py file should resolve (system schema, not project overlay)
        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();
        assert_eq!(resolved.source_format.extension, ".py");
        assert_eq!(
            resolved.source_format.parser,
            "parser:ryeos/core/python/tool-header"
        );
    }

    #[test]
    fn resolve_project_first_with_clash() {
        let project_dir = tempdir();
        let system_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Write the same item in both system and project
        let sys_tool_dir = system_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&sys_tool_dir).unwrap();
        fs::write(
            sys_tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: system\nprint('sys')\n",
        )
        .unwrap();

        let proj_tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&proj_tool_dir).unwrap();
        fs::write(
            proj_tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: project\nprint('proj')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![system_dir],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();

        // Project wins over bundles.
        assert_eq!(resolved.source_space, ItemSpace::Project);
        assert_eq!(resolved.resolved_from, "project");

        // Bundle copy is shadowed.
        assert_eq!(resolved.shadowed.len(), 1);
        assert_eq!(resolved.shadowed[0].space, ItemSpace::Bundle);
    }

    /// Without a project root, the effective dispatcher MUST be
    /// equivalent to the boot dispatcher — same parser tool registry,
    /// same fingerprint. The whole point of the per-request seam is
    /// that overlays cost nothing when there's no project to overlay.
    #[test]
    fn effective_dispatcher_no_project_root_returns_boot_clone() {
        let engine = test_engine();
        let effective = engine
            .effective_parser_dispatcher(None, &SubjectResolutionAuthority::Projectless)
            .unwrap();
        assert_eq!(
            effective.parser_tools.fingerprint(),
            engine.parser_dispatcher.parser_tools.fingerprint(),
            "no-project effective dispatcher must mirror boot fingerprint"
        );
        assert_eq!(
            engine
                .effective_registry_fingerprint(None, &SubjectResolutionAuthority::Projectless,)
                .unwrap(),
            engine.registry_fingerprint(),
            "no-project effective composite fingerprint must equal boot fingerprint"
        );
    }

    #[test]
    fn project_scoped_engine_does_not_retain_deleted_project_trust() {
        let project_dir = tempdir();
        let trusted_dir = project_dir.join(crate::AI_DIR).join(crate::TRUST_KEYS_DIR);
        fs::create_dir_all(&trusted_dir).unwrap();

        let signing_key = SigningKey::from_bytes(&[77u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = crate::trust::compute_fingerprint(&verifying_key);
        let encoded = base64::engine::general_purpose::STANDARD.encode(verifying_key.as_bytes());
        let key_path = trusted_dir.join("project.pub");
        fs::write(&key_path, encoded).unwrap();

        let scoped = test_engine().for_project_root(&project_dir, None).unwrap();
        assert!(scoped.trust_store.is_trusted(&fingerprint));

        fs::remove_file(key_path).unwrap();
        let current = scoped.effective_trust_store(Some(&project_dir)).unwrap();
        assert!(
            !current.is_trusted(&fingerprint),
            "project trust removals must be visible to the next request"
        );
    }

    const PARSER_KIND_SCHEMA: &str = "\
location:
  directory: parsers
formats:
  - extensions: [\".yaml\"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: \"#\"
";

    fn write_signed_parser_kind_schema(kinds_dir: &Path) {
        let parser_dir = kinds_dir.join("parser");
        fs::create_dir_all(&parser_dir).unwrap();
        fs::write(
            parser_dir.join("parser.kind-schema.yaml"),
            sign_schema_yaml(PARSER_KIND_SCHEMA),
        )
        .unwrap();
    }

    /// Tool kind schema that points at a parser ref the test builtins
    /// do NOT register — only the project overlay supplies it. If
    /// resolution went through the boot dispatcher, parsing would
    /// fail with `ParserNotRegistered`. If it goes through the
    /// effective dispatcher, the overlay rescues the parse.
    const TOOL_SCHEMA_USING_PROJECT_PARSER: &str = "\
location:
  directory: tools
formats:
  - extensions: [\".pyx\"]
    parser: parser:proj/only
    signature:
      prefix: \"#\"
";

    fn write_signed_parser_descriptor(project_dir: &Path, rel_id: &str, yaml: &str) {
        let path = project_dir
            .join(crate::AI_DIR)
            .join("parsers")
            .join(format!("{rel_id}.yaml"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Parser descriptors require `output_schema`. Inject empty
        // mapping if not present so existing test fixtures that
        // don't exercise contract semantics keep working.
        let yaml_owned = if yaml.contains("output_schema") {
            yaml.to_string()
        } else {
            format!("{yaml}output_schema:\n  root_type: mapping\n  required: {{}}\n")
        };
        // sign_schema_yaml also injects composed_value_contract for
        // KIND schemas; that's harmless for descriptors since the
        // descriptor parser uses `deny_unknown_fields` only on its
        // own struct, and this body is appended as a top-level field
        // — in practice all tests that use this helper write
        // descriptors not kind schemas, so the contract injection
        // would actually corrupt them. Sign directly.
        let signed = lillux::signature::sign_content(&yaml_owned, &test_signing_key(), "#", None);
        fs::write(path, signed).unwrap();
    }

    /// A project's `.ai/parsers/` MUST surface in the per-request
    /// effective fingerprint — otherwise plan caches keyed off the
    /// boot fingerprint would silently serve stale results when a
    /// project ships its own parser overlay.
    #[test]
    fn effective_dispatcher_with_project_root_includes_overlay() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_parser_kind_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(ts);

        let boot_fp = engine.registry_fingerprint();
        let no_project_fp = engine
            .effective_registry_fingerprint(None, &SubjectResolutionAuthority::Projectless)
            .unwrap();
        assert_eq!(boot_fp, no_project_fp);

        // Project ships a parser descriptor that shadows
        // `parser:ryeos/core/yaml/yaml`. Even though the descriptor
        // body is identical in shape to the test built-in, the
        // serialized bytes differ (different version field), so the
        // overlay MUST change the registry fingerprint.
        write_signed_parser_descriptor(
            &project_dir,
            "ryeos/core/yaml/yaml",
            "version: \"9.9.9-project-overlay\"\n\
             handler: \"handler:ryeos/core/yaml-document\"\n\
             parser_api_version: 1\n\
             parser_config: {}\n",
        );

        let with_project_fp = engine
            .effective_registry_fingerprint(Some(&project_dir), &SubjectResolutionAuthority::LiveFs)
            .expect("effective fingerprint with project root");

        assert_ne!(
            boot_fp, with_project_fp,
            "project overlay MUST shift the per-request fingerprint; \
             plan caches would otherwise serve stale results. \
             boot={boot_fp} project={with_project_fp}"
        );

        // And the dispatcher itself MUST carry the overlay's
        // descriptor — same canonical ref, project's version string.
        let effective = engine
            .effective_parser_dispatcher(Some(&project_dir), &SubjectResolutionAuthority::LiveFs)
            .unwrap();
        let descriptor = effective
            .parser_tools
            .get("parser:ryeos/core/yaml/yaml")
            .expect("project overlay descriptor present in effective dispatcher");
        assert_eq!(
            descriptor.version, "9.9.9-project-overlay",
            "effective dispatcher must serve the project's overlaid descriptor, \
             not the boot version"
        );
    }

    /// End-to-end: `engine.resolve()` MUST go through the per-request
    /// effective dispatcher. The system tool kind cites a parser ref
    /// (`parser:proj/only`) that the boot dispatcher does NOT register
    /// — only the project's `.ai/parsers/` overlay supplies it. If
    /// resolve still hit the boot dispatcher this test would fail
    /// with `ParserNotRegistered`.
    #[test]
    fn engine_resolve_uses_project_overlay_parser() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_parser_kind_schema(&kinds_dir);

        // Tool kind schema that names a parser only the project supplies.
        let tool_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("tool.kind-schema.yaml"),
            sign_schema_yaml(TOOL_SCHEMA_USING_PROJECT_PARSER),
        )
        .unwrap();

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Project-local parser descriptor — the only place
        // `parser:proj/only` is defined. Re-uses the yaml_document
        // native handler so we don't have to register a new one.
        write_signed_parser_descriptor(
            &project_dir,
            "proj/only",
            "version: \"1.0.0\"\n\
             handler: \"handler:ryeos/core/yaml-document\"\n\
             parser_api_version: 1\n\
             parser_config:\n  require_mapping: true\n",
        );

        // Tool file the engine will resolve. The body is valid YAML
        // (the proj/only parser is a yaml_document handler), so the
        // parse succeeds iff the overlay's descriptor is resolved.
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.pyx"), "name: hello\n").unwrap();

        // Empty-handler boot dispatcher would crash on parser lookup
        // even with the overlay if effective dispatcher wasn't used —
        // but the canonical-bundle test dispatcher provides handlers,
        // so the overlay just supplies the descriptor.
        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(ts);

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir.clone(),
            },
            subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine
            .resolve(&ctx, &ref_)
            .expect("resolve must succeed via project overlay parser");
        assert_eq!(resolved.source_format.parser, "parser:proj/only");
        assert_eq!(resolved.source_format.extension, ".pyx");
    }

    /// The request snapshot must carry a cache fingerprint derived from the
    /// exact dispatcher it carries. `build_plan` consumes this same object, so
    /// parser behaviour, parser identity, and trust cannot come from separate
    /// overlay reads.
    #[test]
    fn effective_snapshot_fingerprint_matches_its_dispatcher() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Project ships a parser overlay so the effective fingerprint
        // genuinely diverges from boot — otherwise the structural
        // identity would still hold but the test would be trivial.
        write_signed_parser_descriptor(
            &project_dir,
            "ryeos/core/yaml/yaml",
            "version: \"7.7.7-snapshot-test\"\n\
             handler: \"handler:ryeos/core/yaml-document\"\n\
             parser_api_version: 1\n\
             parser_config: {}\n",
        );

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(ts);

        let snapshot = engine
            .effective_request_snapshot(Some(&project_dir), &SubjectResolutionAuthority::LiveFs)
            .expect("effective request snapshot loads");
        let via_dispatcher =
            engine.fingerprint_for(snapshot.parser_dispatcher.parser_tools.fingerprint());

        assert_eq!(
            snapshot.registry_fingerprint, via_dispatcher,
            "the request snapshot fingerprint must describe the dispatcher \
             in that same snapshot"
        );

        // Test setup sanity: the overlay must actually shift the
        // fingerprint, otherwise the equality above is vacuous.
        assert_ne!(
            snapshot.registry_fingerprint,
            engine.registry_fingerprint(),
            "test setup must produce a non-trivial overlay shift"
        );
    }

    #[test]
    fn effective_snapshot_rejects_root_authority_substitution() {
        let engine = test_engine();
        let project_dir = tempdir();

        assert!(
            engine
                .effective_request_snapshot(
                    Some(&project_dir),
                    &SubjectResolutionAuthority::Projectless,
                )
                .is_err()
        );
        assert!(
            engine
                .effective_request_snapshot(None, &SubjectResolutionAuthority::LiveFs)
                .is_err()
        );
    }
}
