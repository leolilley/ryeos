//! Admission of signed external-content declarations into retained state.
//!
//! This daemon-owned layer resolves admitted named roots and node policy. File
//! traversal and manifest construction remain meaning-blind state mechanics;
//! executors receive only the retained realization and never a live locator.

use std::path::Path;

use ryeos_engine::contracts::ItemSpace;
use ryeos_engine::external_content::{
    ExternalContentDeclaration, ExternalContentKind, ExternalContentRoot,
};
use ryeos_engine::external_realization::{
    ExternalRealizationProof, RealizationStore, RealizedExternalContent, RealizedExternalContentSet,
};
use ryeos_state::{
    DigestOnlyExternalContentSink, ExternalCapturePolicy, ExternalContentBlobSink,
    ExternalContentCaptureKind, LaunchCaptureBudget, MAX_CAPTURE_FILE_BYTES, PendingCasPublication,
};
use serde::Serialize;

use crate::state::AppState;

/// Admission evidence retained until the finalized launch capsule becomes the
/// authoritative durable root.
pub struct AdmittedExternalRealizations {
    proof: ExternalRealizationProof,
    store: ExternalRealizationStore,
    publication: Option<PendingCasPublication>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalContentPinPreview {
    pub id: String,
    pub expected_digest: Option<String>,
    pub observed_digest: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalContentValidationPreview {
    pub declarations: Vec<ExternalContentPinPreview>,
    pub ready_for_admission: bool,
}

/// Observe the exact manifests a structurally valid declaration would pin.
/// This is validation-only: no object, blob, binding, or launch authority is
/// published. Strict admission continues to reject pending or mismatched pins.
pub fn preview_external_content_pins(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
) -> anyhow::Result<Option<ExternalContentValidationPreview>> {
    let contract = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.external_content.as_ref());
    let declarer = ryeos_engine::external_content::declaring_authority(resolution)?;
    let Some(declarations) = ryeos_engine::external_content::declarations_from_composed(
        &resolution.composed.composed,
        contract,
        declarer,
    )?
    else {
        return Ok(None);
    };

    let mut budget = LaunchCaptureBudget::default();
    let mut sink = DigestOnlyExternalContentSink;
    let mut previews = Vec::with_capacity(declarations.len());
    let mut ready_for_admission = true;
    for declaration in declarations {
        let observed_digest = match declaration.locator.as_ref() {
            Some(locator) => {
                let base_path = resolve_named_root(engine, roots, &locator.root)?;
                let base = lillux::PinnedDirectory::open(&base_path)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "external content root `{}` is unavailable",
                        locator.root.label()
                    )
                })?;
                let policy = ExternalCapturePolicy::new(
                    locator.path.clone(),
                    state.ignore_matcher.as_ref(),
                )?;
                let manifest = ryeos_state::capture_external_content_at(
                    &base,
                    &locator.path,
                    capture_kind(declaration.kind),
                    &declaration.exclude,
                    &policy,
                    &mut budget,
                    &mut sink,
                )?;
                ryeos_state::external_content_manifest_digest(&manifest)?
            }
            None => declaration.digest.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "locator-free external content `{}` has no retained digest",
                    declaration.id
                )
            })?,
        };
        let status = match declaration.mode {
            ryeos_engine::external_content::ExternalContentMode::Captured => "captured",
            ryeos_engine::external_content::ExternalContentMode::Pinned
                if declaration.digest.as_deref() == Some(observed_digest.as_str()) =>
            {
                "matched"
            }
            ryeos_engine::external_content::ExternalContentMode::Pinned => {
                ready_for_admission = false;
                "mismatched"
            }
        };
        previews.push(ExternalContentPinPreview {
            id: declaration.id,
            expected_digest: declaration.digest,
            observed_digest,
            status: status.to_string(),
        });
    }
    Ok(Some(ExternalContentValidationPreview {
        declarations: previews,
        ready_for_admission,
    }))
}

impl AdmittedExternalRealizations {
    pub fn finalization_evidence(&self) -> (&ExternalRealizationProof, &dyn RealizationStore) {
        (&self.proof, &self.store)
    }

    pub fn into_publication(mut self) -> Option<PendingCasPublication> {
        self.publication.take()
    }
}

/// Re-prove an already sealed realization using retained stores only.
pub fn recover_external_realizations(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<Option<AdmittedExternalRealizations>> {
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
    else {
        return Ok(None);
    };
    let realized = RealizedExternalContentSet::from_value(value)?;
    let store = ExternalRealizationStore::new(pinned_state_authority(state)?);
    let proof = ryeos_engine::external_realization::prove_external_realizations(realized, &store)?;
    Ok(Some(AdmittedExternalRealizations {
        proof,
        store,
        publication: None,
    }))
}

/// Admit the effective declaration list and project its retained realization
/// into the reserved composed-derived slot.
pub fn admit_external_realizations(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    inherited: Option<&RealizedExternalContentSet>,
) -> anyhow::Result<Option<AdmittedExternalRealizations>> {
    let mut publication = None;
    let mut admitted = admit_external_realizations_in_publication(
        state,
        engine,
        kind,
        resolution,
        roots,
        inherited,
        &mut publication,
    )?;
    if let Some(admitted) = admitted.as_mut() {
        admitted.publication = publication;
    }
    Ok(admitted)
}

pub fn admit_external_realizations_in_publication(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    inherited: Option<&RealizedExternalContentSet>,
    publication: &mut Option<PendingCasPublication>,
) -> anyhow::Result<Option<AdmittedExternalRealizations>> {
    let contract = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.external_content.as_ref());
    let declarer = ryeos_engine::external_content::declaring_authority(resolution)?;
    let Some(declarations) = ryeos_engine::external_content::declarations_from_composed(
        &resolution.composed.composed,
        contract,
        declarer,
    )?
    else {
        return inherit_external_realizations(state, resolution, inherited);
    };

    admit_declarations_in_publication(
        state,
        Some(engine),
        Some(roots),
        resolution,
        contract,
        declarations,
        inherited,
        publication,
        kind,
    )
}

/// Admit the locator-free pinned declarations of a prepared content
/// dependency. The signed launch policy supplies only mechanical ceilings;
/// manifest identity and consumer binding remain owned by the resolved item
/// and the existing external-content subsystem.
pub fn admit_portable_content_dependency_in_publication(
    state: &AppState,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    policy: &ryeos_engine::runtime_registry::LaunchContentExternalPolicy,
    inherited: Option<&RealizedExternalContentSet>,
    publication: &mut Option<PendingCasPublication>,
) -> anyhow::Result<AdmittedExternalRealizations> {
    let contract = policy.declaration_contract();
    let declarer = ryeos_engine::external_content::declaring_authority(resolution)?;
    let declarations = ryeos_engine::external_content::declarations_from_composed(
        &resolution.composed.composed,
        Some(&contract),
        declarer,
    )?
    .ok_or_else(|| anyhow::anyhow!("content dependency has no external_content declaration"))?;
    if declarations.is_empty()
        || declarations.iter().any(|declaration| {
            declaration.mode != ryeos_engine::external_content::ExternalContentMode::Pinned
                || declaration.locator.is_some()
                || declaration.digest.is_none()
        })
    {
        anyhow::bail!(
            "content dependency must contain at least one locator-free pinned declaration"
        );
    }
    admit_declarations_in_publication(
        state,
        None,
        None,
        resolution,
        Some(&contract),
        declarations,
        inherited,
        publication,
        "content-dependency",
    )?
    .ok_or_else(|| anyhow::anyhow!("content dependency produced no external realization"))
}

#[allow(clippy::too_many_arguments)]
fn admit_declarations_in_publication(
    state: &AppState,
    engine: Option<&ryeos_engine::engine::Engine>,
    roots: Option<&ryeos_engine::item_resolution::ResolutionRoots>,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    contract: Option<&ryeos_engine::kind_registry::ExecutionExternalContentDecl>,
    declarations: Vec<ExternalContentDeclaration>,
    inherited: Option<&RealizedExternalContentSet>,
    publication: &mut Option<PendingCasPublication>,
    diagnostic_kind: &str,
) -> anyhow::Result<Option<AdmittedExternalRealizations>> {
    if publication.is_none() {
        let authority = pinned_state_authority(state)?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        let staged_roots = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(&guard, "launch-realization")?;
        drop(guard);
        *publication = Some(PendingCasPublication::new(authority, staged_roots));
    }
    let authority = publication
        .as_ref()
        .expect("external admission initialized its publication")
        .authority()
        .try_clone()?;
    let proof_authority = authority.try_clone()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire CAS write permit: {error}"))?;
    let cas = authority.cas_store()?;
    let staged_roots = publication
        .as_mut()
        .expect("external admission initialized its publication")
        .staged_roots_mut();
    let mut budget = LaunchCaptureBudget::default();
    let mut realized =
        Vec::with_capacity(declarations.len() + inherited.map(|set| set.iter().len()).unwrap_or(0));
    let mut sink = GuardedCasBlobSink {
        guard: &guard,
        cas: &cas,
        staged_roots,
        stored_blobs: 0,
        reused_blobs: 0,
    };

    let mut content_total = 0u64;
    let mut large_total = 0u64;
    let consumer_ref = resolution.root.resolved_ref.clone();
    let consumer_publisher = resolution.root.signer_fingerprint.clone();
    for declaration in &declarations {
        if declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
            && declaration.locator.is_none()
            && let Some(digest) = declaration.digest.as_deref()
            && let Some(value) = cas.get_object(digest)?
            && value.get("kind").and_then(serde_json::Value::as_str)
                == Some(ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND)
        {
            realized.push(seal_pinned_content_realization(
                declaration,
                digest,
                &authority,
                &guard,
                &cas,
                &mut sink,
                &mut content_total,
                state,
                &consumer_ref,
                consumer_publisher.as_deref(),
            )?);
            continue;
        }
        if declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
            && let Some(digest) = declaration.digest.as_deref()
            && let Some(large_manifest) =
                ryeos_state::objects::load_if_large_content_manifest(&cas, digest)?
        {
            realized.push(seal_pinned_large_realization(
                declaration,
                digest,
                large_manifest,
                contract,
                &authority,
                &guard,
                &cas,
                &mut sink,
                &mut large_total,
                state,
                &consumer_ref,
                consumer_publisher.as_deref(),
            )?);
            continue;
        }

        let locator = declaration.locator.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "external content `{}` has no retained manifest and no admitted source locator",
                declaration.id
            )
        })?;
        let base_path = resolve_named_root(
            engine.ok_or_else(|| anyhow::anyhow!("locator admission has no engine authority"))?,
            roots.ok_or_else(|| anyhow::anyhow!("locator admission has no resolution roots"))?,
            &locator.root,
        )?;
        let base = lillux::PinnedDirectory::open(&base_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "external content root `{}` is unavailable",
                locator.root.label()
            )
        })?;
        let policy =
            ExternalCapturePolicy::new(locator.path.clone(), state.ignore_matcher.as_ref())?;
        let manifest = ryeos_state::capture_external_content_at(
            &base,
            &locator.path,
            capture_kind(declaration.kind),
            &declaration.exclude,
            &policy,
            &mut budget,
            &mut sink,
        )?;
        let manifest_hash = sink.staged_roots.store_object_admitted(
            &guard,
            &cas,
            &serde_json::to_value(&manifest)?,
        )?;
        let verified = ryeos_state::VerifiedExternalContentClosure::load(&cas, &manifest_hash)?;
        if verified.manifest() != &manifest {
            anyhow::bail!(
                "stored external content manifest {manifest_hash} differs from its captured value"
            );
        }
        if declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
            && declaration.digest.as_deref() != Some(manifest_hash.as_str())
        {
            anyhow::bail!(
                "pinned external content `{}` expected {}, observed {manifest_hash}",
                declaration.id,
                declaration.digest.as_deref().unwrap_or("<missing>")
            );
        }
        realized.push(RealizedExternalContent {
            id: declaration.id.clone(),
            kind: declaration.kind,
            mode: declaration.mode,
            manifest_hash,
            entry_count: manifest.entry_count,
            total_bytes: manifest.total_bytes,
            mount: declaration.mount.clone(),
        });
    }

    if let Some(inherited) = inherited {
        realized.extend(inherited.iter().cloned());
    }
    let realized = RealizedExternalContentSet::new(realized)?;
    resolution.composed.derived.insert(
        ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
        realized.to_value()?,
    );
    let store = ExternalRealizationStore::new(proof_authority);
    let proof = ryeos_engine::external_realization::prove_external_realizations(realized, &store)?;
    let (stored_blobs, reused_blobs) = sink.counts();
    tracing::info!(
        kind = diagnostic_kind,
        declaration_count = declarations.len(),
        stored_blobs,
        reused_blobs,
        "admitted external content realization"
    );
    drop(sink);
    drop(_permit);
    drop(guard);

    Ok(Some(AdmittedExternalRealizations {
        proof,
        store,
        publication: None,
    }))
}

#[allow(clippy::too_many_arguments)]
fn seal_pinned_content_realization(
    declaration: &ExternalContentDeclaration,
    digest: &str,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    cas: &lillux::CasStore,
    sink: &mut GuardedCasBlobSink<'_>,
    content_total: &mut u64,
    state: &AppState,
    consumer_ref: &str,
    consumer_publisher: Option<&str>,
) -> anyhow::Result<RealizedExternalContent> {
    let consumer_publisher = consumer_publisher.ok_or_else(|| {
        anyhow::anyhow!("retained external-content consumer has no verified publisher fingerprint")
    })?;
    crate::operator_external_content::require_active_binding(
        state,
        cas,
        digest,
        consumer_ref,
        consumer_publisher,
    )?;
    if declaration.locator.is_some() {
        anyhow::bail!(
            "external content `{}` must bind retained bytes without a live locator",
            declaration.id
        );
    }
    let verified = ryeos_state::VerifiedExternalContentClosure::load(cas, digest)?;
    let manifest = verified.manifest();
    if declaration.kind == ExternalContentKind::File && !manifest.is_file_shaped() {
        anyhow::bail!(
            "external content `{}` declares a file but manifest {digest} is not file-shaped",
            declaration.id
        );
    }
    *content_total = content_total
        .checked_add(manifest.total_bytes)
        .ok_or_else(|| anyhow::anyhow!("external-content realization byte total overflow"))?;
    if *content_total > ryeos_state::objects::MAX_EXTERNAL_CONTENT_TOTAL_BYTES {
        anyhow::bail!("retained content realizations exceed the content-tier launch bound");
    }
    authority.ensure_guard(guard)?;
    sink.staged_roots.protect_cas_closure_admitted(
        guard,
        std::iter::once(digest),
        verified.verified_blob_sizes().keys().map(String::as_str),
    )?;
    Ok(RealizedExternalContent {
        id: declaration.id.clone(),
        kind: declaration.kind,
        mode: declaration.mode,
        manifest_hash: digest.to_owned(),
        entry_count: manifest.entry_count,
        total_bytes: manifest.total_bytes,
        mount: declaration.mount.clone(),
    })
}

fn inherit_external_realizations(
    state: &AppState,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    inherited: Option<&RealizedExternalContentSet>,
) -> anyhow::Result<Option<AdmittedExternalRealizations>> {
    let Some(inherited) = inherited else {
        return Ok(None);
    };
    let realized = inherited.clone();
    resolution.composed.derived.insert(
        ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
        realized.to_value()?,
    );
    let store = ExternalRealizationStore::new(pinned_state_authority(state)?);
    let proof = ryeos_engine::external_realization::prove_external_realizations(realized, &store)?;
    Ok(Some(AdmittedExternalRealizations {
        proof,
        store,
        publication: None,
    }))
}

struct ExternalRealizationStore {
    authority: ryeos_state::PinnedStateAuthority,
}

impl ExternalRealizationStore {
    fn new(authority: ryeos_state::PinnedStateAuthority) -> Self {
        Self { authority }
    }
}

impl RealizationStore for ExternalRealizationStore {
    fn realization_available(&self, manifest_hash: &str) -> anyhow::Result<bool> {
        let guard = self.authority.acquire_shared_guard()?;
        self.authority.ensure_guard(&guard)?;
        let cas = self.authority.cas_store()?;
        if let Some(manifest) =
            ryeos_state::objects::load_if_large_content_manifest(&cas, manifest_hash)?
        {
            let store = self.authority.large_object_store()?;
            for entry in &manifest.entries {
                if entry.file_sha256.is_some() {
                    store.verify_manifest_commitment(entry)?;
                }
            }
            let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                &cas,
                [manifest_hash.to_owned()],
                ryeos_state::object_closure::ObjectClosureLimits::default(),
            )?;
            if !closure.is_complete() {
                anyhow::bail!("large-content realization closure is incomplete");
            }
            return Ok(true);
        }
        ryeos_state::VerifiedExternalContentClosure::load(&cas, manifest_hash).map(|_| true)
    }
}

struct GuardedCasBlobSink<'a> {
    guard: &'a ryeos_state::CasMutationGuard,
    cas: &'a lillux::CasStore,
    staged_roots: &'a mut ryeos_state::StagedCasRootLease,
    stored_blobs: usize,
    reused_blobs: usize,
}

impl GuardedCasBlobSink<'_> {
    fn counts(&self) -> (usize, usize) {
        (self.stored_blobs, self.reused_blobs)
    }
}

impl ExternalContentBlobSink for GuardedCasBlobSink<'_> {
    fn store_file(
        &mut self,
        file: std::fs::File,
        path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)> {
        if expected_size > MAX_CAPTURE_FILE_BYTES {
            anyhow::bail!("external content file {path} exceeds {MAX_CAPTURE_FILE_BYTES} bytes");
        }
        let outcome = self.cas.put_blob_from_open_regular_bounded(
            file,
            Path::new(path),
            MAX_CAPTURE_FILE_BYTES,
        )?;
        self.staged_roots
            .protect_blob_hash_admitted(self.guard, &outcome.hash)?;
        if outcome.created {
            self.stored_blobs += 1;
        } else {
            self.reused_blobs += 1;
        }
        Ok((outcome.hash, outcome.size))
    }
}

#[allow(clippy::too_many_arguments)]
fn seal_pinned_large_realization(
    declaration: &ExternalContentDeclaration,
    digest: &str,
    manifest: ryeos_state::objects::ExternalLargeContentManifestObject,
    contract: Option<&ryeos_engine::kind_registry::ExecutionExternalContentDecl>,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    cas: &lillux::CasStore,
    sink: &mut GuardedCasBlobSink<'_>,
    large_total: &mut u64,
    state: &AppState,
    consumer_ref: &str,
    consumer_publisher: Option<&str>,
) -> anyhow::Result<RealizedExternalContent> {
    let grant = contract
        .and_then(|contract| contract.large_content.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "external content `{}` names a large manifest without a signed large-content grant",
                declaration.id
            )
        })?;
    let consumer_publisher = consumer_publisher.ok_or_else(|| {
        anyhow::anyhow!("large-content consumer has no verified publisher fingerprint")
    })?;
    crate::operator_external_content::require_active_binding(
        state,
        cas,
        digest,
        consumer_ref,
        consumer_publisher,
    )?;
    if declaration.locator.is_some() {
        anyhow::bail!(
            "external content `{}` must bind large bytes from the retained store, not a live locator",
            declaration.id
        );
    }
    *large_total = large_total
        .checked_add(manifest.total_bytes)
        .ok_or_else(|| anyhow::anyhow!("large-content realization byte total overflow"))?;
    let ceiling = grant
        .max_total_bytes
        .unwrap_or(ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES);
    if *large_total > ceiling {
        anyhow::bail!("large-content realizations exceed the signed {ceiling}-byte grant");
    }
    if declaration.kind == ExternalContentKind::File && !manifest.is_file_shaped() {
        anyhow::bail!(
            "external content `{}` declares a file but manifest {digest} is not file-shaped",
            declaration.id
        );
    }
    let store = authority.large_object_store()?;
    for entry in &manifest.entries {
        if entry.file_sha256.is_some() {
            store.verify_manifest_commitment(entry)?;
        }
    }
    let stored = sink
        .staged_roots
        .store_object_admitted(guard, cas, &manifest.to_value()?)?;
    if stored != digest {
        anyhow::bail!("large-content manifest {digest} re-stored as {stored}");
    }
    Ok(RealizedExternalContent {
        id: declaration.id.clone(),
        kind: declaration.kind,
        mode: declaration.mode,
        manifest_hash: digest.to_owned(),
        entry_count: manifest.entry_count,
        total_bytes: manifest.total_bytes,
        mount: declaration.mount.clone(),
    })
}

fn resolve_named_root(
    engine: &ryeos_engine::engine::Engine,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    root: &ExternalContentRoot,
) -> anyhow::Result<std::path::PathBuf> {
    match root {
        ExternalContentRoot::ProjectFiles => roots
            .authoritative_root(
                &ryeos_engine::contracts::ItemSourceRoot::Project,
                ItemSpace::Project,
                None,
            )
            .map_err(|error| {
                anyhow::anyhow!("project_files root authority is unavailable: {error}")
            })?
            .content_root
            .clone()
            .ok_or_else(|| anyhow::anyhow!("project_files root has no content authority")),
        ExternalContentRoot::NodeFiles => engine
            .node_config_root()
            .ok_or_else(|| anyhow::anyhow!("node_files root authority is unavailable")),
        ExternalContentRoot::Bundle(name) => roots
            .authoritative_bundle(name)
            .map_err(|error| {
                anyhow::anyhow!("bundle:{name} root authority is unavailable: {error}")
            })?
            .content_root
            .clone()
            .ok_or_else(|| anyhow::anyhow!("bundle:{name} root has no content authority")),
    }
}

fn capture_kind(kind: ExternalContentKind) -> ExternalContentCaptureKind {
    match kind {
        ExternalContentKind::Tree => ExternalContentCaptureKind::Tree,
        ExternalContentKind::File => ExternalContentCaptureKind::File,
    }
}

fn pinned_state_authority(state: &AppState) -> anyhow::Result<ryeos_state::PinnedStateAuthority> {
    state.state_store.pinned_state_authority()
}
