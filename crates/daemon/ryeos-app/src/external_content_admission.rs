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
use serde_json::Value;

use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::state::AppState;

/// Exact, non-secret reason a retained content realization cannot be admitted.
///
/// Absence is an operator-correctable resource state, not an internal launch
/// failure.  Keeping it typed lets generic execution surfaces report a stable
/// code without matching an error string or learning anything about the
/// workload that requested the content.
#[derive(Debug, thiserror::Error)]
#[error("external-content consumer has no active operator binding")]
pub struct ExternalContentBindingUnavailable {
    pub manifest_hash: String,
    pub consumer_ref: String,
}

fn require_admission_binding(
    state: &AppState,
    cas: &lillux::CasStore,
    manifest_hash: &str,
    consumer: &ryeos_state::objects::ExternalContentConsumerAuthority,
) -> anyhow::Result<ryeos_state::objects::ExternalContentBinding> {
    crate::operator_external_content::require_active_binding(state, cas, manifest_hash, consumer)
        .map_err(|error| {
            if error
                .downcast_ref::<crate::operator_external_content::BindingNotActive>()
                .is_some()
            {
                return ExternalContentBindingUnavailable {
                    manifest_hash: manifest_hash.to_owned(),
                    consumer_ref: consumer.consumer_ref().to_owned(),
                }
                .into();
            }
            error
        })
}

/// Called after the existing source-admission pass, by both binding and launch.
/// Declarative programs can have no executable source closure; do not invent
/// one or infer source-loading authority from the external-content declaration.
pub(crate) fn consumer_authority(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    subject_resolution_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
) -> anyhow::Result<ryeos_state::objects::ExternalContentConsumerAuthority> {
    let publisher = resolution.root.signer_fingerprint.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "locator-free external-content consumer has no verified publisher fingerprint"
        )
    })?;
    match (resolution.root.source_space, &resolution.root.source_root) {
        (
            ryeos_engine::contracts::ItemSpace::Bundle,
            ryeos_engine::contracts::ItemSourceRoot::Bundle { .. },
        ) => {
            // A bundle-provided consumer can deliberately compose product
            // relationship definitions from a pinned project. In that case
            // the executable bytes remain bundle-owned, but the effective
            // pre-realization program and its relationship closure are
            // generation-scoped.
            let Some(project_snapshot_hash) = subject_resolution_authority.operational_generation()
            else {
                return ryeos_state::objects::ExternalContentConsumerAuthority::installed_bundle(
                    resolution.root.resolved_ref.clone(),
                    publisher,
                );
            };
            pinned_project_consumer_authority(resolution, publisher, project_snapshot_hash)
        }
        (
            ryeos_engine::contracts::ItemSpace::Project,
            ryeos_engine::contracts::ItemSourceRoot::Project,
        ) => {
            let project_snapshot_hash = subject_resolution_authority
                .operational_generation()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "project external-content consumer requires exact generation authority"
                    )
                })?;
            pinned_project_consumer_authority(resolution, publisher, project_snapshot_hash)
        }
        _ => anyhow::bail!(
            "external-content consumer has incoherent or unsupported source authority"
        ),
    }
}

fn pinned_project_consumer_authority(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    publisher: String,
    project_snapshot_hash: &str,
) -> anyhow::Result<ryeos_state::objects::ExternalContentConsumerAuthority> {
    let source_closure = resolution
        .composed
        .derived
        .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
        .map(ryeos_state::objects::EffectiveSourceClosureProjection::from_value)
        .transpose()?;
    let effective_consumer_digest =
        ryeos_engine::external_content::pre_external_realization_consumer_digest(resolution)?;
    ryeos_state::objects::ExternalContentConsumerAuthority::pinned_project(
        resolution.root.resolved_ref.clone(),
        publisher,
        project_snapshot_hash.to_owned(),
        effective_consumer_digest,
        source_closure,
    )
}

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
    pub observed_digest: Option<String>,
    pub binding_digest: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalContentValidationPreview {
    pub declarations: Vec<ExternalContentPinPreview>,
    pub ready_for_admission: bool,
}

/// Read-side result for a locator-free preparer-selected content dependency.
/// Realizations are present only when every exact retained manifest and
/// binding is currently ready; they are never serialized by public callers.
pub struct PortableContentDependencyPreview {
    pub validation: ExternalContentValidationPreview,
    pub realizations: Option<RealizedExternalContentSet>,
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
    subject_resolution_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
) -> anyhow::Result<Option<ExternalContentValidationPreview>> {
    let contract = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.external_content_contract());
    let declarer = ryeos_engine::external_content::declaring_authority(resolution)?;
    let Some(declarations) =
        ryeos_engine::external_content::effective_external_content_declarations(
            resolution, contract, declarer,
        )?
    else {
        return Ok(None);
    };

    let mut budget = LaunchCaptureBudget::default();
    let mut sink = DigestOnlyExternalContentSink;
    let mut previews = Vec::with_capacity(declarations.len());
    let mut ready_for_admission = true;
    for declaration in &declarations {
        let (observed_digest, binding_digest, status, ready) = match declaration.locator.as_ref() {
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
                let observed = ryeos_state::external_content_manifest_digest(&manifest)?;
                let status = match declaration.mode {
                    ryeos_engine::external_content::ExternalContentMode::Captured => "captured",
                    ryeos_engine::external_content::ExternalContentMode::Pinned
                        if declaration.digest.as_deref() == Some(observed.as_str()) =>
                    {
                        "matched"
                    }
                    ryeos_engine::external_content::ExternalContentMode::Pinned => "mismatched",
                };
                let ready = status != "mismatched";
                (Some(observed), None, status, ready)
            }
            None => preview_retained_external_content(
                state,
                contract,
                resolution,
                subject_resolution_authority,
                declaration,
            )?,
        };
        ready_for_admission &= ready;
        previews.push(ExternalContentPinPreview {
            id: declaration.id.clone(),
            expected_digest: declaration.digest.clone(),
            observed_digest,
            binding_digest,
            status: status.to_owned(),
        });
    }
    validate_retained_declaration_totals(state, contract, &declarations)?;
    Ok(Some(ExternalContentValidationPreview {
        declarations: previews,
        ready_for_admission,
    }))
}

/// Preview one preparer-selected portable content dependency through the same
/// retained-manifest and exact-binding checks used by ordinary declarations.
/// Prepared content dependencies are deliberately locator-free, so this path
/// never opens an ambient named root and never stages a publication.
pub fn preview_portable_content_dependency(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    policy: &ryeos_engine::runtime_registry::LaunchContentExternalPolicy,
    subject_resolution_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
) -> anyhow::Result<ExternalContentValidationPreview> {
    Ok(preview_portable_content_dependency_with_realizations(
        state,
        resolution,
        policy,
        subject_resolution_authority,
    )?
    .validation)
}

pub fn preview_portable_content_dependency_with_realizations(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    policy: &ryeos_engine::runtime_registry::LaunchContentExternalPolicy,
    subject_resolution_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
) -> anyhow::Result<PortableContentDependencyPreview> {
    let contract = policy.declaration_contract();
    let declarer = ryeos_engine::external_content::declaring_authority(resolution)?;
    let declarations = ryeos_engine::external_content::effective_external_content_declarations(
        resolution,
        Some(&contract),
        declarer,
    )?
    .ok_or_else(|| anyhow::anyhow!("content dependency has no external_content declaration"))?;
    if declarations.is_empty() {
        anyhow::bail!("content dependency has no external_content declaration");
    }
    let mut previews = Vec::with_capacity(declarations.len());
    let mut ready_for_admission = true;
    for declaration in &declarations {
        if declaration.locator.is_some() {
            anyhow::bail!(
                "portable content dependency `{}` contains an ambient locator",
                declaration.id
            );
        }
        let (observed_digest, binding_digest, status, ready) = preview_retained_external_content(
            state,
            Some(&contract),
            resolution,
            subject_resolution_authority,
            declaration,
        )?;
        ready_for_admission &= ready;
        previews.push(ExternalContentPinPreview {
            id: declaration.id.clone(),
            expected_digest: declaration.digest.clone(),
            observed_digest,
            binding_digest,
            status: status.to_owned(),
        });
    }
    validate_retained_declaration_totals(state, Some(&contract), &declarations)?;
    let realizations = if ready_for_admission {
        Some(retained_realization_set(state, &declarations)?)
    } else {
        None
    };
    Ok(PortableContentDependencyPreview {
        validation: ExternalContentValidationPreview {
            declarations: previews,
            ready_for_admission,
        },
        realizations,
    })
}

/// Check storage-tier grants without changing the exact consumer binding.
/// Also used for the aggregate content supplied to a prepared execution target.
pub fn validate_retained_declaration_totals(
    state: &AppState,
    contract: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
    declarations: &[ExternalContentDeclaration],
) -> anyhow::Result<()> {
    let authority = pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    validate_retained_declaration_totals_with_cas(&cas, contract, declarations)
}

fn validate_retained_declaration_totals_with_cas(
    cas: &lillux::CasStore,
    contract: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
    declarations: &[ExternalContentDeclaration],
) -> anyhow::Result<()> {
    let mut ordinary_total = 0u64;
    let mut large_total = 0u64;
    for declaration in declarations {
        let Some(digest) = declaration
            .locator
            .is_none()
            .then_some(declaration.digest.as_deref())
            .flatten()
        else {
            continue;
        };
        let Some(value) = cas.get_object(digest)? else {
            continue;
        };
        match value.get("kind").and_then(Value::as_str) {
            Some(ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND) => {
                let manifest =
                    ryeos_state::objects::ExternalContentManifestObject::from_value(&value)?;
                ordinary_total = ordinary_total
                    .checked_add(manifest.total_bytes)
                    .ok_or_else(|| {
                        anyhow::anyhow!("external-content realization byte total overflow")
                    })?;
                if ordinary_total > ryeos_state::objects::MAX_EXTERNAL_CONTENT_TOTAL_BYTES {
                    anyhow::bail!(
                        "retained content realizations exceed the content-tier launch bound"
                    );
                }
            }
            Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
                let manifest =
                    ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&value)?;
                large_total = large_total
                    .checked_add(manifest.total_bytes)
                    .ok_or_else(|| {
                        anyhow::anyhow!("large-content realization byte total overflow")
                    })?;
                let grant = contract
                    .and_then(|contract| contract.large_content.as_ref())
                    .ok_or_else(|| anyhow::anyhow!(
                        "external content `{}` names a large manifest without a signed large-content grant",
                        declaration.id
                    ))?;
                let ceiling = grant
                    .max_total_bytes
                    .unwrap_or(ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES);
                if large_total > ceiling {
                    anyhow::bail!(
                        "large-content realizations exceed the signed {ceiling}-byte grant"
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn retained_realization_set(
    state: &AppState,
    declarations: &[ExternalContentDeclaration],
) -> anyhow::Result<RealizedExternalContentSet> {
    let authority = pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let mut realized = Vec::with_capacity(declarations.len());
    for declaration in declarations {
        let digest = declaration.digest.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "locator-free external content `{}` has no retained digest",
                declaration.id
            )
        })?;
        let value = cas.get_object(digest)?.ok_or_else(|| {
            anyhow::anyhow!("retained external-content manifest disappeared during validation")
        })?;
        let (entry_count, total_bytes) = match value.get("kind").and_then(Value::as_str) {
            Some(ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND) => {
                let manifest =
                    ryeos_state::objects::ExternalContentManifestObject::from_value(&value)?;
                (manifest.entry_count, manifest.total_bytes)
            }
            Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
                let manifest =
                    ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&value)?;
                (manifest.entry_count, manifest.total_bytes)
            }
            Some(other) => {
                anyhow::bail!("retained external-content manifest has unsupported kind `{other}`")
            }
            None => anyhow::bail!("retained external-content manifest is untyped"),
        };
        realized.push(RealizedExternalContent {
            id: declaration.id.clone(),
            kind: declaration.kind,
            mode: declaration.mode,
            manifest_hash: digest.to_owned(),
            entry_count,
            total_bytes,
            mount_root: declaration.mount_root,
            mount: declaration.mount.clone(),
        });
    }
    RealizedExternalContentSet::new(realized)
}

fn preview_retained_external_content(
    state: &AppState,
    contract: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    subject_resolution_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
    declaration: &ExternalContentDeclaration,
) -> anyhow::Result<(Option<String>, Option<String>, &'static str, bool)> {
    if declaration.mode != ryeos_engine::external_content::ExternalContentMode::Pinned {
        anyhow::bail!(
            "locator-free external content `{}` is not pinned",
            declaration.id
        );
    }
    let digest = declaration.digest.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "locator-free external content `{}` has no retained digest",
            declaration.id
        )
    })?;
    let consumer = consumer_authority(resolution, subject_resolution_authority)?;
    let authority = pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let Some(value) = cas.get_object(digest)? else {
        return Ok((None, None, "missing_manifest", false));
    };
    match value.get("kind").and_then(Value::as_str) {
        Some(ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND) => {
            let verified = ryeos_state::VerifiedExternalContentClosure::load(&cas, digest)?;
            if declaration.kind == ExternalContentKind::File
                && !verified.manifest().is_file_shaped()
            {
                anyhow::bail!(
                    "external content `{}` declares a file but manifest {digest} is not file-shaped",
                    declaration.id
                );
            }
        }
        Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
            if contract
                .and_then(|contract| contract.large_content.as_ref())
                .is_none()
            {
                anyhow::bail!(
                    "external content `{}` names a large manifest without a signed large-content grant",
                    declaration.id
                );
            }
            let manifest = ryeos_state::objects::load_if_large_content_manifest(&cas, digest)?
                .ok_or_else(|| anyhow::anyhow!("large-content manifest changed kind"))?;
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
            let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                &cas,
                [digest.to_owned()],
                state
                    .node_policy
                    .require::<NodeObjectClosurePolicy>()?
                    .closure_limits()?,
            )?;
            if !closure.is_complete() {
                anyhow::bail!("large-content realization closure is incomplete");
            }
        }
        Some(other) => anyhow::bail!(
            "external content `{}` manifest {digest} has unsupported kind `{other}`",
            declaration.id
        ),
        None => anyhow::bail!(
            "external content `{}` manifest {digest} is untyped",
            declaration.id
        ),
    }
    let binding = crate::operator_external_content::active_binding_from_store(
        &state.state_store,
        &cas,
        digest,
        &consumer,
        state.identity.fingerprint(),
    )?;
    drop(guard);
    match binding {
        Some((binding_digest, binding)) => {
            crate::operator_external_content::require_current_binding_authorizer(state, &binding)?;
            Ok((Some(digest.to_owned()), Some(binding_digest), "ready", true))
        }
        None => Ok((Some(digest.to_owned()), None, "missing_binding", false)),
    }
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
    let store = ExternalRealizationStore::new(
        pinned_state_authority(state)?,
        state
            .node_policy
            .require::<NodeObjectClosurePolicy>()?
            .closure_limits()?,
    );
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
    subject_resolution_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
    inherited: Option<&RealizedExternalContentSet>,
) -> anyhow::Result<Option<AdmittedExternalRealizations>> {
    let mut publication = None;
    let mut admitted = admit_external_realizations_in_publication(
        state,
        engine,
        kind,
        resolution,
        roots,
        subject_resolution_authority,
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
    subject_resolution_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
    inherited: Option<&RealizedExternalContentSet>,
    publication: &mut Option<PendingCasPublication>,
) -> anyhow::Result<Option<AdmittedExternalRealizations>> {
    let contract = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.external_content_contract());
    let declarer = ryeos_engine::external_content::declaring_authority(resolution)?;
    let Some(declarations) =
        ryeos_engine::external_content::effective_external_content_declarations(
            resolution, contract, declarer,
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
        subject_resolution_authority,
        inherited,
        publication,
        kind,
    )
}

/// Admit the locator-free pinned declarations of a prepared content
/// dependency. The signed launch policy supplies only mechanical ceilings;
/// manifest identity and consumer binding remain owned by the resolved item
/// and the existing external-content subsystem.
///
/// Portable does not mean projectless: a bound project item keeps the exact
/// subject generation already admitted by its outer launch. Pass that authority
/// through preview and admission; never infer it from a path, current HEAD, or
/// the installed execution dependency receiving the content.
pub fn admit_portable_content_dependency_in_publication(
    state: &AppState,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    policy: &ryeos_engine::runtime_registry::LaunchContentExternalPolicy,
    subject_resolution_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
    inherited: Option<&RealizedExternalContentSet>,
    publication: &mut Option<PendingCasPublication>,
) -> anyhow::Result<AdmittedExternalRealizations> {
    let contract = policy.declaration_contract();
    let declarer = ryeos_engine::external_content::declaring_authority(resolution)?;
    let declarations = ryeos_engine::external_content::effective_external_content_declarations(
        resolution,
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
        subject_resolution_authority,
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
    contract: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
    declarations: Vec<ExternalContentDeclaration>,
    subject_resolution_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
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
    let retained_consumer = declarations
        .iter()
        .any(|declaration| declaration.locator.is_none())
        .then(|| consumer_authority(resolution, subject_resolution_authority))
        .transpose()?;
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
                retained_consumer
                    .as_ref()
                    .expect("locator-free declaration resolved consumer authority"),
            )?);
            continue;
        }
        if declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
            && declaration.locator.is_none()
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
                retained_consumer
                    .as_ref()
                    .expect("locator-free declaration resolved consumer authority"),
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
            mount_root: declaration.mount_root,
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
    let store = ExternalRealizationStore::new(
        proof_authority,
        state
            .node_policy
            .require::<NodeObjectClosurePolicy>()?
            .closure_limits()?,
    );
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
    consumer: &ryeos_state::objects::ExternalContentConsumerAuthority,
) -> anyhow::Result<RealizedExternalContent> {
    require_admission_binding(state, cas, digest, consumer)?;
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
        mount_root: declaration.mount_root,
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
    let store = ExternalRealizationStore::new(
        pinned_state_authority(state)?,
        state
            .node_policy
            .require::<NodeObjectClosurePolicy>()?
            .closure_limits()?,
    );
    let proof = ryeos_engine::external_realization::prove_external_realizations(realized, &store)?;
    Ok(Some(AdmittedExternalRealizations {
        proof,
        store,
        publication: None,
    }))
}

struct ExternalRealizationStore {
    authority: ryeos_state::PinnedStateAuthority,
    // Retain the selected node's admitted budget across the engine's
    // meaning-blind proof interface. Never substitute closure defaults here.
    closure_limits: ryeos_state::object_closure::ObjectClosureLimits,
}

impl ExternalRealizationStore {
    fn new(
        authority: ryeos_state::PinnedStateAuthority,
        closure_limits: ryeos_state::object_closure::ObjectClosureLimits,
    ) -> Self {
        Self {
            authority,
            closure_limits,
        }
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
                self.closure_limits,
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
    contract: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    cas: &lillux::CasStore,
    sink: &mut GuardedCasBlobSink<'_>,
    large_total: &mut u64,
    state: &AppState,
    consumer: &ryeos_state::objects::ExternalContentConsumerAuthority,
) -> anyhow::Result<RealizedExternalContent> {
    let grant = contract
        .and_then(|contract| contract.large_content.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "external content `{}` names a large manifest without a signed large-content grant",
                declaration.id
            )
        })?;
    require_admission_binding(state, cas, digest, consumer)?;
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
        mount_root: declaration.mount_root,
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

#[cfg(test)]
mod consumer_authority_tests {
    use super::*;
    use ryeos_engine::contracts::{ItemSourceRoot, SubjectResolutionAuthority};
    use ryeos_engine::resolution::{
        KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
    };

    #[test]
    fn exact_consumer_projection_preserves_source_presence_and_rejects_malformed_evidence() {
        let mut resolution = ResolutionOutput {
            root: ResolvedAncestor {
                requested_id: "project/build".into(),
                resolved_ref: "tool:project/build".into(),
                source_path: "/fixture/.ai/tools/project/build.yaml".into(),
                source_space: ItemSpace::Project,
                source_root: ItemSourceRoot::Project,
                trust_class: TrustClass::TrustedProject,
                signer_fingerprint: Some("a".repeat(64)),
                alias_resolution: None,
                added_by: ResolutionStepName::PipelineInit,
                raw_content: String::new(),
                source_content_digest: "b".repeat(64),
                raw_content_digest: "c".repeat(64),
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: Default::default(),
            effective_trust_class: TrustClass::TrustedProject,
            composed: KindComposedView::identity(serde_json::json!({
                "executor_id": "@subprocess",
                "config": {"command": "realization:platform/bin/compiler"}
            })),
        };
        let generation = SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: "d".repeat(64),
        };
        let declarative = consumer_authority(&resolution, &generation).unwrap();
        assert!(declarative.source_closure().is_none());
        assert!(consumer_authority(&resolution, &SubjectResolutionAuthority::LiveFs).is_err());
        let source = ryeos_state::objects::EffectiveSourceClosureProjection {
            schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
            binding_hash: "e".repeat(64),
            content_manifest_hash: "f".repeat(64),
            owner_key: "1".repeat(64),
            file_count: 1,
            total_bytes: 1,
        };
        resolution.composed.derived.insert(
            ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
            serde_json::to_value(&source).unwrap(),
        );
        let source_owning = consumer_authority(&resolution, &generation).unwrap();
        assert_eq!(source_owning.source_closure(), Some(&source));
        assert_ne!(source_owning, declarative);

        resolution.root.source_space = ItemSpace::Bundle;
        resolution.root.source_root = ItemSourceRoot::Bundle {
            name: "standard".into(),
        };
        resolution.root.trust_class = TrustClass::TrustedBundle;
        let bundle_with_project_relationships =
            consumer_authority(&resolution, &generation).unwrap();
        assert!(matches!(
            bundle_with_project_relationships,
            ryeos_state::objects::ExternalContentConsumerAuthority::PinnedProject { .. }
        ));
        assert_eq!(
            bundle_with_project_relationships.source_closure(),
            Some(&source)
        );
        assert!(matches!(
            consumer_authority(&resolution, &SubjectResolutionAuthority::Projectless).unwrap(),
            ryeos_state::objects::ExternalContentConsumerAuthority::InstalledBundle { .. }
        ));

        resolution.root.source_space = ItemSpace::Project;
        resolution.root.source_root = ItemSourceRoot::Project;
        resolution.root.trust_class = TrustClass::TrustedProject;
        resolution.composed.derived.insert(
            ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
            Value::Null,
        );
        assert!(consumer_authority(&resolution, &generation).is_err());
    }
}

#[cfg(test)]
mod content_contract_tests {
    use super::*;

    #[test]
    fn retained_content_target_totals_enforce_the_combined_ordinary_tier() {
        let root = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(root.path().join("cas"));
        // Manifest-only budget test: actual payload verification is owned by
        // realization admission, not this aggregate-metadata check.
        let bytes = 25 * 1024 * 1024;
        let manifest =
            ryeos_state::objects::ExternalContentManifestObject::from_value(&serde_json::json!({
            "schema":ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA,
                "kind":ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
                "entry_count":6, "total_bytes":6 * bytes,
                "entries":(0..6).map(|i| serde_json::json!({
                    "path":format!("file-{i}"), "kind":"file", "mode":420,
                    "blob_hash":"a".repeat(64), "size":bytes
                })).collect::<Vec<_>>()
            }))
            .unwrap();
        let hash = cas
            .store_object(&serde_json::to_value(&manifest).unwrap())
            .unwrap();
        let declaration = |id| {
            serde_json::from_value::<ExternalContentDeclaration>(serde_json::json!({
                "id":id, "kind":"tree", "mode":"pinned", "digest":hash,
                "mount_root":"project", "mount":id
            }))
            .unwrap()
        };
        let own = declaration("own");
        let contributed = declaration("contributed");
        validate_retained_declaration_totals_with_cas(&cas, None, std::slice::from_ref(&own))
            .unwrap();
        validate_retained_declaration_totals_with_cas(
            &cas,
            None,
            std::slice::from_ref(&contributed),
        )
        .unwrap();
        assert!(
            validate_retained_declaration_totals_with_cas(&cas, None, &[own, contributed])
                .unwrap_err()
                .to_string()
                .contains("content-tier launch bound")
        );
    }

    #[test]
    fn retained_content_target_totals_require_a_large_grant_even_for_tiny_payloads() {
        let root = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(root.path().join("cas"));
        let blob = cas.store_blob(b"x").unwrap();
        let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&serde_json::json!({
            "schema":ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA,
            "kind":ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
            "entry_count":1, "total_bytes":1,
            "entries":[{"path":"content", "kind":"file", "mode":420, "blob_hash":blob, "size":1}]
        })).unwrap();
        let hash = cas.store_object(&manifest.to_value().unwrap()).unwrap();
        let declaration = |id: &str| {
            serde_json::from_value::<ExternalContentDeclaration>(serde_json::json!({
                "id":id, "kind":"file", "mode":"pinned", "digest":hash,
                "mount_root":"project", "mount":id
            }))
            .unwrap()
        };
        let first = declaration("first");
        let mut contract = ryeos_engine::kind_registry::KindExternalContentDecl {
            realization_derived: "effective_external_realizations".into(),
            allowed_roots: vec![],
            allowed_mount_roots: vec![ryeos_state::objects::ExternalContentMountRoot::Project],
            max_declarations: 8,
            large_content: None,
        };
        let check = |contract: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
                     declarations: &[ExternalContentDeclaration]| {
            validate_retained_declaration_totals_with_cas(&cas, contract, declarations)
        };
        assert!(
            check(None, std::slice::from_ref(&first))
                .unwrap_err()
                .to_string()
                .contains("without a signed large-content grant")
        );
        assert!(check(Some(&contract), std::slice::from_ref(&first)).is_err());
        contract.large_content = Some(ryeos_engine::kind_registry::KindLargeContentGrant {
            max_total_bytes: Some(1),
        });
        check(Some(&contract), std::slice::from_ref(&first)).unwrap();
        let combined = [first, declaration("second")];
        assert!(
            check(Some(&contract), &combined)
                .unwrap_err()
                .to_string()
                .contains("exceed the signed 1-byte grant")
        );
        contract.large_content.as_mut().unwrap().max_total_bytes = None;
        check(Some(&contract), &combined).unwrap();
    }
}
