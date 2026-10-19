//! Daemon-owned acquisition and publication of admitted executable source.
//!
//! Engine code defines the authority-neutral capture/testimony contracts.
//! This layer selects a typed project or bundle authority, applies the shared
//! durable capture floor, publishes immutable blobs and objects under a
//! staged root, and inserts only the bounded derived projection.

use std::path::Path;

use anyhow::{Context as _, ensure};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace, SubjectResolutionAuthority};
use ryeos_engine::launch::plan_builder::ExecutorSourcePolicyProjection;
use ryeos_engine::project_content::{AuthoritativeProjectContent, ProjectContentEntry};
use ryeos_engine::source_closure::{
    AuthoritativeSourceContent, CapturedSourceCandidate, SourceClosureProof, SourceClosureStore,
    SourceRootRequest, SourceRootSelection,
};
use ryeos_engine::source_closure::source_view::{
    DirectorySourceContent, canonical_item_source_path, executor_source_request,
    item_directory_binding_root,
};
use ryeos_state::PendingCasPublication;
use ryeos_state::objects::{
    AdmittedStructuredSessionProfile, EffectiveSourceClosureProjection, SOURCE_CLOSURE_DERIVED_KEY,
};

use crate::state::AppState;

mod observation;
pub use observation::{StructuredSourceValidationObservation, observe_bundle_structured_source};

pub struct AdmittedSourceClosure {
    proof: SourceClosureProof,
    store: RetainedSourceClosureStore,
    publication: Option<PendingCasPublication>,
    binding: ryeos_state::objects::EffectiveSourceBinding,
    manifest: ryeos_state::objects::SourceClosureManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceClosureValidationPreview {
    pub owner_ref: String,
    pub expected_digest: Option<String>,
    pub observed_digest: String,
    pub binding_digest: Option<String>,
    pub file_count: usize,
    pub total_bytes: u64,
    pub ready_for_admission: bool,
    pub status: String,
}

impl AdmittedSourceClosure {
    /// Reproduce request DATA from this exact recovered signed source, never
    /// from a mutable registry or the caller's current request carrier.
    pub(crate) fn verify_retained_worker_target_data(
        &self,
        engine: &ryeos_engine::engine::Engine,
        resolution: &ryeos_engine::resolution::ResolutionOutput,
        plan: &ryeos_engine::contracts::ExecutionPlan,
    ) -> anyhow::Result<()> {
        plan.validate_current_target_data()?;
        let schema = require_current_kind_testimony(
            &engine.node_trust_store, &engine.trust_store, &self.binding)?;
        let opted = schema.execution.as_ref().and_then(|e| e.target.as_ref())
            .is_some_and(|t| t.request_format.is_some());
        if !opted && plan.current_target_request.is_none() { return Ok(()); }
        ensure!(self.binding.owner == source_owner_identity(resolution)?,
            "current request source owner contradicts exact retained resolution");
        ensure!(self.binding.owner.item_kind == "worker"
            && self.binding.owner.canonical_ref == plan.root_ref
            && plan.root_ref == resolution.root.resolved_ref && plan.item_kind == "worker",
            "current request lacks SAME signed Worker root");
        ensure!(matches!(&self.binding.execution_policy,
            ryeos_state::objects::SourceExecutionPolicyIdentity::Worker { .. }),
            "current request source is not a Worker source policy");
        require_current_source_policy(&engine.node_trust_store, &engine.trust_store,
            resolution, &self.binding, &schema)?;
        let execution = schema.execution.as_ref()
            .context("retained Worker lacks execution schema")?;
        let (expected_ordinary, expected_current) = execution.project_target_data(&resolution.composed.composed)?;
        ensure!(expected_ordinary == plan.target_requirement,
            "retained signed Worker target contradicts admitted ordinary target");
        let expected = expected_current.as_ref().map(|v| v.canonical_bytes()).transpose()?;
        let observed = plan.current_target_request.as_ref().map(|v| v.canonical_bytes()).transpose()?;
        ensure!(expected == observed,
            "retained signed Worker current request differs or was dropped");
        Ok(())
    }

    pub fn finalization_evidence(&self) -> (&SourceClosureProof, &dyn SourceClosureStore) {
        (&self.proof, &self.store)
    }

    pub fn binding(&self) -> &ryeos_state::objects::EffectiveSourceBinding {
        &self.binding
    }

    pub fn manifest(&self) -> &ryeos_state::objects::SourceClosureManifest {
        &self.manifest
    }

    /// Exact CAS authority that retained this capture, including staged
    /// source blobs before its pending publication is committed.
    pub fn source_authority(&self) -> anyhow::Result<ryeos_state::PinnedStateAuthority> {
        self.store.authority.try_clone()
    }

    pub fn into_publication(mut self) -> Option<PendingCasPublication> {
        self.publication.take()
    }
}

/// Raw owned bytes beneath an ACTUAL recovered source. The original input owner
/// must retain this value before content validation/parsing or late checks.
pub(crate) struct LoadedStartupContractMember {
    pub(crate) file: ryeos_state::objects::SourceClosureFile,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) fn load_selected_startup_contract_member(
    captured: &AdmittedSourceClosure,
    selection: &ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection,
    guard: &ryeos_state::CasMutationGuard,
) -> anyhow::Result<LoadedStartupContractMember> {
    selection.validate().map_err(anyhow::Error::msg)?;
    let authority = captured.source_authority()?;
    // Recovery acquired its own authority: join that ACTUAL authority to the
    // original owner's SAME guard BEFORE any selected blob read.
    authority.ensure_guard(guard)?;
    captured
        .binding()
        .validate_content_manifest(captured.manifest())?;
    let file = captured
        .manifest()
        .entries
        .iter()
        .find(|file| file.root == selection.source_root && file.path == selection.member)
        .context("signed startup contract member absent from actual source")?;
    ensure!(
        file.blob_hash == selection.sha256 && file.size > 0 && file.size <= 16 * 1024,
        "signed startup contract member identity/size mismatch"
    );
    let cas = authority.cas_store()?;
    let bytes = cas
        .get_blob_bounded(&file.blob_hash, file.size)?
        .context("signed startup contract blob absent")?;
    // Raw bytes are returned for deposit before validation. This helper cannot
    // issue accepted startup authority or a qualified device/provider value.
    Ok(LoadedStartupContractMember {
        file: file.clone(),
        bytes,
    })
}

/// Compile a structured-session Worker only from its admitted source blobs.
/// The supplied guard must belong to the capture's retained CAS authority.
/// This helper never reopens the live Bundle or treats a profile path as authority.
pub fn compile_admitted_structured_worker_profile(
    captured: &AdmittedSourceClosure,
    guard: &ryeos_state::recovery::CasMutationGuard,
) -> anyhow::Result<ryeos_state::objects::AdmittedStructuredSessionProfile> {
    let authority = captured.source_authority()?;
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    compile_structured_worker_from_cas(&cas, captured.binding(), captured.manifest())
}

/// Reconstruct data beneath an already authenticated retained projection.
/// This neither admits source nor creates executable authority. The caller
/// must join the returned owner/profile with its accepted enclosing purpose.
pub(crate) fn compile_retained_structured_worker_profile(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    projection: &EffectiveSourceClosureProjection,
) -> anyhow::Result<(
    AdmittedStructuredSessionProfile,
    ryeos_state::objects::SourceOwnerIdentity,
)> {
    authority.ensure_guard(guard)?;
    projection.validate()?;
    let cas = authority.cas_store()?;
    let binding = ryeos_state::objects::EffectiveSourceBinding::from_value(
        &ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &cas,
            &projection.binding_hash,
            ryeos_state::objects::MAX_SOURCE_BINDING_BYTES as u64,
        )?,
    )?;
    let manifest = ryeos_state::objects::SourceClosureManifest::from_value(
        &ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &cas,
            &projection.content_manifest_hash,
            ryeos_state::objects::MAX_SOURCE_MANIFEST_BYTES as u64,
        )?,
    )?;
    ensure!(
        binding.digest()? == projection.binding_hash
            && binding.content_manifest_hash == projection.content_manifest_hash
            && manifest.digest()? == projection.content_manifest_hash
            && binding.owner_key()? == projection.owner_key
            && manifest.totals.file_count == projection.file_count
            && manifest.totals.total_bytes == projection.total_bytes,
        "retained Worker source differs from authenticated projection"
    );
    let profile = compile_structured_worker_from_cas(&cas, &binding, &manifest)?;
    authority.ensure_guard(guard)?;
    Ok((profile, binding.owner))
}

fn compile_structured_worker_from_cas(
    cas: &lillux::CasStore,
    binding: &ryeos_state::objects::EffectiveSourceBinding,
    manifest: &ryeos_state::objects::SourceClosureManifest,
) -> anyhow::Result<AdmittedStructuredSessionProfile> {
    compile_structured_worker_from_blobs(binding, manifest, |hash, size| {
        cas.get_blob_bounded(hash, size)?
            .context("captured Worker source blob is absent")
    })
}

/// Admission and source observation compile the same exact manifest-owned
/// bytes. The loader supplies either retained CAS bytes or the original capture;
/// it never resolves a profile path from the live filesystem.
fn compile_structured_worker_from_blobs(
    binding: &ryeos_state::objects::EffectiveSourceBinding,
    manifest: &ryeos_state::objects::SourceClosureManifest,
    load: impl FnMut(&str, u64) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<AdmittedStructuredSessionProfile> {
    binding.validate_content_manifest(manifest)?;
    let entry = match &binding.logical_binding {
        ryeos_state::objects::SourceLogicalBinding::Worker { entry, .. } => entry,
        _ => anyhow::bail!("structured-session source has a non-Worker logical binding"),
    };
    let source_files = load_profile_source_files(manifest, load)?;
    let profile_bytes = source_files.get(entry).ok_or_else(|| {
        anyhow::anyhow!("structured-session entry is absent from captured source")
    })?;
    ryeos_engine::structured_session_profile::compile(profile_bytes, &source_files)
}

fn load_profile_source_files(
    manifest: &ryeos_state::objects::SourceClosureManifest,
    mut load: impl FnMut(&str, u64) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<std::collections::BTreeMap<String, Vec<u8>>> {
    manifest.validate()?;
    let mut source_files = std::collections::BTreeMap::new();
    for file in &manifest.entries {
        let bytes = load(&file.blob_hash, file.size)?;
        ensure!(
            bytes.len() as u64 == file.size && lillux::sha256_hex(&bytes) == file.blob_hash,
            "retained Worker source blob differs from exact manifest"
        );
        ensure!(
            source_files.insert(file.path.clone(), bytes).is_none(),
            "structured-session source has duplicate logical paths"
        );
    }
    Ok(source_files)
}

/// One current signed Bundle Worker, captured before product selection. The
/// effective definition here includes admitted source but no product result.
/// It is not a qualification claim or applied-runtime testimony.
pub struct AdmittedBundleStructuredWorkerProfile {
    pub bundle_generation_identity: String,
    pub source: EffectiveSourceClosureProjection,
    pub profile: AdmittedStructuredSessionProfile,
    pub signed_effective_definition_digest: String,
    pub preselection_effective_definition_digest: String,
    pub raw_content_digest: String,
    pub publisher_fingerprint: String,
}

/// Captured source whose CAS roots remain staged until its consumer has
/// checked the signed-definition join. Dropping it never grants a profile.
pub struct PreparedBundleStructuredWorkerProfile {
    admitted: AdmittedBundleStructuredWorkerProfile,
    publication: Option<PendingCasPublication>,
}

impl PreparedBundleStructuredWorkerProfile {
    pub fn admitted(&self) -> &AdmittedBundleStructuredWorkerProfile {
        &self.admitted
    }

    pub fn publish(self) -> anyhow::Result<AdmittedBundleStructuredWorkerProfile> {
        if let Some(publication) = self.publication {
            publication.publish()?;
        }
        Ok(self.admitted)
    }

    /// Transfer the source roots into a larger admission transaction. Its
    /// owner must keep this stage until all joined inputs have been checked.
    pub fn into_staged_parts(
        self,
    ) -> (
        AdmittedBundleStructuredWorkerProfile,
        Option<PendingCasPublication>,
    ) {
        (self.admitted, self.publication)
    }
}

pub fn admit_bundle_structured_worker_profile(
    state: &AppState,
    worker_ref: &str,
) -> anyhow::Result<AdmittedBundleStructuredWorkerProfile> {
    prepare_bundle_structured_worker_profile(state, worker_ref)?.publish()
}

pub fn prepare_bundle_structured_worker_profile(
    state: &AppState,
    worker_ref: &str,
) -> anyhow::Result<PreparedBundleStructuredWorkerProfile> {
    let canonical = CanonicalRef::parse(worker_ref)?;
    ensure!(
        canonical.to_string() == worker_ref
            && canonical.suffix.is_none()
            && canonical.kind == "worker",
        "structured-session Worker must be an exact Bundle ref"
    );
    let (admitted, publication) = state.engine.with_checked_bundle_generation(|generation| {
        let roots = state.engine.resolution_roots(None);
        let mut resolution =
            generation.effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
                item_ref: canonical,
                expected_kind: Some("worker".into()),
                project_root: None,
                subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            })?;
        ensure!(
            resolution.root.resolved_ref == worker_ref
                && resolution.effective_trust_class
                    == ryeos_engine::resolution::TrustClass::TrustedBundle
                && resolution.root.source_space == ItemSpace::Bundle
                && matches!(resolution.root.source_root, ItemSourceRoot::Bundle { .. }),
            "structured-session Worker must resolve from a trusted Bundle"
        );
        let raw_content_digest = resolution.root.raw_content_digest.clone();
        let signed_effective_definition_digest = resolution
            .effective_definition_digest()?
            .as_str()
            .to_owned();
        let publisher_fingerprint = resolution
            .root
            .signer_fingerprint
            .clone()
            .context("trusted Worker has no publisher")?;
        let captured = admit_source_closure(
            state,
            &state.engine,
            "worker",
            &mut resolution,
            &roots,
            None,
            None,
        )?
        .context("signed Worker has no admitted source closure")?;
        let source = EffectiveSourceClosureProjection::from_value(
            resolution
                .composed
                .derived
                .get(SOURCE_CLOSURE_DERIVED_KEY)
                .context("source admission omitted its exact projection")?,
        )?;
        ensure!(
            captured.binding().digest()? == source.binding_hash
                && captured.manifest().digest()? == source.content_manifest_hash,
            "signed Worker source projection differs from its captured closure"
        );
        let authority = captured.source_authority()?;
        let guard = authority.acquire_shared_guard()?;
        let profile = compile_admitted_structured_worker_profile(&captured, &guard)?;
        profile.validate()?;
        drop(guard);
        crate::effective_program_preparation::prepare_hookless_preselection_effective_program(
            &state.engine,
            "worker",
            &mut resolution,
        )?;
        let preselection_effective_definition_digest = resolution
            .effective_definition_digest()?
            .as_str()
            .to_owned();
        Ok((
            AdmittedBundleStructuredWorkerProfile {
                bundle_generation_identity: generation.request_engine_generation_identity().into(),
                source,
                profile,
                signed_effective_definition_digest,
                preselection_effective_definition_digest,
                raw_content_digest,
                publisher_fingerprint,
            },
            captured.into_publication(),
        ))
    })?;
    Ok(PreparedBundleStructuredWorkerProfile {
        admitted,
        publication,
    })
}

pub fn admit_source_closure(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    project: Option<(&Path, &dyn AuthoritativeProjectContent, String)>,
    executor_policy: Option<&ExecutorSourcePolicyProjection>,
) -> anyhow::Result<Option<AdmittedSourceClosure>> {
    let mut publication = None;
    let mut admitted = admit_source_closure_in_publication(
        state,
        engine,
        kind,
        resolution,
        roots,
        project,
        executor_policy,
        &mut publication,
        None,
    )?;
    if let Some(admitted) = admitted.as_mut() {
        admitted.publication = publication;
    }
    Ok(admitted)
}

pub fn admit_source_closure_in_publication(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    project: Option<(&Path, &dyn AuthoritativeProjectContent, String)>,
    executor_policy: Option<&ExecutorSourcePolicyProjection>,
    publication: &mut Option<PendingCasPublication>,
    preview: Option<&mut Option<SourceClosureValidationPreview>>,
) -> anyhow::Result<Option<AdmittedSourceClosure>> {
    let Some(captured) = capture_source_closure(
        state,
        engine,
        kind,
        resolution,
        roots,
        project,
        executor_policy,
        preview,
    )?
    else {
        return Ok(None);
    };
    let CapturedSourceClosure { candidate, binding } = captured;
    let content_manifest_hash = binding.content_manifest_hash.clone();

    if publication.is_none() {
        let authority = state.state_store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        let staged = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(&guard, "launch-realization")?;
        drop(guard);
        *publication = Some(PendingCasPublication::new(authority, staged));
    }
    let authority = publication
        .as_ref()
        .expect("source admission initialized its publication")
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
    let staged = publication
        .as_mut()
        .expect("source admission initialized its publication")
        .staged_roots_mut();
    for blob in &candidate.blobs {
        let outcome = cas.put_blob(&blob.bytes)?;
        if outcome.hash != blob.blob_hash {
            anyhow::bail!("stored source blob contradicts its captured identity");
        }
        staged.protect_blob_hash_admitted(&guard, &outcome.hash)?;
    }
    let stored_manifest_hash =
        staged.store_object_admitted(&guard, &cas, &candidate.manifest.to_value()?)?;
    if stored_manifest_hash != content_manifest_hash {
        anyhow::bail!("stored source manifest contradicts its captured identity");
    }
    let binding_hash = staged.store_object_admitted(&guard, &cas, &binding.to_value()?)?;
    let projection = ryeos_state::objects::EffectiveSourceClosureProjection {
        schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
        binding_hash,
        content_manifest_hash,
        owner_key: binding.owner_key()?,
        file_count: candidate.manifest.totals.file_count,
        total_bytes: candidate.manifest.totals.total_bytes,
    };
    resolution.composed.derived.insert(
        ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
        projection.to_value()?,
    );
    let store = RetainedSourceClosureStore {
        authority: proof_authority,
    };
    let proof = ryeos_engine::source_closure::prove_source_closure(projection, &store)?;
    drop(_permit);
    drop(guard);
    Ok(Some(AdmittedSourceClosure {
        proof,
        store,
        publication: None,
        binding,
        manifest: candidate.manifest,
    }))
}

/// Original bounded bytes and their checked testimony. This private value has
/// no CAS roots, publication, derived executable projection or launch authority.
struct CapturedSourceClosure {
    candidate: CapturedSourceCandidate,
    binding: ryeos_state::objects::EffectiveSourceBinding,
}

#[allow(clippy::too_many_arguments)]
fn capture_source_closure(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    project: Option<(&Path, &dyn AuthoritativeProjectContent, String)>,
    executor_policy: Option<&ExecutorSourcePolicyProjection>,
    mut preview: Option<&mut Option<SourceClosureValidationPreview>>,
) -> anyhow::Result<Option<CapturedSourceClosure>> {
    if resolution
        .composed
        .derived
        .contains_key(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
    {
        anyhow::bail!(
            "effective program attempted to pre-populate reserved source closure authority"
        );
    }
    let kind_schema = engine
        .kinds
        .get(kind)
        .ok_or_else(|| anyhow::anyhow!("source closure kind `{kind}` is not registered"))?;
    let contract = kind_schema
        .execution
        .as_ref()
        .and_then(|execution| execution.source_closure.as_ref());
    let Some(contract) = contract else {
        if resolution.composed.composed.get("source").is_some() {
            anyhow::bail!(
                "item declares `source` but its signed kind has no source closure authority"
            );
        }
        return Ok(None);
    };
    if matches!(
        &contract.location,
        ryeos_engine::kind_registry::SourceClosureLocationDecl::ItemNamespace
    ) && executor_policy.is_none()
    {
        return Ok(None);
    }
    let kind_evidence = engine.kinds.schema_evidence(kind).ok_or_else(|| {
        anyhow::anyhow!("source closure kind `{kind}` has no retained signed schema evidence")
    })?;
    let registered = roots.authoritative_root(
        &resolution.root.source_root,
        resolution.root.source_space,
        None,
    )?;
    let content_root = registered.content_root.as_deref().ok_or_else(|| {
        anyhow::anyhow!("source closure typed root has no registered content authority")
    })?;

    let (source, requests, root_entry, root_item_extension, worker_declaration): (
        Box<dyn AuthoritativeSourceContent + '_>,
        Vec<SourceRootRequest>,
        String,
        String,
        Option<ryeos_engine::source_closure::WorkerSourceDeclaration>,
    ) = match &contract.location {
        ryeos_engine::kind_registry::SourceClosureLocationDecl::ItemNamespace => {
            let policy = executor_policy.ok_or_else(|| {
                anyhow::anyhow!("direct source-owning item has no verified executor source policy")
            })?;
            let source_location = policy.policy.location;
            let source: Box<dyn AuthoritativeSourceContent> =
                match (resolution.root.source_space, project) {
                    (ItemSpace::Project, Some((project_root, content, identity))) => {
                        if project_root != content_root {
                            anyhow::bail!(
                                "project source authority root differs from typed resolution root"
                            );
                        }
                        Box::new(ProjectSourceContent { content, identity })
                    }
                    (ItemSpace::Project, None) => Box::new(DirectorySourceContent::new(
                        content_root,
                        format!(
                            "live:{}:{}",
                            root_identity_key(&resolution.root.source_root),
                            resolution.root.source_content_digest
                        ),
                        state.ignore_matcher.as_ref(),
                    )?),
                    (_, _) => Box::new(DirectorySourceContent::new(
                        content_root,
                        root_identity_key(&resolution.root.source_root),
                        state.ignore_matcher.as_ref(),
                    )?),
                };
            let (request, entry) = executor_source_request(
                source.as_ref(),
                source_location,
                kind,
                kind_schema,
                &resolution.root.resolved_ref,
                &resolution.root.source_content_digest,
                contract.max_file_bytes,
            )?;
            let extension = Path::new(&entry)
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| format!(".{extension}"))
                .ok_or_else(|| anyhow::anyhow!("source owner has no kind-owned extension"))?;
            (source, vec![request], entry, extension, None)
        }
        ryeos_engine::kind_registry::SourceClosureLocationDecl::OwnerRelativeSource { .. } => {
            if executor_policy.is_some() {
                anyhow::bail!("self-launching source cannot borrow an executor source policy");
            }
            let declaration = if preview.is_some() {
                ryeos_engine::source_closure::WorkerSourceDeclaration::from_composed_for_static_preview(
                    &resolution.composed.composed,
                    Some(contract),
                )?
            } else {
                ryeos_engine::source_closure::WorkerSourceDeclaration::from_composed(
                    &resolution.composed.composed,
                    Some(contract),
                )?
            }
            .ok_or_else(|| anyhow::anyhow!("source declaration is absent"))?;
            let bare_id = resolution
                .root
                .resolved_ref
                .split_once(':')
                .map(|(_, id)| id)
                .ok_or_else(|| anyhow::anyhow!("source owner ref is not canonical"))?;
            let owner_namespace = bare_id
                .split('/')
                .next()
                .filter(|part| !part.is_empty())
                .ok_or_else(|| anyhow::anyhow!("source owner namespace is absent"))?;
            let prefix = Path::new(ryeos_engine::AI_DIR)
                .join(&kind_schema.directory)
                .join(owner_namespace)
                .join(&declaration.root);
            let source: Box<dyn AuthoritativeSourceContent> =
                match (resolution.root.source_space, project) {
                    (ItemSpace::Project, Some((project_root, content, identity))) => {
                        if project_root != content_root {
                            anyhow::bail!(
                                "project source authority root differs from typed resolution root"
                            );
                        }
                        Box::new(ProjectSourceContent { content, identity })
                    }
                    (ItemSpace::Project, None) => Box::new(DirectorySourceContent::new(
                        content_root,
                        format!(
                            "live:{}:{}",
                            root_identity_key(&resolution.root.source_root),
                            resolution.root.source_content_digest
                        ),
                        state.ignore_matcher.as_ref(),
                    )?),
                    (_, _) => Box::new(DirectorySourceContent::new(
                        content_root,
                        root_identity_key(&resolution.root.source_root),
                        state.ignore_matcher.as_ref(),
                    )?),
                };
            let descriptor_path = canonical_item_source_path(
                source.as_ref(),
                kind,
                kind_schema,
                &resolution.root.resolved_ref,
                &resolution.root.source_content_digest,
                contract.max_file_bytes,
            )?;
            let descriptor_extension = descriptor_path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| format!(".{extension}"))
                .ok_or_else(|| anyhow::anyhow!("source owner has no kind-owned extension"))?;
            let entry = declaration.entry.clone();
            (
                source,
                vec![SourceRootRequest {
                    id: "source".to_owned(),
                    selection: SourceRootSelection::Tree { prefix },
                }],
                entry,
                descriptor_extension,
                Some(declaration),
            )
        }
    };

    let candidate = ryeos_engine::source_closure::capture_source_candidate(
        source.as_ref(),
        &requests,
        contract,
    )?;
    require_manifest_entry(&candidate, &root_entry)?;
    if let Some(output) = preview.as_deref_mut()
        && let Some(declaration) = worker_declaration.as_ref()
    {
        let observed_digest = candidate.manifest.digest()?;
        let (ready_for_admission, status) = if !lillux::valid_hash(&declaration.digest) {
            (false, "pending")
        } else if declaration.digest == observed_digest {
            (true, "matched")
        } else {
            (false, "mismatched")
        };
        *output = Some(SourceClosureValidationPreview {
            owner_ref: resolution.root.resolved_ref.to_string(),
            expected_digest: Some(declaration.digest.clone()),
            observed_digest,
            binding_digest: None,
            file_count: candidate.manifest.totals.file_count,
            total_bytes: candidate.manifest.totals.total_bytes,
            ready_for_admission,
            status: status.to_owned(),
        });
        return Ok(None);
    }
    let testimony = match contract.testimony {
        ryeos_engine::kind_registry::SourceClosureTestimonyDecl::OwnerSignedFiles => {
            verify_owner_signed_files(
                &candidate,
                &root_entry,
                kind_schema,
                resolution,
                &engine.trust_store,
            )?
        }
        ryeos_engine::kind_registry::SourceClosureTestimonyDecl::OwnerSignedDigest => {
            let declaration = worker_declaration.as_ref().ok_or_else(|| {
                anyhow::anyhow!("signed-digest source has no atomic source declaration")
            })?;
            let observed = candidate.manifest.digest()?;
            if declaration.digest != observed {
                anyhow::bail!(
                    "signed source digest expected {}, observed {observed}",
                    declaration.digest
                );
            }
            ryeos_state::objects::SourceTestimonyProof::OwnerSignedDigest {
                expected_manifest_hash: declaration.digest.clone(),
            }
        }
    };

    let content_manifest_hash = candidate.manifest.digest()?;
    let owner = source_owner_identity(resolution)?;
    let normalized_declaration = serde_json::to_value(contract)?;
    let root_format = kind_schema
        .spec_for(&root_item_extension)
        .ok_or_else(|| anyhow::anyhow!("source owner extension is absent from its signed kind"))?;
    let execution_policy = if let Some(policy) = executor_policy {
        ryeos_state::objects::SourceExecutionPolicyIdentity::Executor {
            declarer_ref: policy.declarer.canonical_ref.clone(),
            signer_fingerprint: policy
                .declarer
                .signer_fingerprint
                .clone()
                .ok_or_else(|| anyhow::anyhow!("source policy declarer has no signer"))?,
            source_content_digest: policy.declarer.content_hash.clone(),
            raw_content_digest: policy.declarer.raw_content_digest.clone(),
            policy_digest: policy.policy.digest()?,
            chain_digest: policy.chain_digest.clone(),
        }
    } else {
        ryeos_state::objects::SourceExecutionPolicyIdentity::Worker {
            source_declaration_digest: worker_declaration
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("worker source declaration is absent"))?
                .identity_digest()?,
        }
    };
    let logical_binding = if let Some(policy) = executor_policy {
        match policy.policy.location {
            ryeos_engine::source_closure::ExecutorSourceLocation::ItemNamespace => {
                ryeos_state::objects::SourceLogicalBinding::Tool {
                    loader_roots: policy.policy.load_roots.clone(),
                    root_entry: root_entry.clone(),
                }
            }
            ryeos_engine::source_closure::ExecutorSourceLocation::ItemDirectory => {
                ryeos_state::objects::SourceLogicalBinding::ToolDirectory {
                    loader_roots: policy.policy.load_roots.clone(),
                    root: item_directory_binding_root(&resolution.root.resolved_ref)?,
                    root_entry: root_entry.clone(),
                }
            }
        }
    } else {
        ryeos_state::objects::SourceLogicalBinding::Worker {
            root: worker_declaration
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("worker source declaration is absent"))?
                .root
                .clone(),
            entry: root_entry.clone(),
        }
    };
    let binding = ryeos_state::objects::EffectiveSourceBinding {
        schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
        kind: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_KIND.to_owned(),
        owner,
        kind_ceiling: ryeos_state::objects::SignedKindSourceCeiling {
            schema_ref: kind_evidence.canonical_ref.clone(),
            source_content_digest: kind_evidence.source_content_digest.clone(),
            raw_content_digest: kind_evidence.raw_content_digest.clone(),
            signer_fingerprint: kind_evidence.signer_fingerprint.clone(),
            signature_header: kind_evidence.signature_header.clone(),
            schema_body: kind_evidence.body.clone(),
            schema_document: kind_evidence.document.clone(),
            normalized_declaration,
            root_kind_format: serde_json::json!({
                "extension": root_format.ext,
                "parser": root_format.parser,
                "signature": root_format.signature,
            }),
            root_signature_envelope: serde_json::to_value(&root_format.signature)?,
        },
        content_manifest_hash: content_manifest_hash.clone(),
        testimony,
        execution_policy,
        logical_binding,
    };
    binding.validate()?;
    binding.validate_content_manifest(&candidate.manifest)?;
    ensure!(
        candidate.revalidate(source.as_ref(), &requests, contract)
            == ryeos_engine::source_closure::SourceAuthorityStatus::Current,
        "source authority changed during capture and testimony"
    );

    if let Some(preview) = preview {
        let expected_digest = match &binding.testimony {
            ryeos_state::objects::SourceTestimonyProof::OwnerSignedDigest {
                expected_manifest_hash,
            } => Some(expected_manifest_hash.clone()),
            ryeos_state::objects::SourceTestimonyProof::OwnerSignedFiles { .. } => None,
        };
        *preview = Some(SourceClosureValidationPreview {
            owner_ref: binding.owner.canonical_ref.clone(),
            expected_digest,
            observed_digest: content_manifest_hash,
            binding_digest: Some(binding.digest()?),
            file_count: candidate.manifest.totals.file_count,
            total_bytes: candidate.manifest.totals.total_bytes,
            ready_for_admission: true,
            status: "matched".to_owned(),
        });
        return Ok(None);
    }

    Ok(Some(CapturedSourceClosure { candidate, binding }))
}

#[allow(clippy::too_many_arguments)]
pub fn preview_source_closure(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    project: Option<(&Path, &dyn AuthoritativeProjectContent, String)>,
    executor_policy: Option<&ExecutorSourcePolicyProjection>,
) -> anyhow::Result<Option<SourceClosureValidationPreview>> {
    let mut resolution = resolution.clone();
    let mut publication = None;
    let mut preview = None;
    let admitted = admit_source_closure_in_publication(
        state,
        engine,
        kind,
        &mut resolution,
        roots,
        project,
        executor_policy,
        &mut publication,
        Some(&mut preview),
    )?;
    if admitted.is_some() || publication.is_some() {
        anyhow::bail!("static source validation attempted to mint launch authority");
    }
    Ok(preview)
}

pub fn recover_source_closure(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<Option<AdmittedSourceClosure>> {
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
    else {
        return Ok(None);
    };
    let projection = ryeos_state::objects::EffectiveSourceClosureProjection::from_value(value)?;
    let authority = state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    let binding_value = cas
        .get_object(&projection.binding_hash)?
        .ok_or_else(|| anyhow::anyhow!("retained source binding is absent"))?;
    let binding = ryeos_state::objects::EffectiveSourceBinding::from_value(&binding_value)?;
    let manifest_value = cas
        .get_object(&projection.content_manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("retained source manifest is absent"))?;
    let manifest = ryeos_state::objects::SourceClosureManifest::from_value(&manifest_value)?;
    if binding.content_manifest_hash != projection.content_manifest_hash
        || manifest.digest()? != projection.content_manifest_hash
        || binding.digest()? != projection.binding_hash
        || binding.owner_key()? != projection.owner_key
        || manifest.totals.file_count != projection.file_count
        || manifest.totals.total_bytes != projection.total_bytes
    {
        anyhow::bail!("retained source closure contradicts its effective projection");
    }
    binding.validate_content_manifest(&manifest)?;
    if source_owner_identity(resolution)? != binding.owner {
        anyhow::bail!("retained source owner contradicts its admitted resolution");
    }
    let schema =
        require_current_kind_testimony(&engine.node_trust_store, &engine.trust_store, &binding)?;
    require_current_source_policy(
        &engine.node_trust_store,
        &engine.trust_store,
        resolution,
        &binding,
        &schema,
    )?;
    let store = RetainedSourceClosureStore { authority };
    let proof = ryeos_engine::source_closure::prove_source_closure(projection, &store)?;
    Ok(Some(AdmittedSourceClosure {
        proof,
        store,
        publication: None,
        binding,
        manifest,
    }))
}

fn require_current_kind_testimony(
    node_trust: &ryeos_engine::trust::TrustStore,
    item_trust: &ryeos_engine::trust::TrustStore,
    binding: &ryeos_state::objects::EffectiveSourceBinding,
) -> anyhow::Result<ryeos_engine::kind_registry::KindSchema> {
    if item_trust.get(&binding.owner.signer_fingerprint).is_none()
        && node_trust.get(&binding.owner.signer_fingerprint).is_none()
    {
        anyhow::bail!("retained source testimony signer is no longer trusted");
    }
    let evidence = ryeos_engine::kind_registry::KindSchemaEvidence {
        canonical_ref: binding.kind_ceiling.schema_ref.clone(),
        source_content_digest: binding.kind_ceiling.source_content_digest.clone(),
        raw_content_digest: binding.kind_ceiling.raw_content_digest.clone(),
        signer_fingerprint: binding.kind_ceiling.signer_fingerprint.clone(),
        signature_header: binding.kind_ceiling.signature_header.clone(),
        body: binding.kind_ceiling.schema_body.clone(),
        document: binding.kind_ceiling.schema_document.clone(),
    };
    let schema =
        ryeos_engine::kind_registry::verify_retained_kind_schema_evidence(&evidence, node_trust)?;
    let declaration = schema
        .execution
        .as_ref()
        .and_then(|execution| execution.source_closure.as_ref())
        .ok_or_else(|| anyhow::anyhow!("retained kind no longer proves source authority"))?;
    if serde_json::to_value(declaration)? != binding.kind_ceiling.normalized_declaration {
        anyhow::bail!("retained source ceiling contradicts its signed kind declaration");
    }
    let extension = binding
        .kind_ceiling
        .root_kind_format
        .get("extension")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("retained source format has no extension"))?;
    let format = schema.spec_for(extension).ok_or_else(|| {
        anyhow::anyhow!("retained source format extension is absent from its kind")
    })?;
    let expected_format = serde_json::json!({
        "extension": format.ext,
        "parser": format.parser,
        "signature": format.signature,
    });
    if expected_format != binding.kind_ceiling.root_kind_format
        || serde_json::to_value(&format.signature)? != binding.kind_ceiling.root_signature_envelope
    {
        anyhow::bail!("retained source format contradicts its signed kind");
    }
    Ok(schema)
}

fn require_current_source_policy(
    node_trust: &ryeos_engine::trust::TrustStore,
    item_trust: &ryeos_engine::trust::TrustStore,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    binding: &ryeos_state::objects::EffectiveSourceBinding,
    schema: &ryeos_engine::kind_registry::KindSchema,
) -> anyhow::Result<()> {
    match &binding.execution_policy {
        ryeos_state::objects::SourceExecutionPolicyIdentity::Executor {
            signer_fingerprint,
            ..
        } => {
            if item_trust.get(signer_fingerprint).is_none()
                && node_trust.get(signer_fingerprint).is_none()
            {
                anyhow::bail!("retained source policy signer is no longer trusted");
            }
        }
        ryeos_state::objects::SourceExecutionPolicyIdentity::Worker {
            source_declaration_digest,
        } => {
            let contract = schema
                .execution
                .as_ref()
                .and_then(|execution| execution.source_closure.as_ref())
                .ok_or_else(|| anyhow::anyhow!("retained worker kind has no source contract"))?;
            let declaration = ryeos_engine::source_closure::WorkerSourceDeclaration::from_composed(
                &resolution.composed.composed,
                Some(contract),
            )?
            .ok_or_else(|| anyhow::anyhow!("retained worker source declaration is absent"))?;
            if declaration.identity_digest()? != *source_declaration_digest
                || declaration.digest != binding.content_manifest_hash
            {
                anyhow::bail!("retained worker source policy contradicts its signed declaration");
            }
        }
    }
    Ok(())
}

fn verify_owner_signed_files(
    candidate: &CapturedSourceCandidate,
    root_entry: &str,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> anyhow::Result<ryeos_state::objects::SourceTestimonyProof> {
    let owner = resolution
        .root
        .signer_fingerprint
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!("source-owning project item has no verified signer fingerprint")
        })?;
    let mut testimony = Vec::with_capacity(candidate.manifest.entries.len());
    let mut root_matched = false;
    for entry in &candidate.manifest.entries {
        let blob = candidate
            .blobs
            .iter()
            .find(|blob| blob.blob_hash == entry.blob_hash)
            .ok_or_else(|| anyhow::anyhow!("source testimony blob is absent"))?;
        let content = std::str::from_utf8(&blob.bytes)
            .map_err(|_| anyhow::anyhow!("signed source file is not UTF-8"))?;
        let extension = Path::new(&entry.path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!(".{extension}"))
            .ok_or_else(|| anyhow::anyhow!("source file has no kind-owned extension"))?;
        let format = kind_schema.spec_for(&extension).ok_or_else(|| {
            anyhow::anyhow!("source file extension `{extension}` is not owned by the root kind")
        })?;
        let header =
            ryeos_engine::item_resolution::parse_signature_header(content, &format.signature)
                .ok_or_else(|| anyhow::anyhow!("source file `{}` is unsigned", entry.path))?;
        if header.signer_fingerprint != owner {
            anyhow::bail!(
                "source file `{}` is signed by a foreign principal",
                entry.path
            );
        }
        let (trust, signer) = ryeos_engine::trust::verify_item_signature(
            content,
            &header,
            &format.signature,
            trust_store,
        )?;
        if trust != ryeos_engine::contracts::TrustClass::Trusted
            || signer.as_ref().map(|signer| signer.0.as_str()) != Some(owner)
        {
            anyhow::bail!(
                "source file `{}` has no current owner testimony",
                entry.path
            );
        }
        if entry.path == root_entry {
            root_matched = true;
            if entry.blob_hash != resolution.root.source_content_digest {
                anyhow::bail!("source root entry differs from the resolved executable bytes");
            }
        }
        testimony.push(serde_json::json!({
            "path": &entry.path,
            "blob_hash": &entry.blob_hash,
            "signer": owner,
            "content_hash": header.content_hash,
        }));
    }
    if !root_matched {
        anyhow::bail!("source closure does not contain its resolved root entry");
    }
    let entries_digest = lillux::sha256_hex(
        lillux::canonical_json(&serde_json::Value::Array(testimony))?.as_bytes(),
    );
    Ok(
        ryeos_state::objects::SourceTestimonyProof::OwnerSignedFiles {
            signer_fingerprint: owner.to_owned(),
            file_count: candidate.manifest.entries.len(),
            entries_digest,
        },
    )
}

fn require_manifest_entry(candidate: &CapturedSourceCandidate, entry: &str) -> anyhow::Result<()> {
    if !candidate
        .manifest
        .entries
        .iter()
        .any(|file| file.root == "source" && file.path == entry)
    {
        anyhow::bail!("source closure entry `{entry}` is absent after capture policy filtering");
    }
    Ok(())
}

pub(crate) fn source_owner_identity(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<ryeos_state::objects::SourceOwnerIdentity> {
    let signer =
        resolution.root.signer_fingerprint.clone().ok_or_else(|| {
            anyhow::anyhow!("source-owning item has no verified signer fingerprint")
        })?;
    let (item_kind, logical_item_key) = resolution
        .root
        .resolved_ref
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("source owner ref is not canonical"))?;
    let (source_space, source_root) =
        match (resolution.root.source_space, &resolution.root.source_root) {
            (ItemSpace::Project, ItemSourceRoot::Project) => (
                ryeos_state::objects::SourceSpaceIdentity::Project,
                ryeos_state::objects::SourceRootIdentity::Project,
            ),
            (ItemSpace::Bundle, ItemSourceRoot::Bundle { name }) => (
                ryeos_state::objects::SourceSpaceIdentity::Bundle,
                ryeos_state::objects::SourceRootIdentity::Bundle { name: name.clone() },
            ),
            (ItemSpace::Node, ItemSourceRoot::Node) => (
                ryeos_state::objects::SourceSpaceIdentity::Node,
                ryeos_state::objects::SourceRootIdentity::Node,
            ),
            _ => anyhow::bail!("source owner has incoherent typed root authority"),
        };
    Ok(ryeos_state::objects::SourceOwnerIdentity {
        canonical_ref: resolution.root.resolved_ref.clone(),
        item_kind: item_kind.to_owned(),
        source_space,
        source_root,
        root_source_content_digest: resolution.root.source_content_digest.clone(),
        root_raw_content_digest: resolution.root.raw_content_digest.clone(),
        signer_fingerprint: signer,
        logical_item_key: logical_item_key.to_owned(),
    })
}

fn root_identity_key(root: &ItemSourceRoot) -> String {
    match root {
        ItemSourceRoot::Project => "project".to_owned(),
        ItemSourceRoot::Bundle { name } => format!("bundle:{name}"),
        ItemSourceRoot::Node => "node".to_owned(),
        ItemSourceRoot::Search { label } => format!("search:{label}"),
    }
}

struct ProjectSourceContent<'a> {
    content: &'a dyn AuthoritativeProjectContent,
    identity: String,
}

impl AuthoritativeSourceContent for ProjectSourceContent<'_> {
    fn authority_identity(&self) -> Result<String, ryeos_engine::error::EngineError> {
        Ok(self.identity.clone())
    }

    fn list_files(
        &self,
        prefix: &Path,
        max_entries: usize,
    ) -> Result<Vec<ProjectContentEntry>, ryeos_engine::error::EngineError> {
        self.content.list_files(prefix, true, max_entries)
    }

    fn read_file(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, ryeos_engine::error::EngineError> {
        self.content.read_file(path, max_bytes)
    }
}

/// Temporary C2 source-only fixture. It uses production admission/publication
/// and recovery; it supplies no protected backend, product or funded owner.
#[cfg(test)]
pub(crate) mod temporary_source_fixture {
    use super::*;
    use ryeos_engine::resolution::{
        KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
    };

    pub(crate) struct Fixture {
        pub(crate) resolution: ResolutionOutput,
        pub(crate) recovered: AdmittedSourceClosure,
        // Keep the actual staged CAS roots alive through every member read.
        pub(crate) publication: PendingCasPublication,
        pub(crate) bundle: tempfile::TempDir,
    }

    pub(crate) fn admit(state: &mut AppState, member: &[u8]) -> anyhow::Result<Fixture> {
        admit_with_logical_root(state, member, "source")
    }

    pub(crate) fn admit_with_logical_root(
        state: &mut AppState,
        member: &[u8],
        logical_root: &str,
    ) -> anyhow::Result<Fixture> {
        admit_with_logical_root_and_member(state, member, logical_root, "startup.json")
    }

    pub(crate) fn admit_with_logical_root_and_member(
        state: &mut AppState,
        member: &[u8],
        logical_root: &str,
        member_path: &str,
    ) -> anyhow::Result<Fixture> {
        admit_variant(state, member, logical_root, member_path, false)
    }

    pub(crate) fn admit_current_target(state: &mut AppState, member: &[u8]) -> anyhow::Result<Fixture> {
        admit_variant(state, member, "source", "startup.json", true)
    }

    fn admit_variant(state: &mut AppState, member: &[u8], logical_root: &str,
        member_path: &str, current: bool) -> anyhow::Result<Fixture> {
        // Validate both relative coordinates before allocation or writes.
        ryeos_state::objects::validate_canonical_project_relative_path(logical_root)?;
        ryeos_state::objects::validate_canonical_project_relative_path(member_path)?;
        anyhow::ensure!(
            member_path != "bootstrap.py",
            "temporary member replaces entry"
        );
        let bundle = tempfile::tempdir()?;
        let kinds = bundle.path().join("fixture-kinds");
        std::fs::create_dir_all(kinds.join("worker"))?;
        let key = lillux::crypto::SigningKey::from_bytes(&[113; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: fingerprint.clone(),
                verifying_key: key.verifying_key(),
                label: None,
            },
        ]);
        let schema = r##"location:
  directory: workers
execution:
  aliases:
    "@base": "tool:ryeos/base/base"
  source_closure:
    derived: effective_source_closure
    location: {type: owner_relative_source, path: [source]}
    testimony: owner_signed_digest
    max_files: 8
    max_total_bytes: 65536
    max_file_bytes: 32768
    max_depth: 8
formats:
  - extensions: [".yaml"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: "#"
metadata:
  rules:
    name:
      from: filename
composed_value_contract:
  root_type: mapping
  required: {}
effective_trust:
  include_references: false
resolution: []
composer: handler:ryeos/core/identity
"##;
        let schema = if current {
            schema.replace("execution:\n", "execution:\n  target:\n    path: [supported_target]\n    required: true\n    request_format: ryeos.execution_target.v1\n    limits: {max_requirements: 1, max_resource_count: 1, max_facts_per_requirement: 16}\n")
        } else { schema.to_owned() };
        std::fs::write(
            kinds.join("worker/worker.kind-schema.yaml"),
            lillux::signature::sign_content(&schema, &key, "#", None),
        )?;
        let registry = ryeos_engine::kind_registry::KindRegistry::load_base(&[kinds], &trust)?;
        let contract = registry
            .get("worker")
            .and_then(|kind| kind.execution.as_ref())
            .and_then(|execution| execution.source_closure.as_ref())
            .context("temporary signed kind has no source contract")?;
        let prefix = Path::new(".ai/workers/test").join(logical_root);
        std::fs::create_dir_all(bundle.path().join(&prefix))?;
        // An inert test entry, never executed and never a production consumer.
        std::fs::write(
            bundle.path().join(&prefix).join("bootstrap.py"),
            b"# C2 source only\n",
        )?;
        let member_file = bundle.path().join(&prefix).join(member_path);
        std::fs::create_dir_all(
            member_file
                .parent()
                .context("temporary member parent absent")?,
        )?;
        std::fs::write(member_file, member)?;
        let source = DirectorySourceContent::new(
            bundle.path(),
            "bundle:factory-fixture".into(),
            state.ignore_matcher.as_ref(),
        )?;
        let candidate = ryeos_engine::source_closure::capture_source_candidate(
            &source,
            &[SourceRootRequest {
                id: "source".into(),
                selection: SourceRootSelection::Tree { prefix },
            }],
            contract,
        )?;
        let declaration = ryeos_engine::source_closure::WorkerSourceDeclaration {
            root: logical_root.into(),
            entry: "bootstrap.py".into(),
            digest: candidate.manifest.digest()?,
        };
        declaration.validate()?;
        let mut definition = serde_json::json!({
            "kind": "worker", "name": "factory", "category": "test", "source": declaration,
        });
        if current {
            definition["supported_target"] = serde_json::json!({"schema":"ryeos.execution_target.v1",
                "os":"linux","arch":"x86_64","resources":[],"original_physical_bounds":null});
        }
        let body = serde_yaml::to_string(&definition)?;
        let document = lillux::signature::sign_content(&body, &key, "#", None);
        let header = lillux::signature::parse_signature_line(
            document
                .lines()
                .next()
                .context("temporary signature absent")?,
            "#",
            None,
        )
        .context("temporary signature malformed")?;
        ensure!(
            lillux::signature::is_valid_signature_for(
                &header.content_hash,
                &header.signature_b64,
                &header.signer_fingerprint,
                &body,
                &key.verifying_key(),
                &fingerprint,
            ),
            "temporary Worker signature invalid"
        );
        let descriptor = bundle.path().join(".ai/workers/test/factory.yaml");
        std::fs::write(&descriptor, &document)?;
        // Resolution is test input derived from the verified actual document;
        // no admitted source/proof owner is constructed here.
        let mut resolution = ResolutionOutput {
            root: ResolvedAncestor {
                requested_id: "worker:test/factory".into(),
                resolved_ref: "worker:test/factory".into(),
                source_path: descriptor,
                source_space: ItemSpace::Bundle,
                source_root: ItemSourceRoot::Bundle {
                    name: "factory-fixture".into(),
                },
                trust_class: TrustClass::TrustedBundle,
                signer_fingerprint: Some(fingerprint),
                alias_resolution: None,
                added_by: ResolutionStepName::PipelineInit,
                raw_content: body.clone(),
                source_content_digest: lillux::sha256_hex(document.as_bytes()),
                raw_content_digest: lillux::sha256_hex(body.as_bytes()),
            },
            ancestors: vec![],
            references_edges: vec![],
            referenced_items: vec![],
            step_outputs: Default::default(),
            effective_trust_class: TrustClass::TrustedBundle,
            composed: KindComposedView::identity(serde_yaml::from_str(&body)?),
        };
        let mut engine = (*state.engine).clone();
        engine.kinds = registry;
        engine.node_trust_store = trust.clone();
        engine.trust_store = trust;
        state.engine = std::sync::Arc::new(engine);
        let roots = ryeos_engine::item_resolution::ResolutionRoots::from_registered(
            None,
            &[ryeos_engine::item_resolution::RegisteredBundleRoot {
                name: "factory-fixture".into(),
                canonical_root: bundle.path().canonicalize()?,
            }],
        );
        let admitted = admit_source_closure(
            state,
            &state.engine,
            "worker",
            &mut resolution,
            &roots,
            None,
            None,
        )?
        .context("actual temporary source admission returned None")?;
        let publication = admitted
            .into_publication()
            .context("actual admission did not retain its CAS publication")?;
        let recovered = recover_source_closure(state, &state.engine, &resolution)?
            .context("actual temporary source recovery returned None")?;
        Ok(Fixture {
            resolution,
            recovered,
            publication,
            bundle,
        })
    }
}

#[cfg(test)]
mod directory_source_tests {
    #[test]
    fn actual_source_cas_authority_refuses_foreign_original_guard() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        // A thread holds one test Store at a time. The foreign node's
        // descriptor-pinned authority outlives its Store, so that node is
        // opened, pinned and closed before the actual node opens.
        let foreign = {
            let (_second_state_owner, second_state) =
                crate::state::test_support::build(second.path()).unwrap();
            second_state.state_store.pinned_state_authority().unwrap()
        };
        let (_first_state_owner, first_state) =
            crate::state::test_support::build(first.path()).unwrap();
        let actual = first_state.state_store.pinned_state_authority().unwrap();
        let same_guard = actual.acquire_shared_guard().unwrap();
        actual.ensure_guard(&same_guard).unwrap();
        // A thread nests shared guards of one runtime only, so release the
        // actual guard before taking the foreign one.
        drop(same_guard);
        let foreign_guard = foreign.acquire_shared_guard().unwrap();
        assert!(actual.ensure_guard(&foreign_guard).is_err());
        // Actual state/CAS authority control for the helper's pre-read gate.
        // No recovered source or protected original input owner is fabricated;
        // full recovered-member/binder custody controls are still required.
    }


}

struct RetainedSourceClosureStore {
    authority: ryeos_state::PinnedStateAuthority,
}

impl SourceClosureStore for RetainedSourceClosureStore {
    fn source_closure_available(
        &self,
        binding_hash: &str,
        manifest_hash: &str,
    ) -> anyhow::Result<bool> {
        let cas = self.authority.cas_store()?;
        let Some(binding_value) = cas.get_object(binding_hash)? else {
            return Ok(false);
        };
        let binding = ryeos_state::objects::EffectiveSourceBinding::from_value(&binding_value)?;
        if binding.digest()? != binding_hash || binding.content_manifest_hash != manifest_hash {
            return Ok(false);
        }
        let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
            &cas,
            [binding_hash.to_owned()],
            ryeos_state::object_closure::ObjectClosureLimits::default(),
        )?;
        Ok(closure.is_complete())
    }
}

#[cfg(test)]
mod retained_current_worker_source_controls {
    fn plan(resolution: &ryeos_engine::resolution::ResolutionOutput,
        engine: &ryeos_engine::engine::Engine) -> ryeos_engine::contracts::ExecutionPlan {
        let (ordinary, current) = engine.kinds.get("worker").unwrap().execution.as_ref().unwrap()
            .project_target_data(&resolution.composed.composed).unwrap();
        let value = serde_json::json!({"plan_id":"source-data-only", "root_executor_id":"tool:test/runtime",
            "root_ref":resolution.root.resolved_ref,"item_kind":"worker", "nodes":[],"entrypoint":"entry",
            "capabilities":{"requires_model":false,"requires_subprocess":true,"requires_network":false,"custom":[]},
            "materialization_requirements":[], "network_authority_ceiling":"node_policy","filesystem_authority_ceiling":"node_policy",
            "target_requirement":ordinary,"current_target_request":current,"endpoint_requirement":{"kind":"local"},
            "external_endpoint_binding":null,"resource_authority_ceiling":"node_policy","subprocess_limits":null,
            "cache_key":"data","executor_authorities":[]});
        serde_json::from_value(value).unwrap()
    }
    #[test]
    fn genuine_signed_source_reproduction_detects_dropped_carrier_owner_and_schema_substitution() {
        let root = tempfile::tempdir().unwrap();
        let (_state_owner, mut state) = crate::state::test_support::build(root.path()).unwrap();
        let mut fixture = super::temporary_source_fixture::admit_current_target(&mut state, b"{}").unwrap();
        let current = plan(&fixture.resolution, &state.engine);
        // Real signed kind/subject and actual CAS admission/recovery; plan is
        // DATA only, not an actual verified original/native session owner.
        fixture.recovered.verify_retained_worker_target_data(&state.engine, &fixture.resolution, &current).unwrap();
        assert!(current.validate_endpoint_for_sealing().is_err());
        let mut dropped = current.clone(); dropped.current_target_request = None;
        assert!(fixture.recovered.verify_retained_worker_target_data(&state.engine, &fixture.resolution, &dropped).is_err());
        let mut foreign = fixture.resolution.clone(); foreign.root.resolved_ref = "worker:test/foreign".into();
        assert!(fixture.recovered.verify_retained_worker_target_data(&state.engine, &foreign, &current).is_err());
        let old = fixture.recovered.binding.kind_ceiling.schema_body.clone();
        fixture.recovered.binding.kind_ceiling.schema_body = old.replace("ryeos.execution_target.v1", "unknown");
        assert!(fixture.recovered.verify_retained_worker_target_data(&state.engine, &fixture.resolution, &current).is_err());
        fixture.recovered.binding.kind_ceiling.schema_body = old;
        let mut revoked = (*state.engine).clone();
        revoked.node_trust_store = ryeos_engine::trust::TrustStore::from_signers(vec![]);
        assert!(fixture.recovered.verify_retained_worker_target_data(&revoked, &fixture.resolution, &current).is_err());
        // Fixture constructs resolution from signature-verified real document;
        // it does not execute an actual composer handler or native factory.
    }
}
