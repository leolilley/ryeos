//! Admission, recovery, and execution of kind-neutral persistent sessions.
//!
//! A kind owns the executable definition and declares the mechanical session
//! contract.  This module does not interpret request or response bodies.  It
//! captures that already-resolved definition into an immutable capsule before
//! the outer runtime capsule is minted, then reopens only retained content.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_app::persistent_session::StartedPersistentSession;
use ryeos_app::runtime_db::{WorkerProcessRecord, WorkerProcessState, daemon_generation_id};
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::{ResolvedExecutionRequest, prepare_captured_item_plan};
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
    SubjectResolutionAuthority,
};
use ryeos_engine::kind_registry::{PersistentSessionDecl, TerminatorDecl};
use ryeos_engine::protocols::descriptor::{
    PersistentSessionCleanupAuthority, PersistentSessionProcessMode,
};
use ryeos_engine::protocols::{VerifiedProtocol, validate_persistent_session_protocol};
use ryeos_state::objects::{
    AdmittedPersistentSessionCapsule, ExecutableSearchPathEntry, PERSISTENT_SESSION_CAPSULE_KIND,
    PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION, PersistentSessionAuthority,
    PersistentSessionLifecycleContract, PersistentSessionWireContract,
};

use super::launch_preparation::{
    PreparedContentDependency, PreparedEvidenceAttachment, PreparedExecutionDependency,
    PreparedRuntimeLaunch, RefBindingLaunchRecord,
};

/// Closed, secret-free recovery diagnostic vocabulary for persistent-session
/// capsule verification. The wrapped error remains available to local tracing,
/// but durable terminal testimony may retain only these reviewed labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SessionCapsuleVerificationStage {
    #[error("session-capsule/load")]
    Load,
    #[error("session-capsule/retained-protocol")]
    RetainedProtocol,
    #[error("session-capsule/process-control")]
    ProcessControl,
    #[error("session-capsule/exact-program")]
    ExactProgram,
    #[error("session-capsule/captured-dependency")]
    CapturedDependency,
    #[error("session-capsule/executable-search")]
    ExecutableSearch,
    #[error("session-capsule/process-environment")]
    ProcessEnvironment,
    #[error("session-capsule/evidence-attachments")]
    EvidenceAttachments,
    #[error("session-capsule/evidence-validation")]
    EvidenceValidation,
    #[error("session-capsule/effective-definition")]
    EffectiveDefinition,
    #[error("session-capsule/effective-definition-digest")]
    EffectiveDefinitionDigest,
    #[error("session-capsule/current-trust")]
    CurrentTrust,
    #[error("session-capsule/protocol-identity")]
    ProtocolIdentity,
    #[error("session-capsule/source-closure")]
    SourceClosure,
    #[error("session-capsule/external-realizations")]
    ExternalRealizations,
    #[error("session-capsule/target-content")]
    TargetContent,
    #[error("session-capsule/persistent-session-realization")]
    PersistentSessionRealization,
}

impl SessionCapsuleVerificationStage {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 17] = [
        Self::Load,
        Self::RetainedProtocol,
        Self::ProcessControl,
        Self::ExactProgram,
        Self::CapturedDependency,
        Self::ExecutableSearch,
        Self::ProcessEnvironment,
        Self::EvidenceAttachments,
        Self::EvidenceValidation,
        Self::EffectiveDefinition,
        Self::EffectiveDefinitionDigest,
        Self::CurrentTrust,
        Self::ProtocolIdentity,
        Self::SourceClosure,
        Self::ExternalRealizations,
        Self::TargetContent,
        Self::PersistentSessionRealization,
    ];

    pub(crate) fn stable_label(self) -> &'static str {
        match self {
            Self::Load => "session-capsule/load",
            Self::RetainedProtocol => "session-capsule/retained-protocol",
            Self::ProcessControl => "session-capsule/process-control",
            Self::ExactProgram => "session-capsule/exact-program",
            Self::CapturedDependency => "session-capsule/captured-dependency",
            Self::ExecutableSearch => "session-capsule/executable-search",
            Self::ProcessEnvironment => "session-capsule/process-environment",
            Self::EvidenceAttachments => "session-capsule/evidence-attachments",
            Self::EvidenceValidation => "session-capsule/evidence-validation",
            Self::EffectiveDefinition => "session-capsule/effective-definition",
            Self::EffectiveDefinitionDigest => "session-capsule/effective-definition-digest",
            Self::CurrentTrust => "session-capsule/current-trust",
            Self::ProtocolIdentity => "session-capsule/protocol-identity",
            Self::SourceClosure => "session-capsule/source-closure",
            Self::ExternalRealizations => "session-capsule/external-realizations",
            Self::TargetContent => "session-capsule/target-content",
            Self::PersistentSessionRealization => "session-capsule/persistent-session-realization",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PersistentSessionEligibilityPreview {
    pub(crate) ready_for_admission: bool,
    pub(crate) status: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExecutionDependencyValidationPreview {
    pub(crate) canonical_ref: String,
    pub(crate) resolution: ryeos_engine::resolution::AsLaunchedResolutionDigest,
    pub(crate) source: Option<ryeos_app::source_closure_admission::SourceClosureValidationPreview>,
    pub(crate) external_content:
        Option<ryeos_app::external_content_admission::ExternalContentValidationPreview>,
    pub(crate) session: Option<PersistentSessionEligibilityPreview>,
    pub(crate) admission_ready: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContentDependencyValidationPreview {
    pub(crate) binding: String,
    pub(crate) canonical_ref: String,
    pub(crate) resolution: ryeos_engine::resolution::AsLaunchedResolutionDigest,
    pub(crate) targets: Vec<String>,
    pub(crate) executable_search: Vec<ryeos_handler_protocol::ExecutableSearchPathEntryWire>,
    pub(crate) external_content_policy: ryeos_engine::runtime_registry::LaunchContentExternalPolicy,
    pub(crate) external_content:
        ryeos_app::external_content_admission::ExternalContentValidationPreview,
    pub(crate) admission_ready: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EnvironmentContributionValidationPreview {
    pub(crate) targets: Vec<String>,
    pub(crate) variables: BTreeMap<String, ryeos_handler_protocol::LaunchEnvironmentValueWire>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PreparedDependencyValidationPreview {
    pub(crate) binding_records: BTreeMap<String, RefBindingLaunchRecord>,
    pub(crate) execution_dependencies: BTreeMap<String, ExecutionDependencyValidationPreview>,
    pub(crate) content_dependencies: BTreeMap<String, ContentDependencyValidationPreview>,
    pub(crate) environment_contributions:
        BTreeMap<String, EnvironmentContributionValidationPreview>,
    pub(crate) admission_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistentSessionExactProgram {
    pub(crate) effective_definition_digest: String,
    pub(crate) resolution_output: ryeos_engine::resolution::RetainedResolutionOutput,
    pub(crate) evidence_attachments: Vec<PreparedEvidenceAttachment>,
}

fn validate_exact_evidence_attachments(exact: &PersistentSessionExactProgram) -> Result<()> {
    let mut previous: Option<&str> = None;
    for binding in &exact.evidence_attachments {
        binding.validate()?;
        if previous.is_some_and(|value| value >= binding.binding_id.as_str()) {
            bail!("persistent-session evidence attachments are not canonically ordered");
        }
        previous = Some(&binding.binding_id);
    }
    Ok(())
}

fn evidence_realizations(
    bindings: &[PreparedEvidenceAttachment],
) -> Result<ryeos_engine::external_realization::RealizedExternalContentSet> {
    let entries = bindings
        .iter()
        .map(|binding| {
            binding.validate()?;
            Ok(
                ryeos_engine::external_realization::RealizedExternalContent {
                    id: format!("ev-{}", binding.binding_id),
                    kind: ryeos_state::objects::ExternalContentKind::File,
                    mode: ryeos_state::objects::ExternalContentMode::Pinned,
                    manifest_hash: binding.manifest_hash.clone(),
                    entry_count: 1,
                    total_bytes: binding.size_bytes,
                    mount_root: ryeos_state::objects::ExternalContentMountRoot::Project,
                    mount: binding.destination_path.clone(),
                },
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ryeos_engine::external_realization::RealizedExternalContentSet::new(entries)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedPersistentSessionIdentity {
    pub canonical_ref: String,
    pub effective_definition_digest: String,
    pub capsule_hash: String,
    pub execution_realization_hash: String,
}

#[derive(Debug, Clone)]
pub struct ExclusivePersistentSessionIdentity {
    pub placement_thread_id: String,
    pub worker_instance_id: String,
    pub boot_identity_hash: String,
    pub boot_epoch: u64,
    pub lifecycle_generation: u64,
    pub control_channel_identity: String,
}

/// Typed evidence for the caller that a start failure crossed process
/// creation and RyeOS could not prove cleanup. The caller must preserve the
/// durable worker/profile fence instead of terminalizing the session.
#[derive(Debug)]
pub struct ExclusiveWorkerCleanupUnproved;

impl std::fmt::Display for ExclusiveWorkerCleanupUnproved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("exclusive worker cleanup is unproved")
    }
}

impl std::error::Error for ExclusiveWorkerCleanupUnproved {}

struct HeldPersistentSession {
    process: ryeos_app::thread_lifecycle::SpawnedPersistentSessionAwaitingAttachment,
    socket: lillux::InheritedDuplexChannel,
    lifelines: Vec<Box<dyn Send + Sync>>,
}

pub(crate) struct AdmittedSessionPublications {
    publications: Vec<ryeos_state::PendingCasPublication>,
}

impl AdmittedSessionPublications {
    pub(crate) fn include_evidence_publication(
        &mut self,
        publication: Option<ryeos_state::PendingCasPublication>,
    ) {
        self.publications.extend(publication);
    }
}

/// Resolve invocation-supplied bundle-event attachments into exact retained
/// launch bindings. Authorization and identity verification happen here,
/// before any persistent worker can be admitted or spawned. The returned CAS
/// publication keeps newly synthesized file manifests and their already-local
/// event/blob inputs rooted until the outer launch capsule is committed.
pub(crate) fn prepare_or_verify_evidence_attachments(
    state: &AppState,
    policy: &ryeos_engine::runtime_registry::LaunchEvidenceAttachmentPolicy,
    principal: &EffectivePrincipal,
    parameters: &Value,
    prepared: &mut PreparedRuntimeLaunch,
    recovered: bool,
) -> Result<Option<ryeos_state::PendingCasPublication>> {
    let requests = parameters
        .as_object()
        .and_then(|object| object.get("evidence_attachments"))
        .map(|value| {
            serde_json::from_value::<Vec<ryeos_handler_protocol::EvidenceAttachmentRequestWire>>(
                value.clone(),
            )
            .context("decode evidence_attachments")
        })
        .transpose()?
        .unwrap_or_default();
    validate_evidence_request_shape(policy, &requests, prepared)?;

    let scopes = super::launch_preparation::principal_scopes(principal);
    let authorizer = ryeos_runtime::authorizer::Authorizer::new();
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;

    let mut observed = Vec::with_capacity(requests.len());
    let mut total_bytes = 0u64;
    for request in &requests {
        let required = ryeos_bundle::runtime_authority::bundle_event_cap(
            &ryeos_bundle::manifest::BundleEventOperation::Scan,
            &request.bundle_id,
            &request.event_kind,
        );
        authorizer
            .authorize(
                &scopes,
                &ryeos_runtime::authorizer::AuthorizationPolicy::require(&required),
            )
            .with_context(|| {
                format!(
                    "evidence attachment `{}` is not authorized for exact bundle-event scan `{required}`",
                    request.binding_id
                )
            })?;
        let (record, attachment, bytes) = state.state_store.read_bundle_event_attachment(
            &request.event_hash,
            &request.bundle_id,
            &request.event_kind,
            &request.attachment_name,
        )?;
        record.event.validate()?;
        if record.event_hash != request.event_hash
            || ryeos_state::objects::hash_bundle_event(&record.event)? != request.event_hash
            || record.event.chain_id != request.chain_id
            || attachment.blob_hash != request.blob_hash
            || lillux::sha256_hex(&bytes) != request.blob_hash
        {
            bail!(
                "evidence attachment `{}` differs from its exact event, chain, or blob coordinate",
                request.binding_id
            );
        }
        total_bytes = total_bytes
            .checked_add(attachment.size_bytes)
            .ok_or_else(|| anyhow!("evidence attachment byte total overflow"))?;
        if total_bytes > policy.max_total_bytes {
            bail!("evidence attachments exceed the signed aggregate byte ceiling");
        }
        let manifest = evidence_file_manifest(&attachment);
        manifest.validate()?;
        let manifest_value = serde_json::to_value(&manifest)?;
        let manifest_hash = lillux::sha256_hex(lillux::canonical_json(&manifest_value)?.as_bytes());
        observed.push((request, attachment, manifest, manifest_hash));
    }

    if recovered {
        let mut expected = observed
            .into_iter()
            .map(|(request, attachment, _, manifest_hash)| {
                prepared_evidence_attachment(policy, request, &attachment, manifest_hash)
            })
            .collect::<Result<Vec<_>>>()?;
        expected.sort_by(|left, right| left.binding_id.cmp(&right.binding_id));
        if expected != prepared.evidence_attachments {
            bail!("recovered evidence attachments contradict the admitted launch binding");
        }
        let cas = authority.cas_store()?;
        for binding in &prepared.evidence_attachments {
            binding.validate()?;
            let value = cas
                .get_object(&binding.manifest_hash)?
                .ok_or_else(|| anyhow!("admitted evidence attachment manifest is missing"))?;
            let manifest = ryeos_state::objects::ExternalContentManifestObject::from_value(&value)?;
            if manifest != evidence_file_manifest_from_binding(binding) {
                bail!("admitted evidence attachment manifest changed");
            }
        }
        return Ok(None);
    }
    if !prepared.evidence_attachments.is_empty() {
        bail!("fresh launch already carries prepared evidence attachments");
    }
    if observed.is_empty() {
        return Ok(None);
    }

    let staged = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(&guard, "evidence-attachments")?;
    let mut publication = ryeos_state::PendingCasPublication::new(authority, staged);
    let authority = publication.authority().try_clone()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow!("cannot acquire evidence-attachment CAS write permit: {error}"))?;
    let cas = authority.cas_store()?;
    let mut bindings = Vec::with_capacity(observed.len());
    for (request, attachment, manifest, expected_manifest_hash) in observed {
        let stored = publication.staged_roots_mut().store_object_admitted(
            &guard,
            &cas,
            &serde_json::to_value(&manifest)?,
        )?;
        if stored != expected_manifest_hash {
            bail!("evidence attachment manifest hash changed during admission");
        }
        publication
            .staged_roots_mut()
            .protect_object_hash_admitted(&guard, &request.event_hash)?;
        publication
            .staged_roots_mut()
            .protect_blob_hash_admitted(&guard, &request.blob_hash)?;
        bindings.push(prepared_evidence_attachment(
            policy,
            request,
            &attachment,
            stored,
        )?);
    }
    bindings.sort_by(|left, right| left.binding_id.cmp(&right.binding_id));
    prepared.evidence_attachments = bindings;
    Ok(Some(publication))
}

fn validate_evidence_request_shape(
    policy: &ryeos_engine::runtime_registry::LaunchEvidenceAttachmentPolicy,
    requests: &[ryeos_handler_protocol::EvidenceAttachmentRequestWire],
    prepared: &PreparedRuntimeLaunch,
) -> Result<()> {
    if requests.len() > usize::from(policy.max_attachments) {
        bail!("evidence attachments exceed the signed count ceiling");
    }
    if requests.is_empty() {
        return Ok(());
    }
    let target = policy
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("runtime does not admit evidence attachments"))?;
    if !prepared.execution_dependencies.contains_key(target) {
        bail!("evidence attachment target is not a prepared execution dependency");
    }
    let prefix = policy
        .destination_prefix
        .as_deref()
        .ok_or_else(|| anyhow!("runtime has no evidence destination authority"))?;
    if policy.allowed_access.as_slice()
        != [ryeos_handler_protocol::EvidenceAttachmentAccessWire::ReadOnly]
    {
        bail!("runtime does not admit read-only evidence attachments");
    }
    let mut ids = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    for request in requests {
        if request.binding_id.is_empty()
            || request.binding_id.len() > 61
            || !request.binding_id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
            || !ids.insert(request.binding_id.as_str())
        {
            bail!("evidence attachment binding_id is invalid or duplicated");
        }
        for (label, value) in [
            ("bundle_id", request.bundle_id.as_str()),
            ("event_kind", request.event_kind.as_str()),
            ("chain_id", request.chain_id.as_str()),
            ("attachment_name", request.attachment_name.as_str()),
        ] {
            ryeos_state::objects::validate_bundle_identifier(label, value)?;
        }
        if !lillux::valid_hash(&request.event_hash) || !lillux::valid_hash(&request.blob_hash) {
            bail!("evidence attachment carries a non-canonical hash");
        }
        validate_evidence_destination(prefix, &request.destination_path)?;
        if !destinations.insert(request.destination_path.as_str()) {
            bail!("evidence attachment destination is duplicated");
        }
        if request.access != ryeos_handler_protocol::EvidenceAttachmentAccessWire::ReadOnly {
            bail!("evidence attachments currently require read_only access");
        }
    }
    Ok(())
}

fn validate_evidence_destination(prefix: &str, destination: &str) -> Result<()> {
    ryeos_state::objects::validate_canonical_project_relative_path(destination)?;
    if !destination.starts_with(&format!("{prefix}/")) {
        bail!("evidence attachment destination must be contained by `{prefix}`");
    }
    Ok(())
}

fn evidence_file_manifest(
    attachment: &ryeos_state::objects::BundleEventAttachment,
) -> ryeos_state::objects::ExternalContentManifestObject {
    ryeos_state::objects::ExternalContentManifestObject {
        schema: ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA.to_owned(),
        kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
        entries: vec![ryeos_state::objects::ExternalContentManifestEntry {
            path: ryeos_state::objects::FILE_REALIZATION_ENTRY_PATH.to_owned(),
            kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
            mode: Some(0o644),
            blob_hash: Some(attachment.blob_hash.clone()),
            size: Some(attachment.size_bytes),
            target: None,
        }],
        entry_count: 1,
        total_bytes: attachment.size_bytes,
    }
}

fn evidence_file_manifest_from_binding(
    binding: &PreparedEvidenceAttachment,
) -> ryeos_state::objects::ExternalContentManifestObject {
    evidence_file_manifest(&ryeos_state::objects::BundleEventAttachment {
        name: binding.attachment_name.clone(),
        blob_hash: binding.blob_hash.clone(),
        size_bytes: binding.size_bytes,
        media_type: binding.media_type.clone(),
    })
}

fn prepared_evidence_attachment(
    policy: &ryeos_engine::runtime_registry::LaunchEvidenceAttachmentPolicy,
    request: &ryeos_handler_protocol::EvidenceAttachmentRequestWire,
    attachment: &ryeos_state::objects::BundleEventAttachment,
    manifest_hash: String,
) -> Result<PreparedEvidenceAttachment> {
    let mut binding = PreparedEvidenceAttachment {
        binding_id: request.binding_id.clone(),
        bundle_id: request.bundle_id.clone(),
        event_kind: request.event_kind.clone(),
        chain_id: request.chain_id.clone(),
        event_hash: request.event_hash.clone(),
        attachment_name: request.attachment_name.clone(),
        blob_hash: request.blob_hash.clone(),
        size_bytes: attachment.size_bytes,
        media_type: attachment.media_type.clone(),
        target: policy.target.clone().expect("validated evidence target"),
        destination_path: request.destination_path.clone(),
        access: request.access,
        manifest_hash,
        binding_digest: String::new(),
    };
    binding.binding_digest = binding.reproduce_binding_digest()?;
    binding.validate()?;
    Ok(binding)
}

impl AdmittedSessionPublications {
    pub(crate) fn publish(self) -> Result<()> {
        for publication in self.publications {
            publication.publish()?;
        }
        Ok(())
    }
}

/// Remove only node-local admission output before independently admitting a
/// transferred portable program on another node. Authored/composed values and
/// the captured dependency remain exact; source closure, external realization,
/// and session capsule coordinates are recomputed from target-local authority.
pub(crate) fn reset_for_cross_site_admission(
    engine: &ryeos_engine::engine::Engine,
    principal: &EffectivePrincipal,
    prepared: &mut PreparedRuntimeLaunch,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    subject_resolution_authority: &SubjectResolutionAuthority,
) -> Result<()> {
    if prepared
        .content_dependencies
        .values()
        .any(|dependency| !dependency.product_selections.is_empty())
    {
        bail!("product selections require target-local composition before cross-site admission");
    }
    // This is a new receiving-node admission, not same-node recovery. Preserve
    // the portable contract exactly, but refuse a node that cannot admit its
    // captured ceilings rather than silently replacing or widening them.
    for dependency in prepared.content_dependencies.values() {
        dependency.validate()?;
        let source = ryeos_engine::canonical_ref::CanonicalRef::parse(&dependency.canonical_ref)?;
        let receiving = engine
            .kinds
            .get(&source.kind)
            .and_then(|kind| kind.external_content_contract());
        require_receiving_content_contract(
            &dependency.external_content_policy.declaration_contract(),
            receiving,
        )?;
        for (target, retained) in &dependency.target_content_contracts {
            let target = prepared
                .execution_dependencies
                .get(target)
                .ok_or_else(|| anyhow!("transferred content target is absent"))?
                .captured_verified_subject()?;
            let receiving = engine
                .kinds
                .get(&target.resolved.kind)
                .and_then(|kind| kind.external_content_contract());
            require_receiving_content_contract(retained, receiving)?;
        }
    }
    prepared.admitted_sessions.clear();
    for dependency in prepared.execution_dependencies.values_mut() {
        dependency.validate()?;
        let mut source_resolution = dependency.resolution.clone();
        clear_node_local_admission_projections(&mut source_resolution);

        let canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(&dependency.canonical_ref)?;
        let plan_context = PlanContext {
            requested_by: principal.clone(),
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: "cross-site-admission".to_owned(),
            origin_site_id: "cross-site-admission".to_owned(),
            execution_hints: ExecutionHints::default(),
            scheduled_fire: None,
            validate_only: true,
        };
        let target_resolved = engine.resolve(&plan_context, &canonical)?;
        let target_verified = engine.verify(&plan_context, target_resolved)?;
        let target_resolution =
            engine.effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
                item_ref: canonical,
                expected_kind: None,
                project_root: None,
                subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            })?;
        let source_portable =
            ryeos_engine::resolution::RetainedResolutionOutput::capture(&source_resolution);
        let target_portable =
            ryeos_engine::resolution::RetainedResolutionOutput::capture(&target_resolution);
        if serde_json::to_value(&source_portable)? != serde_json::to_value(&target_portable)? {
            bail!(
                "target dependency `{}` differs from the transferred portable program",
                dependency.canonical_ref
            );
        }

        let target_subject = super::launch_preparation::PreparedExecutionDependencySubject {
            source_path: target_verified.resolved.source_path.clone(),
            source_space: target_verified.resolved.source_space,
            source_root: target_verified.resolved.source_root.clone(),
            resolved_from: target_verified.resolved.resolved_from.clone(),
            materialized_project_root: target_verified.resolved.materialized_project_root.clone(),
            subject_resolution_authority: target_verified
                .resolved
                .subject_resolution_authority
                .clone(),
            raw_content_digest: target_verified.resolved.raw_content_digest.clone(),
            content_hash: target_verified.resolved.content_hash.clone(),
            signature_header: target_verified.resolved.signature_header.clone(),
            source_format: target_verified.resolved.source_format.clone(),
            metadata: target_verified.resolved.metadata.clone(),
            signer: target_verified.signer.clone(),
            trust_class: target_verified.trust_class,
        };
        if portable_subject_value(&dependency.subject)? != portable_subject_value(&target_subject)?
        {
            bail!(
                "target dependency `{}` verification authority differs from the transferred program",
                dependency.canonical_ref
            );
        }
        dependency.resolution = target_resolution;
        dependency.subject = target_subject;
        dependency.validate()?;
    }
    for dependency in prepared.content_dependencies.values_mut() {
        dependency.validate()?;
        let mut source_resolution = dependency.resolution.restore();
        clear_node_local_admission_projections(&mut source_resolution);
        let canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(&dependency.canonical_ref)?;
        let target_resolution =
            engine.effective_resolution_output(transferred_content_resolution_request(
                canonical,
                source_resolution.root.source_space,
                roots,
                subject_resolution_authority,
            )?)?;
        let source_portable =
            ryeos_engine::resolution::RetainedResolutionOutput::capture(&source_resolution);
        let target_portable =
            ryeos_engine::resolution::RetainedResolutionOutput::capture(&target_resolution);
        if serde_json::to_value(&source_portable)? != serde_json::to_value(&target_portable)? {
            bail!(
                "target content dependency `{}` differs from the transferred portable program",
                dependency.canonical_ref
            );
        }
        dependency.resolution = target_portable;
        dependency.validate()?;
    }
    Ok(())
}

/// Content retains its own source authority on the receiving site. Installed
/// dependencies must not gain a project overlay, while project content must
/// resolve under the outer admission's exact definition generation (not the
/// worker's mutable execution view). The portable comparison still follows.
fn transferred_content_resolution_request(
    item_ref: ryeos_engine::canonical_ref::CanonicalRef,
    source_space: ryeos_engine::contracts::ItemSpace,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    subject: &SubjectResolutionAuthority,
) -> Result<ryeos_engine::engine::EffectiveItemRequest> {
    let (project_root, subject_resolution_authority) = match source_space {
        ryeos_engine::contracts::ItemSpace::Bundle => {
            (None, SubjectResolutionAuthority::Projectless)
        }
        ryeos_engine::contracts::ItemSpace::Project => {
            if subject.operational_generation().is_none() {
                bail!("transferred project content requires exact generation authority");
            }
            let root = roots.authoritative_project_root()?;
            subject.validate_for_materialized_root(root)?;
            (root, subject.clone())
        }
        ryeos_engine::contracts::ItemSpace::Node => {
            bail!("node content cannot be a portable content dependency");
        }
    };
    Ok(ryeos_engine::engine::EffectiveItemRequest {
        item_ref,
        expected_kind: None,
        project_root: project_root.map(std::path::Path::to_path_buf),
        subject_resolution_authority,
    })
}

fn require_receiving_content_contract(
    retained: &ryeos_engine::kind_registry::KindExternalContentDecl,
    receiving: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
) -> Result<()> {
    let receiving = receiving.ok_or_else(|| {
        anyhow!("receiving kind has no content contract for the transferred program")
    })?;
    let large_ceiling = |contract: &ryeos_engine::kind_registry::KindExternalContentDecl| {
        contract.large_content.as_ref().map(|grant| {
            grant
                .max_total_bytes
                .unwrap_or(ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES)
        })
    };
    if retained.realization_derived != receiving.realization_derived
        || retained.max_declarations > receiving.max_declarations
        || retained
            .allowed_roots
            .iter()
            .any(|root| !receiving.allowed_roots.contains(root))
        || retained
            .allowed_mount_roots
            .iter()
            .any(|root| !receiving.allowed_mount_roots.contains(root))
        || large_ceiling(retained) > large_ceiling(receiving)
    {
        bail!(
            "receiving kind does not admit the transferred content contract; select a compatible target"
        );
    }
    Ok(())
}

fn clear_node_local_admission_projections(
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
) {
    resolution
        .composed
        .derived
        .remove(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY);
    resolution
        .composed
        .derived
        .remove(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY);
}

fn portable_subject_value(
    subject: &super::launch_preparation::PreparedExecutionDependencySubject,
) -> Result<Value> {
    let mut value = serde_json::to_value(subject)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("prepared dependency subject must be an object"))?;
    object.remove("source_path");
    object.remove("materialized_project_root");
    Ok(value)
}

/// Admit fresh session dependencies or verify recovered capsule references.
/// No mutable item/config lookup occurs on recovery.
pub(crate) fn admit_or_verify_prepared_sessions(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    prepared: &mut PreparedRuntimeLaunch,
    subject_resolution_authority: &SubjectResolutionAuthority,
    recovered: bool,
    handler_context: Option<&ryeos_app::handler_context::HandlerContext>,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
) -> Result<AdmittedSessionPublications> {
    prepare_product_selections(
        state,
        engine,
        prepared,
        subject_resolution_authority,
        recovered,
        handler_context,
        roots,
    )?;
    validate_prepared_content_targets(state, prepared)?;
    let mut expected_names = BTreeSet::new();
    let (content_by_target, search_by_target, realizations_by_dependency, mut publications) =
        admit_or_verify_content_dependencies(
            state,
            prepared,
            subject_resolution_authority,
            recovered,
        )?;
    let environment_by_target = resolve_target_environments(
        state,
        &prepared.environment_contributions,
        &realizations_by_dependency,
    )?;
    let mut evidence_by_target: BTreeMap<String, Vec<PreparedEvidenceAttachment>> = BTreeMap::new();
    for binding in &prepared.evidence_attachments {
        binding.validate()?;
        evidence_by_target
            .entry(binding.target.clone())
            .or_default()
            .push(binding.clone());
    }
    let (dependencies, admitted_sessions) = (
        &mut prepared.execution_dependencies,
        &mut prepared.admitted_sessions,
    );
    let empty_environment = BTreeMap::new();
    for (name, dependency) in dependencies {
        dependency
            .validate()
            .with_context(|| format!("validate persistent-session dependency `{name}`"))?;
        let target_environment = environment_by_target
            .get(name)
            .unwrap_or(&empty_environment);
        let target_evidence = evidence_by_target
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        // The precheck proved that repeated contracts for this target agree.
        let content_target_contract = prepared
            .content_dependencies
            .values()
            .find_map(|content| content.target_content_contracts.get(name));
        if recovered {
            if (!target_environment.is_empty() || !target_evidence.is_empty())
                && !admitted_sessions.contains_key(name)
            {
                bail!("worker-input target `{name}` has no admitted persistent-session capsule");
            }
            let Some(hash) = admitted_sessions.get(name) else {
                continue;
            };
            expected_names.insert(name.clone());
            verify_session_capsule(
                state,
                engine,
                dependency,
                hash,
                search_by_target.get(name).map(Vec::as_slice).unwrap_or(&[]),
                target_environment,
                target_evidence,
                content_target_contract,
            )
            .with_context(|| format!("verify recovered session dependency `{name}`"))?;
        } else {
            let session = session_contract(engine, dependency)?;
            if (!target_environment.is_empty() || !target_evidence.is_empty()) && session.is_none()
            {
                bail!("worker-input target `{name}` is not a persistent-session dependency");
            }
            let Some((declaration, protocol)) = session else {
                continue;
            };
            expected_names.insert(name.clone());
            if admitted_sessions.contains_key(name) {
                bail!("fresh session dependency `{name}` already carries a capsule hash");
            }
            let (hash, admitted_publications) = admit_session_capsule(
                state,
                engine,
                dependency,
                &declaration,
                &protocol,
                content_by_target.get(name),
                search_by_target.get(name).map(Vec::as_slice).unwrap_or(&[]),
                target_environment,
                target_evidence,
                content_target_contract,
            )
            .inspect_err(|error| {
                tracing::warn!(
                    dependency = %name,
                    error = %error,
                    "persistent-session dependency admission failed"
                );
            })
            .with_context(|| format!("admit persistent-session dependency `{name}`"))?;
            admitted_sessions.insert(name.clone(), hash);
            publications.extend(admitted_publications);
        }
    }
    let actual_names = admitted_sessions.keys().cloned().collect::<BTreeSet<_>>();
    if actual_names != expected_names {
        bail!(
            "admitted persistent-session names contradict execution dependencies: expected={expected_names:?}, actual={actual_names:?}"
        );
    }
    Ok(AdmittedSessionPublications { publications })
}

/// Preserve the operator-correctable external-content absence across the
/// otherwise opaque admission boundary. All other failures retain their
/// existing internal/integrity classification.
pub(crate) fn classify_prepared_session_admission_error(
    error: &anyhow::Error,
) -> Option<crate::dispatch_error::DispatchError> {
    let unavailable = error
        .downcast_ref::<ryeos_app::external_content_admission::ExternalContentBindingUnavailable>(
    )?;
    Some(
        crate::dispatch_error::DispatchError::LaunchResourceNotFound {
            code: "external_content_binding_unavailable".to_owned(),
            message: format!(
                "no active operator binding for manifest {} and consumer {}",
                unavailable.manifest_hash, unavailable.consumer_ref
            ),
            binding: Some(unavailable.consumer_ref.clone()),
        },
    )
}

/// Project the preparer-selected dependencies through current read-side
/// source, content-binding, target, and node session-policy authority. The
/// projection deliberately excludes prepared runtime data, secrets, host
/// paths, capsules, and leases; live launch repeats authoritative admission.
pub(crate) fn preview_prepared_dependencies(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    prepared: &PreparedRuntimeLaunch,
    subject_resolution_authority: &SubjectResolutionAuthority,
    handler_context: Option<&ryeos_app::handler_context::HandlerContext>,
    resolution_roots: &ryeos_engine::item_resolution::ResolutionRoots,
) -> Result<PreparedDependencyValidationPreview> {
    let mut selected_prepared = prepared.clone();
    prepare_product_selections(
        state,
        engine,
        &mut selected_prepared,
        subject_resolution_authority,
        false,
        handler_context,
        resolution_roots,
    )?;
    let prepared = &selected_prepared;
    validate_prepared_content_targets(state, prepared)?;
    // Execution dependencies are admitted bundle programs. This projectless
    // lookup must not be copied into the content-dependency pass below: a
    // project-bound contributor keeps the outer root's definition generation.
    let roots = engine.resolution_roots(None);
    let mut execution_dependencies = BTreeMap::new();
    let mut content_dependencies = BTreeMap::new();
    let mut environment_contributions = BTreeMap::new();
    let mut entries_by_target: BTreeMap<
        String,
        Vec<ryeos_engine::external_realization::RealizedExternalContent>,
    > = BTreeMap::new();
    let mut search_by_target: TargetExecutableSearch = BTreeMap::new();
    let mut realizations_by_dependency = BTreeMap::new();
    let mut admission_ready = true;

    for (name, dependency) in &prepared.execution_dependencies {
        dependency
            .validate()
            .with_context(|| format!("validate prepared execution dependency `{name}`"))?;
        let verified = dependency.captured_verified_subject()?;
        let kind = verified.resolved.kind.as_str();
        if let Some(declarations) = ryeos_engine::external_content::declarations_from_composed(
            &dependency.resolution.composed.composed,
            engine
                .kinds
                .get(kind)
                .and_then(|kind| kind.external_content_contract()),
            ryeos_engine::external_content::declaring_authority(&dependency.resolution)?,
        )? {
            super::external_content::require_supported_mount_roots(
                declarations.iter().map(|entry| entry.mount_root),
                state.isolation.is_enforced(),
            )?;
        }
        let source = ryeos_app::source_closure_admission::preview_source_closure(
            state,
            engine,
            kind,
            &dependency.resolution,
            &roots,
            None,
            None,
        )?;
        let external_content =
            ryeos_app::external_content_admission::preview_external_content_pins(
                state,
                engine,
                kind,
                &dependency.resolution,
                &roots,
                &ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
            )?;
        let session = if let Some((declaration, protocol)) = session_contract(engine, dependency)? {
            let lifecycle = lifecycle_contract(&declaration)?;
            let wire = wire_contract(&protocol)?;
            let eligibility = state
                .persistent_sessions
                .validate_contract_eligibility(&lifecycle, &wire)?;
            Some(PersistentSessionEligibilityPreview {
                ready_for_admission: eligibility.ready_for_admission(),
                status: eligibility.code().to_owned(),
            })
        } else {
            None
        };
        let ready = source
            .as_ref()
            .is_none_or(|preview| preview.ready_for_admission)
            && external_content
                .as_ref()
                .is_none_or(|preview| preview.ready_for_admission)
            && session
                .as_ref()
                .is_none_or(|preview| preview.ready_for_admission);
        admission_ready &= ready;
        execution_dependencies.insert(
            name.clone(),
            ExecutionDependencyValidationPreview {
                canonical_ref: dependency.canonical_ref.clone(),
                resolution: dependency.resolution.as_launched_digest(),
                source,
                external_content,
                session,
                admission_ready: ready,
            },
        );
    }

    for (name, dependency) in &prepared.content_dependencies {
        dependency
            .validate()
            .with_context(|| format!("validate prepared content dependency `{name}`"))?;
        let resolution = dependency.resolution.restore();
        let preview = ryeos_app::external_content_admission::preview_portable_content_dependency_with_realizations(
                state,
                &resolution,
                &dependency.external_content_policy,
                subject_resolution_authority,
            )?;
        let ready = preview.validation.ready_for_admission;
        if let Some(realized) = preview.realizations.as_ref() {
            validate_dependency_search(state, name, dependency, realized)?;
            for target in &dependency.targets {
                entries_by_target
                    .entry(target.clone())
                    .or_default()
                    .extend(realized.iter().cloned());
                search_by_target.entry(target.clone()).or_default().extend(
                    dependency
                        .executable_search
                        .iter()
                        .map(|entry| ExecutableSearchPathEntry {
                            realization_id: entry.realization_id.clone(),
                            relative_directory: entry.relative_directory.clone(),
                        }),
                );
            }
            realizations_by_dependency.insert(name.clone(), realized.clone());
        }
        admission_ready &= ready;
        content_dependencies.insert(
            name.clone(),
            ContentDependencyValidationPreview {
                binding: dependency.binding.clone(),
                canonical_ref: dependency.canonical_ref.clone(),
                resolution: resolution.as_launched_digest(),
                targets: dependency.targets.clone(),
                executable_search: dependency.executable_search.clone(),
                external_content_policy: dependency.external_content_policy.clone(),
                external_content: preview.validation,
                admission_ready: ready,
            },
        );
    }
    validate_content_target_aggregation(entries_by_target, search_by_target)?;
    let environment_by_target = resolve_target_environments(
        state,
        &prepared.environment_contributions,
        &realizations_by_dependency,
    )?;
    for (target, environment) in &environment_by_target {
        if !environment.is_empty()
            && execution_dependencies
                .get(target)
                .is_none_or(|dependency| dependency.session.is_none())
        {
            bail!(
                "environment contribution target `{target}` is not a persistent-session dependency"
            );
        }
    }
    for (name, contribution) in &prepared.environment_contributions {
        environment_contributions.insert(
            name.clone(),
            EnvironmentContributionValidationPreview {
                targets: contribution.targets.clone(),
                variables: contribution.variables.clone(),
            },
        );
    }

    Ok(PreparedDependencyValidationPreview {
        binding_records: prepared.binding_records.clone(),
        execution_dependencies,
        content_dependencies,
        environment_contributions,
        admission_ready,
    })
}

/// Content contributions do not widen the receiving execution kind. Check
/// captured source/runtime ceilings and aggregate target ceilings independently;
/// no rebinding to the target, live item lookup, or evidence-policy substitution.
fn validate_prepared_content_targets(
    state: &AppState,
    prepared: &PreparedRuntimeLaunch,
) -> Result<()> {
    use ryeos_engine::external_content::{
        declaring_authority, effective_external_content_declarations,
    };
    let mut by_target: BTreeMap<
        String,
        (
            ryeos_engine::kind_registry::KindExternalContentDecl,
            Vec<ryeos_engine::external_content::ExternalContentDeclaration>,
        ),
    > = BTreeMap::new();
    for (name, dependency) in &prepared.content_dependencies {
        dependency.validate()?;
        let resolution = dependency.resolution.restore();
        let source_contract = dependency.external_content_policy.declaration_contract();
        let declarations = effective_external_content_declarations(
            &resolution,
            Some(&source_contract),
            declaring_authority(&resolution)?,
        )?
        .ok_or_else(|| anyhow!("content dependency `{name}` has no declarations"))?;
        if declarations.is_empty()
            || declarations.iter().any(|entry| {
                entry.locator.is_some()
                    || entry.mode != ryeos_state::objects::ExternalContentMode::Pinned
            })
        {
            bail!("content dependency `{name}` is not locator-free pinned content");
        }
        super::external_content::require_supported_mount_roots(
            declarations.iter().map(|entry| entry.mount_root),
            state.isolation.is_enforced(),
        )?;
        ryeos_app::external_content_admission::validate_retained_declaration_totals(
            state,
            Some(&source_contract),
            &declarations,
        )?;
        for (target, contract) in &dependency.target_content_contracts {
            if !prepared.execution_dependencies.contains_key(target) {
                bail!("content dependency `{name}` target `{target}` is absent");
            }
            let (incumbent, aggregate) = by_target
                .entry(target.clone())
                .or_insert_with(|| (contract.clone(), Vec::new()));
            if serde_json::to_value(&*incumbent)? != serde_json::to_value(contract)? {
                bail!("content contributions disagree on target `{target}` contract");
            }
            aggregate.extend(declarations.iter().cloned());
        }
    }
    for (target, (contract, contributed)) in by_target {
        let resolution = &prepared.execution_dependencies[&target].resolution;
        let combined = combined_target_content_declarations(
            &resolution.composed.composed,
            declaring_authority(resolution)?,
            &contract,
            &contributed,
        )
        .with_context(|| format!("content target `{target}` exceeds its retained kind contract"))?;
        ryeos_app::external_content_admission::validate_retained_declaration_totals(
            state,
            Some(&contract),
            &combined,
        )
        .with_context(|| format!("content target `{target}` exceeds its retained storage grant"))?;
    }
    Ok(())
}

fn combined_target_content_declarations(
    composed: &Value,
    declarer: ryeos_engine::external_content::DeclaringAuthority<'_>,
    contract: &ryeos_engine::kind_registry::KindExternalContentDecl,
    contributed: &[ryeos_engine::external_content::ExternalContentDeclaration],
) -> Result<Vec<ryeos_engine::external_content::ExternalContentDeclaration>> {
    use ryeos_engine::external_content::declarations_from_composed;
    let mut declarations =
        declarations_from_composed(composed, Some(contract), declarer)?.unwrap_or_default();
    declarations.extend(contributed.iter().cloned());
    Ok(declarations_from_composed(
        &json!({"external_content": declarations}),
        Some(contract),
        declarer,
    )?
    .expect("combined declaration list is present"))
}

/// Validate actual retained manifests after capture, not locator declarations.
/// The conversion is only an input to the existing storage-budget validator;
/// it does not replace authored declarations or the admitted realization set.
fn validate_captured_target_content(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    contract: &ryeos_engine::kind_registry::KindExternalContentDecl,
    evidence: &[PreparedEvidenceAttachment],
) -> Result<()> {
    let realized = realization_set(resolution)?;
    let declarations = captured_target_content_declarations(&realized, evidence)?;
    ryeos_app::external_content_admission::validate_retained_declaration_totals(
        state,
        Some(contract),
        &declarations,
    )
}

fn captured_target_content_declarations(
    realized: &ryeos_engine::external_realization::RealizedExternalContentSet,
    evidence: &[PreparedEvidenceAttachment],
) -> Result<Vec<ryeos_engine::external_content::ExternalContentDeclaration>> {
    let evidence_ids = evidence_realizations(evidence)?
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<BTreeSet<_>>();
    Ok(realized
        .iter()
        .filter(|entry| !evidence_ids.contains(&entry.id))
        .map(
            |entry| ryeos_engine::external_content::ExternalContentDeclaration {
                id: entry.id.clone(),
                kind: entry.kind,
                mode: ryeos_state::objects::ExternalContentMode::Pinned,
                locator: None,
                digest: Some(entry.manifest_hash.clone()),
                exclude: Vec::new(),
                metadata_hint: None,
                mount_root: entry.mount_root,
                mount: entry.mount.clone(),
            },
        )
        .collect::<Vec<_>>())
}

type TargetContentSets =
    BTreeMap<String, ryeos_engine::external_realization::RealizedExternalContentSet>;
type TargetExecutableSearch = BTreeMap<String, Vec<ExecutableSearchPathEntry>>;
type TargetSessionProcessEnvironment =
    BTreeMap<String, BTreeMap<String, ryeos_state::objects::SessionProcessEnvironmentValue>>;
type RealizationsByDependency =
    BTreeMap<String, ryeos_engine::external_realization::RealizedExternalContentSet>;

/// Resolve invocation-time witnesses before any manifest preview, target
/// aggregation, or admission. A preparer only selects signed dependency slots;
/// it cannot manufacture the configured operator's product authority.
#[allow(clippy::too_many_arguments)]
fn prepare_product_selections(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    prepared: &mut PreparedRuntimeLaunch,
    subject: &SubjectResolutionAuthority,
    recovered: bool,
    handler_context: Option<&ryeos_app::handler_context::HandlerContext>,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
) -> Result<()> {
    for (name, dependency) in &mut prepared.content_dependencies {
        let mut resolution = dependency.resolution.restore();
        let retained =
            ryeos_engine::external_content::resolved_external_product_selections(&resolution)?;
        if dependency.product_selections.is_empty() {
            if retained.is_some() {
                bail!(
                    "content dependency `{name}` has product authority without a sealed selector"
                );
            }
            continue;
        }
        let context = handler_context.ok_or_else(|| {
            anyhow!("product selection requires retained local operator authority")
        })?;
        if recovered {
            ryeos_app::operator_external_content::product_composition::verify_recovered_selections(
                state,
                &resolution,
                subject,
                &context.fingerprint,
                &dependency.product_selections,
            )?;
        } else {
            // Content-only dependencies use their already resolved data as D0.
            // Executable roots have a separate source/hook/validator admission
            // phase and must not be smuggled through this data dependency lane.
            let kind =
                ryeos_engine::canonical_ref::CanonicalRef::parse(&resolution.root.resolved_ref)?
                    .kind;
            let schema = engine.kinds.get(&kind).ok_or_else(|| {
                anyhow!("product content dependency `{name}` has no signed kind contract")
            })?;
            if schema.execution.is_some() {
                bail!(
                    "product content dependency `{name}` is executable; select its products through root execution admission"
                );
            }
            ryeos_app::operator_external_content::product_composition::select_products(
                state,
                context,
                engine,
                roots,
                subject,
                &mut resolution,
                &dependency.product_selections,
            )?;
            dependency.resolution =
                ryeos_engine::resolution::RetainedResolutionOutput::capture(&resolution);
        }
    }
    Ok(())
}

fn admit_or_verify_content_dependencies(
    state: &AppState,
    prepared: &mut PreparedRuntimeLaunch,
    subject_resolution_authority: &SubjectResolutionAuthority,
    recovered: bool,
) -> Result<(
    TargetContentSets,
    TargetExecutableSearch,
    RealizationsByDependency,
    Vec<ryeos_state::PendingCasPublication>,
)> {
    let mut entries_by_target: BTreeMap<
        String,
        Vec<ryeos_engine::external_realization::RealizedExternalContent>,
    > = BTreeMap::new();
    let mut search_by_target: TargetExecutableSearch = BTreeMap::new();
    let mut realizations_by_dependency = BTreeMap::new();
    let mut publications = Vec::new();
    for (name, dependency) in &mut prepared.content_dependencies {
        dependency
            .validate()
            .with_context(|| format!("validate content dependency `{name}`"))?;
        let mut resolution = dependency.resolution.restore();
        let realized = if recovered {
            ryeos_app::external_content_admission::recover_external_realizations(
                state,
                &resolution,
            )?
            .ok_or_else(|| anyhow!("recovered content dependency `{name}` has no realization"))?;
            realization_set(&resolution)?
        } else {
            let mut publication = None;
            ryeos_app::external_content_admission::admit_portable_content_dependency_in_publication(
                state,
                &mut resolution,
                &dependency.external_content_policy,
                subject_resolution_authority,
                None,
                &mut publication,
            )?;
            dependency.resolution =
                ryeos_engine::resolution::RetainedResolutionOutput::capture(&resolution);
            if let Some(publication) = publication {
                publications.push(publication);
            }
            realization_set(&resolution)?
        };
        validate_dependency_search(state, name, dependency, &realized)?;
        for target in &dependency.targets {
            entries_by_target
                .entry(target.clone())
                .or_default()
                .extend(realized.iter().cloned());
            search_by_target.entry(target.clone()).or_default().extend(
                dependency
                    .executable_search
                    .iter()
                    .map(|entry| ExecutableSearchPathEntry {
                        realization_id: entry.realization_id.clone(),
                        relative_directory: entry.relative_directory.clone(),
                    }),
            );
        }
        realizations_by_dependency.insert(name.clone(), realized);
    }
    for binding in &prepared.evidence_attachments {
        entries_by_target
            .entry(binding.target.clone())
            .or_default()
            .extend(
                evidence_realizations(std::slice::from_ref(binding))?
                    .iter()
                    .cloned(),
            );
    }
    let (content_by_target, search_by_target) =
        validate_content_target_aggregation(entries_by_target, search_by_target)?;
    Ok((
        content_by_target,
        search_by_target,
        realizations_by_dependency,
        publications,
    ))
}

fn validate_content_target_aggregation(
    entries_by_target: BTreeMap<
        String,
        Vec<ryeos_engine::external_realization::RealizedExternalContent>,
    >,
    search_by_target: TargetExecutableSearch,
) -> Result<(TargetContentSets, TargetExecutableSearch)> {
    let content_by_target = entries_by_target
        .into_iter()
        .map(|(target, entries)| {
            Ok((
                target,
                ryeos_engine::external_realization::RealizedExternalContentSet::new(entries)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    for search in search_by_target.values() {
        if search.len() > ryeos_state::objects::MAX_EXECUTABLE_SEARCH_PATH_ENTRIES {
            bail!("combined executable search exceeds the session capsule bound");
        }
        let mut seen = BTreeSet::new();
        if search.iter().any(|entry| {
            !seen.insert((
                entry.realization_id.as_str(),
                entry.relative_directory.as_str(),
            ))
        }) {
            bail!("combined executable search contains duplicate entries");
        }
    }
    Ok((content_by_target, search_by_target))
}

fn merge_target_environment(
    target: &mut BTreeMap<String, ryeos_state::objects::SessionProcessEnvironmentValue>,
    additional: &BTreeMap<String, ryeos_state::objects::SessionProcessEnvironmentValue>,
) -> Result<()> {
    for (name, value) in additional {
        if target.insert(name.clone(), value.clone()).is_some() {
            bail!("combined session process environment contains duplicate variable `{name}`");
        }
    }
    Ok(())
}

fn realization_set(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> Result<ryeos_engine::external_realization::RealizedExternalContentSet> {
    let value = resolution
        .composed
        .derived
        .get(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY)
        .ok_or_else(|| anyhow!("content dependency has no retained external realization"))?;
    Ok(ryeos_engine::external_realization::RealizedExternalContentSet::from_value(value)?)
}

fn validate_dependency_search(
    state: &AppState,
    name: &str,
    dependency: &PreparedContentDependency,
    realized: &ryeos_engine::external_realization::RealizedExternalContentSet,
) -> Result<()> {
    for search in &dependency.executable_search {
        let entry = realized
            .iter()
            .find(|entry| entry.id == search.realization_id)
            .ok_or_else(|| {
                anyhow!(
                    "content dependency `{name}` executable search names absent realization `{}`",
                    search.realization_id
                )
            })?;
        if entry.kind != ryeos_state::objects::ExternalContentKind::Tree {
            bail!(
                "content dependency `{name}` executable search names non-tree realization `{}`",
                search.realization_id
            );
        }
        let cas_read = state.acquire_cas_read()?;
        let object = cas_read
            .cas()
            .get_object(&entry.manifest_hash)?
            .ok_or_else(|| {
                anyhow!(
                    "content dependency `{name}` executable search realization `{}` has no retained manifest",
                    search.realization_id
                )
            })?;
        let manifest_kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(
                    "content dependency `{name}` executable search realization `{}` manifest has no kind",
                    search.realization_id
                )
            })?;
        let directory_exists = match manifest_kind {
            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND => {
                let manifest =
                    ryeos_state::objects::ExternalContentManifestObject::from_value(&object)?;
                manifest_materializes_directory(
                    &search.relative_directory,
                    manifest
                        .entries
                        .iter()
                        .map(|entry| (entry.path.as_str(), entry.kind)),
                )
            }
            ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
                let manifest =
                    ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&object)?;
                manifest_materializes_directory(
                    &search.relative_directory,
                    manifest
                        .entries
                        .iter()
                        .map(|entry| (entry.path.as_str(), entry.kind)),
                )
            }
            other => bail!(
                "content dependency `{name}` executable search realization `{}` names unsupported manifest kind `{other}`",
                search.realization_id
            ),
        };
        if !directory_exists {
            bail!(
                "content dependency `{name}` executable search directory `{}` is absent from realization `{}`",
                search.relative_directory,
                search.realization_id
            );
        }
    }
    Ok(())
}

fn resolve_target_environments(
    state: &AppState,
    contributions: &BTreeMap<String, super::launch_preparation::PreparedEnvironmentContribution>,
    realizations_by_dependency: &RealizationsByDependency,
) -> Result<TargetSessionProcessEnvironment> {
    let mut by_target = TargetSessionProcessEnvironment::new();
    for (name, contribution) in contributions {
        contribution
            .validate()
            .with_context(|| format!("validate environment contribution `{name}`"))?;
        let resolved = validate_environment_contribution(
            state,
            name,
            contribution,
            realizations_by_dependency,
        )?;
        for target in &contribution.targets {
            merge_target_environment(by_target.entry(target.clone()).or_default(), &resolved)?;
        }
    }
    for environment in by_target.values() {
        ryeos_state::objects::validate_session_process_environment(environment)?;
    }
    Ok(by_target)
}

fn validate_environment_contribution(
    state: &AppState,
    name: &str,
    contribution: &super::launch_preparation::PreparedEnvironmentContribution,
    realizations_by_dependency: &RealizationsByDependency,
) -> Result<BTreeMap<String, ryeos_state::objects::SessionProcessEnvironmentValue>> {
    use ryeos_handler_protocol::LaunchEnvironmentValueWire as Wire;
    use ryeos_state::objects::SessionProcessEnvironmentValue as Retained;

    let mut retained = BTreeMap::new();
    for (variable, value) in &contribution.variables {
        let value = match value {
            Wire::Literal { value } => Retained::Literal {
                value: value.clone(),
            },
            Wire::RuntimeViewDirectory { relative_path } => Retained::RuntimeViewDirectory {
                relative_path: relative_path.clone(),
            },
            Wire::ContentPath {
                content_dependency,
                realization_id,
                relative_path,
                path_kind,
            } => {
                let realized = realizations_by_dependency
                    .get(content_dependency)
                    .ok_or_else(|| {
                        anyhow!(
                            "environment contribution `{name}` has no realized content dependency `{content_dependency}`"
                        )
                    })?;
                let entry = realized
                    .iter()
                    .find(|entry| entry.id == *realization_id)
                    .ok_or_else(|| {
                        anyhow!(
                            "environment contribution `{name}` content dependency `{content_dependency}` names absent realization `{realization_id}`"
                        )
                    })?;
                if entry.kind != ryeos_state::objects::ExternalContentKind::Tree
                    || entry.mode != ryeos_state::objects::ExternalContentMode::Pinned
                {
                    bail!(
                        "environment contribution `{name}` content dependency `{content_dependency}` names a realization that is not a pinned tree: `{realization_id}`"
                    );
                }
                validate_realization_path_exists(
                    state,
                    content_dependency,
                    entry,
                    relative_path,
                    *path_kind,
                    "environment",
                )?;
                Retained::RealizationPath {
                    realization_id: realization_id.clone(),
                    relative_path: relative_path.clone(),
                    path_kind: match path_kind {
                        ryeos_handler_protocol::LaunchEnvironmentPathKindWire::File => {
                            ryeos_state::objects::SessionProcessEnvironmentPathKind::File
                        }
                        ryeos_handler_protocol::LaunchEnvironmentPathKindWire::Directory => {
                            ryeos_state::objects::SessionProcessEnvironmentPathKind::Directory
                        }
                    },
                }
            }
        };
        retained.insert(variable.clone(), value);
    }
    ryeos_state::objects::validate_session_process_environment(&retained)?;
    Ok(retained)
}

fn validate_realization_path_exists(
    state: &AppState,
    dependency_name: &str,
    realization: &ryeos_engine::external_realization::RealizedExternalContent,
    relative_path: &str,
    expected_kind: ryeos_handler_protocol::LaunchEnvironmentPathKindWire,
    use_kind: &str,
) -> Result<()> {
    if relative_path == "." {
        if expected_kind != ryeos_handler_protocol::LaunchEnvironmentPathKindWire::Directory {
            bail!("a realization root environment path must be a directory");
        }
        return Ok(());
    }
    let cas_read = state.acquire_cas_read()?;
    let object = cas_read
        .cas()
        .get_object(&realization.manifest_hash)?
        .ok_or_else(|| {
            anyhow!(
                "content dependency `{dependency_name}` {use_kind} realization `{}` has no retained manifest",
                realization.id
            )
        })?;
    let observed_kind = match object.get("kind").and_then(Value::as_str) {
        Some(ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND) => {
            let manifest =
                ryeos_state::objects::ExternalContentManifestObject::from_value(&object)?;
            manifest
                .entries
                .iter()
                .find(|entry| entry.path == relative_path)
                .map(|entry| entry.kind)
                .or_else(|| {
                    manifest
                        .entries
                        .iter()
                        .any(|entry| entry.path.starts_with(&format!("{relative_path}/")))
                        .then_some(ryeos_state::objects::ExternalContentManifestEntryKind::Dir)
                })
        }
        Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
            let manifest =
                ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&object)?;
            manifest
                .entries
                .iter()
                .find(|entry| entry.path == relative_path)
                .map(|entry| entry.kind)
                .or_else(|| {
                    manifest
                        .entries
                        .iter()
                        .any(|entry| entry.path.starts_with(&format!("{relative_path}/")))
                        .then_some(ryeos_state::objects::ExternalContentManifestEntryKind::Dir)
                })
        }
        Some(other) => bail!(
            "content dependency `{dependency_name}` {use_kind} realization `{}` names unsupported manifest kind `{other}`",
            realization.id
        ),
        None => bail!(
            "content dependency `{dependency_name}` {use_kind} realization `{}` manifest has no kind",
            realization.id
        ),
    };
    let Some(observed_kind) = observed_kind else {
        bail!(
            "content dependency `{dependency_name}` {use_kind} path `{relative_path}` is absent from realization `{}`",
            realization.id
        );
    };
    let kind_matches = matches!(
        (expected_kind, observed_kind),
        (
            ryeos_handler_protocol::LaunchEnvironmentPathKindWire::File,
            ryeos_state::objects::ExternalContentManifestEntryKind::File
        ) | (
            ryeos_handler_protocol::LaunchEnvironmentPathKindWire::Directory,
            ryeos_state::objects::ExternalContentManifestEntryKind::Dir
        )
    );
    if !kind_matches {
        bail!(
            "content dependency `{dependency_name}` {use_kind} path `{relative_path}` has the wrong entry kind in realization `{}`",
            realization.id
        );
    }
    Ok(())
}

fn manifest_materializes_directory<'a>(
    relative_directory: &str,
    entries: impl IntoIterator<
        Item = (
            &'a str,
            ryeos_state::objects::ExternalContentManifestEntryKind,
        ),
    >,
) -> bool {
    if relative_directory == "." {
        return true;
    }
    let child_prefix = format!("{relative_directory}/");
    for (path, kind) in entries {
        if path == relative_directory {
            return kind == ryeos_state::objects::ExternalContentManifestEntryKind::Dir;
        }
        // Materialization creates canonical parents before publishing each
        // entry, so a retained descendant is also exact evidence that the
        // requested directory will exist.
        if path.starts_with(&child_prefix) {
            return true;
        }
    }
    false
}

pub(crate) fn session_contract(
    engine: &ryeos_engine::engine::Engine,
    dependency: &PreparedExecutionDependency,
) -> Result<Option<(PersistentSessionDecl, VerifiedProtocol)>> {
    let kind = &dependency.captured_verified_subject()?.resolved.kind;
    let execution = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .ok_or_else(|| anyhow!("execution dependency kind `{kind}` is not executable"))?;
    let Some(mut declaration) = execution.persistent_session.clone() else {
        return Ok(None);
    };
    apply_resource_overrides(&mut declaration, &dependency.resolution.composed.composed)?;
    let TerminatorDecl::Subprocess {
        protocol: protocol_selection,
    } = execution
        .terminator
        .as_ref()
        .ok_or_else(|| anyhow!("persistent-session kind `{kind}` has no terminator"))?
    else {
        bail!("persistent-session kind `{kind}` is not subprocess-terminated");
    };
    let protocol_ref = protocol_selection
        .resolve(&dependency.resolution.composed.composed)
        .map_err(|reason| anyhow!("persistent-session kind `{kind}`: {reason}"))?;
    let protocol = engine
        .protocols
        .get(&protocol_ref)
        .cloned()
        .ok_or_else(|| anyhow!("persistent-session protocol `{protocol_ref}` is not installed"))?;
    validate_persistent_session_protocol(&protocol.descriptor)
        .map_err(|error| anyhow!("persistent-session protocol `{protocol_ref}`: {error}"))?;
    validate_session_target(
        &dependency.resolution.composed.composed,
        &declaration.target_path,
    )?;
    Ok(Some((declaration, protocol)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistentSessionResourceOverrides {
    real_uid_process_limit: Option<u64>,
}

fn apply_resource_overrides(
    declaration: &mut PersistentSessionDecl,
    composed: &Value,
) -> Result<()> {
    let Some(path) = declaration.resource_overrides_path.as_ref() else {
        return Ok(());
    };
    let mut value = composed;
    for segment in path {
        let Some(next) = value.get(segment) else {
            return Ok(());
        };
        value = next;
    }
    let overrides: PersistentSessionResourceOverrides = serde_json::from_value(value.clone())
        .context("decode signed persistent-session resource overrides")?;
    if let Some(limit) = overrides.real_uid_process_limit {
        if limit == 0 || limit > declaration.max_real_uid_process_limit {
            bail!(
                "persistent-session real-UID process limit {limit} exceeds its signed kind ceiling {}",
                declaration.max_real_uid_process_limit
            );
        }
        declaration.real_uid_process_limit = limit;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistentSessionTarget {
    os: String,
    arch: String,
}

fn validate_session_target(composed: &Value, path: &[String]) -> Result<()> {
    let mut value = composed;
    for segment in path {
        value = value.get(segment).ok_or_else(|| {
            anyhow!(
                "persistent-session subject has no target constraint at `{}`",
                path.join(".")
            )
        })?;
    }
    let target: PersistentSessionTarget = serde_json::from_value(value.clone())
        .context("decode persistent-session target constraint")?;
    if target.os != std::env::consts::OS || target.arch != std::env::consts::ARCH {
        bail!(
            "persistent-session target {}-{} does not admit this {}-{} node",
            target.arch,
            target.os,
            std::env::consts::ARCH,
            std::env::consts::OS
        );
    }
    Ok(())
}

fn admit_session_capsule(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    dependency: &mut PreparedExecutionDependency,
    declaration: &PersistentSessionDecl,
    protocol: &VerifiedProtocol,
    inherited_content: Option<&ryeos_engine::external_realization::RealizedExternalContentSet>,
    executable_search: &[ExecutableSearchPathEntry],
    environment: &BTreeMap<String, ryeos_state::objects::SessionProcessEnvironmentValue>,
    evidence_attachments: &[PreparedEvidenceAttachment],
    content_target_contract: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
) -> Result<(String, Vec<ryeos_state::PendingCasPublication>)> {
    let session = validate_persistent_session_protocol(&protocol.descriptor)
        .map_err(|error| anyhow!(error))?;
    validate_session_process_control(state, session)?;
    let roots = engine.resolution_roots(None);
    let mut resolution = dependency.resolution.clone();
    let mut publication = None;
    let captured_source = ryeos_app::source_closure_admission::admit_source_closure_in_publication(
        state,
        engine,
        &dependency.captured_verified_subject()?.resolved.kind,
        &mut resolution,
        &roots,
        None,
        None,
        &mut publication,
        None,
    )?;
    let captured_external =
        ryeos_app::external_content_admission::admit_external_realizations_in_publication(
            state,
            engine,
            &dependency.captured_verified_subject()?.resolved.kind,
            &mut resolution,
            &roots,
            &ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
            inherited_content,
            &mut publication,
        )?;
    if let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY)
    {
        let realized = ryeos_state::objects::ExternalContentRealizationSet::from_value(value)?;
        super::external_content::require_supported_mount_roots(
            realized.iter().map(|entry| entry.mount_root),
            state.isolation.is_enforced(),
        )?;
    }
    if let Some(contract) = content_target_contract {
        validate_captured_target_content(state, &resolution, contract, evidence_attachments)?;
    }
    let validation = engine.effective_validators.validate(
        &dependency.captured_verified_subject()?.resolved.kind,
        &resolution,
    )?;
    let candidate =
        ryeos_engine::effective_program::lock_validated_effective_program(resolution, validation)?;
    let proof = ryeos_engine::effective_program::prove_finalization_authority(
        &candidate,
        engine
            .kinds
            .get(&dependency.captured_verified_subject()?.resolved.kind)
            .and_then(|schema| schema.execution.as_ref())
            .and_then(|execution| execution.external_content.as_ref()),
        &[],
        &roots,
        None,
        captured_external
            .as_ref()
            .map(|captured| captured.finalization_evidence()),
        captured_source
            .as_ref()
            .map(|captured| captured.finalization_evidence()),
    )?;
    let finalized = ryeos_engine::effective_program::finalize_effective_program(candidate, proof)?;
    // The outer runtime capsule must retain the exact augmented dependency the
    // session capsule executes, including its admitted external-realization
    // projection. Keeping only the pre-admission resolution would make outer
    // recovery compare two different programs.
    dependency.resolution = finalized.resolution().clone();
    let retained_product_selections =
        ryeos_engine::external_content::resolved_external_product_selections(
            finalized.resolution(),
        )?;
    let semantic_resolution =
        ryeos_state::external_content::products::composition::project_resolution_product_selections_for_identity(
            &serde_json::to_value(ryeos_engine::resolution::RetainedResolutionOutput::capture(
                finalized.resolution(),
            ))?,
        )?;
    let exact_program = PersistentSessionExactProgram {
        effective_definition_digest: finalized.effective_definition_digest().as_str().to_owned(),
        resolution_output: serde_json::from_value(semantic_resolution)?,
        evidence_attachments: evidence_attachments.to_vec(),
    };
    let exact_program_value = serde_json::to_value(&exact_program)?;
    let exact_program_hash = canonical_hash(&exact_program_value)?;
    let workspace = logical_admission_workspace();
    let lifecycle = lifecycle_contract(declaration)?;
    let wire = wire_contract(protocol)?;
    if !environment.is_empty()
        && !protocol.descriptor.session.as_ref().is_some_and(|session| {
            session
                .runtime_env_allowlist
                .iter()
                .any(|name| name == ryeos_state::objects::SESSION_PROCESS_ENVIRONMENT_ENV)
        })
    {
        bail!("persistent-session protocol does not authorize a session process environment");
    }
    let verified = dependency.captured_verified_subject()?;
    let mut request = direct_request(state, dependency, &verified, String::new())?;
    let session = validate_persistent_session_protocol(&protocol.descriptor)
        .map_err(|error| anyhow!(error))?;
    let mut plan = prepare_captured_item_plan(
        engine,
        &request,
        &verified,
        &finalized,
        &state.isolation,
        None,
        session.workspace_authority.filesystem_ceiling(),
    )?;
    let executor_ref = plan
        .execution_plan()
        .executor_chain
        .get(1)
        .cloned()
        .ok_or_else(|| anyhow!("persistent-session plan has no executor-chain hop"))?;
    request.executor_ref = executor_ref.clone();
    // Freeze the signed session constraints before hashing the direct plan.
    // Testimony and every later boot consume this same narrowed pair, not a
    // presumed isolation mode derived from the fact that this is a session.
    plan.restrict_isolation_authority(
        session.workspace_authority.filesystem_ceiling(),
        session.network_authority.network_ceiling(),
    );
    plan.bind_persistent_session_workspace(&workspace)?;
    let artifact_identity = plan.admitted_artifact_identity(&request, protocol)?;

    let authority = publication
        .as_ref()
        .map(|publication| publication.authority().try_clone())
        .transpose()?
        .unwrap_or(state.state_store.pinned_state_authority()?);
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let structured_session_profile = if wire.wire_protocol == "ryeos.structured-session" {
        let captured = captured_source
            .as_ref()
            .ok_or_else(|| anyhow!("structured-session worker has no admitted source closure"))?;
        let entry = match &captured.binding().logical_binding {
            ryeos_state::objects::SourceLogicalBinding::Worker { entry, .. } => entry,
            _ => bail!("structured-session worker has a non-worker source binding"),
        };
        let mut source_files = BTreeMap::new();
        for file in &captured.manifest().entries {
            let bytes = cas
                .get_blob(&file.blob_hash)?
                .ok_or_else(|| anyhow!("captured structured-session source blob is absent"))?;
            source_files.insert(file.path.clone(), bytes);
        }
        let profile_bytes = source_files.get(entry).ok_or_else(|| {
            anyhow!("structured-session entry is absent from its captured source closure")
        })?;
        let profile =
            ryeos_engine::structured_session_profile::compile(profile_bytes, &source_files)?;
        validate_required_session_environment(&profile, environment)?;
        Some(profile)
    } else {
        None
    };
    let execution_closure = {
        let _permit = state
            .write_barrier
            .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|error| anyhow!("cannot acquire persistent-session write permit: {error}"))?;
        plan.admit_execution_closure(
            &cas,
            &state.isolation,
            protocol,
            &engine.node_trust_store,
            Some(&workspace),
        )?
    };
    authority.ensure_guard(&guard)?;
    let session_authority = PersistentSessionAuthority {
        exact_program_hash: exact_program_hash.clone(),
        lifecycle: lifecycle.clone(),
        wire: wire.clone(),
        artifact_identity: artifact_identity.clone(),
        execution_closure: execution_closure.clone(),
        runtime_ref: plan.runtime_ref()?.to_owned(),
        executor_ref: executor_ref.clone(),
    };
    let realization = super::execution_realization::admit_persistent_session(
        state,
        &session_authority,
        finalized.resolution(),
        finalized.effective_definition_digest().as_str(),
        &protocol.canonical_ref,
        &protocol.raw_content_digest,
        publication.as_mut(),
    )?;
    if publication.is_none() {
        publication = realization.publication;
    }
    let mut publication = publication.ok_or_else(|| {
        anyhow!("persistent-session admission produced no durable CAS publication")
    })?;
    let capsule = AdmittedPersistentSessionCapsule {
        schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
        kind: PERSISTENT_SESSION_CAPSULE_KIND.to_owned(),
        exact_program: exact_program_value,
        exact_program_hash,
        retained_product_selections,
        lifecycle,
        wire,
        artifact_identity,
        execution_closure,
        execution_realization_hash: realization.hash,
        source_binding_hash: captured_source
            .as_ref()
            .map(|captured| captured.binding().digest())
            .transpose()?,
        structured_session_profile,
        executable_search: executable_search.to_vec(),
        process_environment: environment.clone(),
        runtime_ref: session_authority.runtime_ref,
        executor_ref,
    };
    let expected_hash = capsule.content_hash()?;
    let guard = publication.authority().acquire_shared_guard()?;
    publication.authority().ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow!("cannot acquire session-capsule write permit: {error}"))?;
    let cas = publication.authority().cas_store()?;
    let stored =
        publication
            .staged_roots_mut()
            .store_object_admitted(&guard, &cas, &capsule.to_value()?)?;
    if stored != expected_hash {
        bail!(
            "persistent-session capsule hash mismatch: expected {expected_hash}, stored {stored}"
        );
    }
    Ok((stored, vec![publication]))
}

fn verify_session_capsule(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    dependency: &PreparedExecutionDependency,
    capsule_hash: &str,
    executable_search: &[ExecutableSearchPathEntry],
    environment: &BTreeMap<String, ryeos_state::objects::SessionProcessEnvironmentValue>,
    evidence_attachments: &[PreparedEvidenceAttachment],
    content_target_contract: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
) -> Result<AdmittedPersistentSessionCapsule> {
    let capsule =
        load_capsule(state, capsule_hash).context(SessionCapsuleVerificationStage::Load)?;
    validate_session_process_control(
        state,
        &retained_session_protocol(engine, &capsule)
            .context(SessionCapsuleVerificationStage::RetainedProtocol)?,
    )
    .context(SessionCapsuleVerificationStage::ProcessControl)?;
    let exact =
        retained_exact_program(&capsule).context(SessionCapsuleVerificationStage::ExactProgram)?;
    let retained_dependency =
        ryeos_engine::resolution::RetainedResolutionOutput::capture(&dependency.resolution);
    if exact.resolution_output.root_ref() != dependency.canonical_ref
        || canonical_hash(&serde_json::to_value(&exact.resolution_output)?)?
            != canonical_hash(&serde_json::to_value(&retained_dependency)?)?
    {
        return Err(anyhow!("capsule contradicts its captured dependency"))
            .context(SessionCapsuleVerificationStage::CapturedDependency);
    }
    if capsule.executable_search != executable_search {
        return Err(anyhow!(
            "capsule contradicts its executable-search dependency"
        ))
        .context(SessionCapsuleVerificationStage::ExecutableSearch);
    }
    if &capsule.process_environment != environment {
        return Err(anyhow!(
            "capsule contradicts its process-environment contribution"
        ))
        .context(SessionCapsuleVerificationStage::ProcessEnvironment);
    }
    if exact.evidence_attachments != evidence_attachments {
        return Err(anyhow!("capsule contradicts its evidence attachments"))
            .context(SessionCapsuleVerificationStage::EvidenceAttachments);
    }
    validate_exact_evidence_attachments(&exact)
        .context(SessionCapsuleVerificationStage::EvidenceValidation)?;
    let observed_digest = exact
        .resolution_output
        .effective_definition_digest()
        .context(SessionCapsuleVerificationStage::EffectiveDefinition)?;
    if observed_digest.as_str() != exact.effective_definition_digest {
        return Err(anyhow!("exact effective-definition digest changed"))
            .context(SessionCapsuleVerificationStage::EffectiveDefinitionDigest);
    }
    validate_capsule_current_trust(engine, &capsule)
        .context(SessionCapsuleVerificationStage::CurrentTrust)?;
    let (protocol_ref, protocol_digest) = capsule_protocol_identity(&capsule)
        .context(SessionCapsuleVerificationStage::ProtocolIdentity)?;
    let resolution = exact.resolution_output.restore();
    ryeos_app::source_closure_admission::recover_source_closure(state, &state.engine, &resolution)
        .context(SessionCapsuleVerificationStage::SourceClosure)?;
    ryeos_app::external_content_admission::recover_external_realizations(state, &resolution)
        .context(SessionCapsuleVerificationStage::ExternalRealizations)?;
    if let Some(contract) = content_target_contract {
        validate_captured_target_content(state, &resolution, contract, evidence_attachments)
            .context(SessionCapsuleVerificationStage::TargetContent)?;
    }
    super::execution_realization::verify_persistent_session(
        state,
        &capsule,
        &resolution,
        &exact.effective_definition_digest,
        protocol_ref,
        protocol_digest,
    )
    .context(SessionCapsuleVerificationStage::PersistentSessionRealization)?;
    Ok(capsule)
}

pub fn inspect_capsule(
    state: &AppState,
    capsule_hash: &str,
) -> Result<AdmittedPersistentSessionIdentity> {
    let capsule = load_capsule(state, capsule_hash)?;
    validate_capsule_current_trust(&state.engine, &capsule)?;
    let exact = retained_exact_program(&capsule)?;
    validate_exact_evidence_attachments(&exact)?;
    let current_digest = exact.resolution_output.effective_definition_digest()?;
    if current_digest.as_str() != exact.effective_definition_digest {
        bail!("persistent-session exact program digest does not reproduce");
    }
    let resolution = exact.resolution_output.restore();
    ryeos_app::source_closure_admission::recover_source_closure(state, &state.engine, &resolution)?;
    super::execution_realization::verify_persistent_session(
        state,
        &capsule,
        &resolution,
        &exact.effective_definition_digest,
        capsule_protocol_identity(&capsule)?.0,
        capsule_protocol_identity(&capsule)?.1,
    )?;
    Ok(AdmittedPersistentSessionIdentity {
        canonical_ref: exact.resolution_output.root_ref().to_owned(),
        effective_definition_digest: exact.effective_definition_digest,
        capsule_hash: capsule_hash.to_owned(),
        execution_realization_hash: capsule.execution_realization_hash,
    })
}

pub fn execute_capsule<C, D>(
    state: &AppState,
    capsule_hash: &str,
    request_body: Value,
    cancelled: C,
    on_delta: D,
) -> Result<Value>
where
    C: Fn() -> bool,
    D: FnMut(Value) -> Result<()>,
{
    let capsule = load_capsule(state, capsule_hash)?;
    validate_capsule_current_trust(&state.engine, &capsule)?;
    if retained_session_protocol(&state.engine, &capsule)?.process_mode
        != PersistentSessionProcessMode::PooledRequests
    {
        let protocol_ref = capsule_protocol_identity(&capsule)?.0;
        bail!(
            "exclusive persistent-session protocol `{protocol_ref}` cannot enter the request pool"
        );
    }
    let exact = retained_exact_program(&capsule)?;
    validate_exact_evidence_attachments(&exact)?;
    let current_digest = exact.resolution_output.effective_definition_digest()?;
    if current_digest.as_str() != exact.effective_definition_digest {
        bail!("persistent-session exact program digest does not reproduce");
    }
    let resolution = exact.resolution_output.restore();
    super::execution_realization::verify_persistent_session(
        state,
        &capsule,
        &resolution,
        &exact.effective_definition_digest,
        capsule_protocol_identity(&capsule)?.0,
        capsule_protocol_identity(&capsule)?.1,
    )?;
    let pool_key = canonical_hash(&json!({
        "capsule_hash": capsule_hash,
        "execution_realization_hash": capsule.execution_realization_hash,
        "authority": capsule.authority().digest()?,
    }))?;
    let lifecycle = capsule.lifecycle.clone();
    let wire = capsule.wire.clone();
    state.persistent_sessions.execute(
        &pool_key,
        &lifecycle,
        &wire,
        request_body,
        || start_capsule_process(state, capsule_hash, &capsule, &exact),
        cancelled,
        on_delta,
    )
}

/// This is node admission of a retained protocol, not portable document
/// parsing. Refuse an unsupported dedicated lifecycle before creating its
/// worker reservation or spending an upstream model contact.
fn validate_session_process_control(
    state: &AppState,
    session: &ryeos_engine::protocols::descriptor::PersistentSessionProtocol,
) -> Result<()> {
    match (session.process_mode, session.cleanup_authority) {
        (
            PersistentSessionProcessMode::PooledRequests,
            PersistentSessionCleanupAuthority::NotRequired,
        ) => {}
        (
            PersistentSessionProcessMode::ExclusiveSession,
            PersistentSessionCleanupAuthority::LocalProcessScope,
        ) => {
            let readiness = &state
                .isolation
                .inspection()
                .process_scope_readiness
                .exclusive_session;
            if !readiness.ready {
                bail!(
                    "exclusive session requires qualified node process-scope support ({})",
                    readiness.reason.as_str()
                );
            }
            // Readiness is an inspection aid, not launch authority. Recheck the
            // retained generation and exact qualified capability set at admission.
            state
                .isolation
                .process_scope_control_timeout()
                .context("exclusive session requires qualified node process-scope support")?;
        }
        (
            PersistentSessionProcessMode::ExclusiveSession,
            PersistentSessionCleanupAuthority::ExternalPlacementIncarnation,
        ) => {
            bail!("exclusive session requires protected external placement-incarnation authority");
        }
        (
            PersistentSessionProcessMode::ExclusiveSession,
            PersistentSessionCleanupAuthority::TrustedProcessGroup,
        ) => {
            let readiness = &state
                .isolation
                .inspection()
                .process_scope_readiness
                .trusted_exclusive_session;
            if !readiness.ready {
                bail!(
                    "trusted process-group session is disabled by node policy ({})",
                    readiness.reason.as_str()
                );
            }
        }
        (PersistentSessionProcessMode::PooledRequests, _) => {
            bail!("pooled persistent session cannot select dedicated cleanup authority");
        }
        (
            PersistentSessionProcessMode::ExclusiveSession,
            PersistentSessionCleanupAuthority::NotRequired,
        ) => {
            bail!("exclusive persistent session requires dedicated cleanup authority");
        }
    }
    Ok(())
}

fn retained_session_protocol(
    engine: &ryeos_engine::engine::Engine,
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<ryeos_engine::protocols::descriptor::PersistentSessionProtocol> {
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        protocol_ref,
        protocol_content_hash,
        protocol_signer_fingerprint,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("persistent-session protocol has no direct artifact identity");
    };
    let ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
        protocol_descriptor_document,
        execution_plan,
        ..
    } = &capsule.execution_closure
    else {
        bail!("persistent-session protocol has no retained direct closure");
    };
    // The current trust store may revoke the retained signer. It must never
    // replace admitted behavior with a newer descriptor at the same ref.
    let descriptor = verify_and_decode_retained_protocol_document(
        protocol_descriptor_document,
        protocol_content_hash,
        protocol_signer_fingerprint,
        &engine.node_trust_store,
    )
    .map_err(|error| anyhow!("verify retained session protocol: {error}"))?;
    ryeos_engine::protocols::validate_admitted_protocol_descriptor(protocol_ref, &descriptor)?;
    let session =
        validate_persistent_session_protocol(&descriptor).map_err(|error| anyhow!(error))?;
    let plan: ryeos_engine::contracts::ExecutionPlan =
        serde_json::from_value(execution_plan.clone())?;
    if plan
        .filesystem_authority_ceiling
        .intersect(session.workspace_authority.filesystem_ceiling())
        != plan.filesystem_authority_ceiling
        || plan
            .network_authority_ceiling
            .intersect(session.network_authority.network_ceiling())
            != plan.network_authority_ceiling
    {
        bail!("persistent-session plan widens its retained protocol ceilings");
    }
    Ok(session.clone())
}

fn verify_and_decode_retained_protocol_document(
    document: &str,
    expected_content_hash: &str,
    expected_signer: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<ryeos_engine::protocols::ProtocolDescriptor> {
    let body = super::launch::verify_admitted_signed_descriptor_document(
        document,
        expected_content_hash,
        expected_signer,
        trust_store,
    )?;
    decode_retained_protocol_descriptor(&body)
}

/// Decode already-admitted predecessor protocol bytes without weakening
/// current authoring. Before cleanup authority became explicit, pooled
/// sessions had no dedicated cleanup owner and exclusive sessions always
/// required the local Lillux process scope. The signed original bytes are
/// verified before this narrow historical interpretation is applied.
fn decode_retained_protocol_descriptor(
    body: &str,
) -> Result<ryeos_engine::protocols::ProtocolDescriptor> {
    let mut value: serde_json::Value = serde_yaml::from_str(body)?;
    if let Some(session) = value
        .get_mut("session")
        .and_then(serde_json::Value::as_object_mut)
        && !session.contains_key("cleanup_authority")
    {
        let historical = match session
            .get("process_mode")
            .and_then(serde_json::Value::as_str)
        {
            Some("pooled_requests") => "not_required",
            Some("exclusive_session") => "local_process_scope",
            _ => bail!("retained session protocol has no interpretable cleanup authority"),
        };
        session.insert(
            "cleanup_authority".to_owned(),
            serde_json::Value::String(historical.to_owned()),
        );
    }
    serde_json::from_value(value).context("decode retained session protocol descriptor")
}

fn start_capsule_process(
    state: &AppState,
    capsule_hash: &str,
    capsule: &AdmittedPersistentSessionCapsule,
    exact: &PersistentSessionExactProgram,
) -> Result<StartedPersistentSession> {
    let session_protocol = retained_session_protocol(&state.engine, capsule)?;
    if session_protocol.process_mode != PersistentSessionProcessMode::PooledRequests {
        bail!("exclusive persistent-session protocol cannot use the pooled launcher");
    }
    let workspace_name = format!(
        "persistent-session-{}-{:08x}",
        &capsule_hash[..16],
        rand::random::<u32>()
    );
    let (workspace, workspace_lifeline) = ryeos_app::temp_dir_guard::create_projectless_workspace(
        &state.config.runtime_root().cache(),
        &workspace_name,
    )?;
    let mut held = spawn_capsule_process_held(
        state,
        capsule_hash,
        capsule,
        exact,
        &workspace,
        None,
        &session_protocol,
        None,
        &BTreeMap::new(),
        Vec::new(),
        None,
    )?;
    held.lifelines.push(Box::new(workspace_lifeline));
    // The fixed pool becomes the process owner as soon as this constructor
    // succeeds. It has no durable cross-restart attachment: restart recovery
    // deliberately reconstructs an equivalent pooled process from the
    // immutable capsule.
    let running = held.process.release_after_attachment()?;
    Ok(StartedPersistentSession {
        running,
        socket: held.socket,
        lifelines: held.lifelines,
        expected_boot_identity: None,
        observation_sink: None,
    })
}

/// Create a daemon-owned execution view under RyeOS's code-enforced
/// `.ai/cache` snapshot exclusion. Disabled-isolation launches can materialize
/// immutable runtime inputs here without adding them to the project candidate.
fn create_node_owned_runtime_view(workspace: &Path) -> Result<lillux::PinnedDirectory> {
    let mut current = lillux::PinnedDirectory::open(workspace)?
        .ok_or_else(|| anyhow!("persistent-session workspace is missing"))?;
    for component in [".ai", "cache", "ryeos-runtime"] {
        current = current
            .open_or_create_child(std::ffi::OsStr::new(component), 0o700)
            .with_context(|| format!("open node-owned runtime-view component `{component}`"))?;
    }
    current.tighten_owner_private_directory()?;
    Ok(current)
}

fn prepare_session_process_environment(
    capsule: &AdmittedPersistentSessionCapsule,
    workspace: &Path,
    workspace_view: Option<&lillux::InheritedDescriptorAuthority>,
    workspace_authority: ryeos_engine::protocols::descriptor::PersistentSessionWorkspaceAuthority,
    enforced: bool,
) -> Result<(
    ryeos_state::objects::PreparedSessionProcessEnvironment,
    Vec<ryeos_engine::isolation::IsolationWritableRuntimeViewMountAuthority>,
)> {
    use ryeos_state::objects::{
        PreparedSessionProcessEnvironment, SessionProcessEnvironmentValue,
        SessionRuntimeViewDelivery, runtime_view_mount_destination,
    };
    ryeos_state::objects::validate_session_process_environment(&capsule.process_environment)?;
    let mut prepared = PreparedSessionProcessEnvironment {
        bindings: capsule.process_environment.clone(),
        runtime_view_delivery: SessionRuntimeViewDelivery::DescriptorWorkspace,
    };
    if !enforced {
        prepared.validate()?;
        return Ok((prepared, Vec::new()));
    }
    let runtime_views = capsule
        .process_environment
        .iter()
        .filter_map(|(name, value)| match value {
            SessionProcessEnvironmentValue::RuntimeViewDirectory { relative_path } => {
                Some((name, relative_path))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let destinations = runtime_views
        .iter()
        .map(|(name, _)| Ok(((*name).clone(), runtime_view_mount_destination(name)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    prepared.runtime_view_delivery = SessionRuntimeViewDelivery::MountedNamespace { destinations };
    prepared.validate()?;
    let mut mounts = Vec::with_capacity(runtime_views.len());
    if !runtime_views.is_empty() {
        // Borrow the exact workspace incarnation, never its diagnostic host
        // pathname. Only the existing projectless scratch owner has no view.
        let _lease = lillux::retain_fork_sensitive_descriptors();
        let (workspace_is_retained_view, workspace) = match (workspace_view, workspace_authority) {
            (
                Some(view),
                ryeos_engine::protocols::descriptor::PersistentSessionWorkspaceAuthority::RuntimeWorkspace,
            ) => (true, view.clone()),
            (
                None,
                ryeos_engine::protocols::descriptor::PersistentSessionWorkspaceAuthority::EphemeralScratch,
            ) => {
                let scratch = lillux::PinnedDirectory::open(workspace)?
                    .ok_or_else(|| anyhow!("persistent-session scratch workspace is missing"))?
                    .into_inherited_descriptor_path()?;
                (false, scratch)
            }
            _ => bail!("runtime-view preparation contradicts its admitted workspace authority"),
        };
        for (name, relative_path) in runtime_views {
            let mut relative = PathBuf::from(".ai/cache/ryeos-runtime");
            if relative_path != "." {
                relative.push(relative_path);
            }
            let source = workspace.open_or_create_private_directory_descendant(&relative)?;
            mounts.push(if workspace_is_retained_view {
                let workspace_relative_path = relative
                    .to_str()
                    .ok_or_else(|| anyhow!("runtime-view workspace path is not UTF-8"))?
                    .to_owned();
                ryeos_engine::isolation::IsolationWritableRuntimeViewMountAuthority::new_workspace_descendant(
                    name.clone(),
                    workspace_relative_path,
                    source,
                )?
            } else {
                ryeos_engine::isolation::IsolationWritableRuntimeViewMountAuthority::new(
                    name.clone(),
                    source,
                )?
            });
        }
    }
    Ok((prepared, mounts))
}

fn prepare_structured_session_baseline(
    profile: &ryeos_state::objects::AdmittedStructuredSessionProfile,
    source_directory: &lillux::PinnedDirectory,
    state_root: &Path,
    enforced: bool,
) -> Result<Option<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>> {
    // The profile compiler resolves baseline_source relative to the captured
    // source manifest root. The worker-visible mount/entry path is not an
    // ambient daemon path, nor an authority for reopening the baseline.
    let source_file = source_directory
        .open_pinned_regular_descendant(Path::new(&profile.baseline_source), false)?
        .ok_or_else(|| anyhow!("admitted structured-session baseline is missing"))?;
    let bytes = source_file.read_bounded(64 * 1024)?;
    if bytes.is_empty() {
        bail!("admitted structured-session baseline is empty");
    }

    let state_directory = lillux::PinnedDirectory::open(state_root)?
        .ok_or_else(|| anyhow!("structured-session state root is missing"))?;
    let destination_name = std::ffi::OsStr::new(&profile.baseline_destination);
    let incumbent = state_directory
        .open_pinned_regular(destination_name, false)
        .context("open workload compatibility seed through Lillux")?;
    let current_matches = incumbent
        .as_ref()
        .map(|entry| {
            Ok::<bool, anyhow::Error>(
                entry.permission_mode()? == 0o400 && entry.read_bounded(64 * 1024)? == bytes,
            )
        })
        .transpose()?
        .unwrap_or(false);
    if !current_matches {
        state_directory
            .atomic_write_pinned_if_same(destination_name, incumbent.as_ref(), &bytes, 0o400)
            .context("publish workload compatibility seed through Lillux")?;
    }
    if !enforced {
        return Ok(None);
    }
    let source_path = source_file.path().to_path_buf();
    let destination = state_root.join(&profile.baseline_destination);
    let source_descriptor = source_file.inherited_descriptor_authority()?;
    Ok(Some(
        ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new_state_overlay(
            source_path,
            destination,
            source_descriptor,
        ),
    ))
}

fn spawn_capsule_process_held(
    state: &AppState,
    capsule_hash: &str,
    capsule: &AdmittedPersistentSessionCapsule,
    exact: &PersistentSessionExactProgram,
    workspace: &Path,
    workspace_view: Option<&lillux::InheritedDescriptorAuthority>,
    session_protocol: &ryeos_engine::protocols::descriptor::PersistentSessionProtocol,
    state_root: Option<&Path>,
    runtime_environment: &BTreeMap<String, String>,
    mut extra_target_channels: Vec<ryeos_engine::isolation::IsolationTargetChannelAuthority>,
    process_scope: Option<lillux::ProcessScope>,
) -> Result<HeldPersistentSession> {
    let resolution = exact.resolution_output.restore();
    // A typed source-entry consumer does not require a project-code shadow.
    // Keep enforced source outside the retained CoW; creating its authored
    // namespace there would mutate the candidate merely to launch the worker.
    // Disabled isolation retains its separately budgeted private runtime view.
    let source_placement = if state.isolation.is_enforced() {
        super::source_closure::SourceMountPlacement::ExecutionRuntime
    } else {
        super::source_closure::SourceMountPlacement::Project
    };
    super::source_closure::validate_external_mount_separation(
        state,
        &resolution,
        source_placement,
    )?;
    let private_budget = (!state.isolation.is_enforced())
        .then(super::external_content::private_materialization_budget)
        .transpose()?;
    let private_runtime_view;
    let realization_workspace = if state.isolation.is_enforced()
        || session_protocol.process_mode == PersistentSessionProcessMode::PooledRequests
    {
        workspace
    } else {
        private_runtime_view = create_node_owned_runtime_view(workspace)?;
        private_runtime_view.path()
    };
    let bound = if state.isolation.is_enforced() {
        super::external_content::bind_external_realizations(state, &resolution, &workspace)?
    } else {
        super::external_content::bind_external_realizations_in_private_workspace_with_budget(
            state,
            &resolution,
            realization_workspace,
            private_budget
                .as_ref()
                .expect("disabled isolation has a private copy budget"),
        )?
    };
    let (mounts, external_env, mut leases) = match bound {
        Some(bound) => {
            let (mounts, env, leases) = bound.into_spawn_parts();
            (mounts, Some(env), leases)
        }
        None => (Vec::new(), None, Vec::new()),
    };
    if realization_workspace != workspace && !exact.evidence_attachments.is_empty() {
        // The runtime view keeps executable dependencies out of the retained
        // candidate. Evidence also has an admitted project-relative address
        // consumed by the workload itself, whose cwd is `workspace`. Bind the
        // same immutable inputs there before attachment; fold-back derives
        // their operational exclusions from the root capsule on live/recovery
        // paths. The existing runtime-view bindings and environment stay exact.
        let evidence = evidence_realizations(&exact.evidence_attachments)?;
        let admitted = realization_set(&resolution)?;
        for entry in evidence.iter() {
            if !admitted.iter().any(|candidate| candidate == entry) {
                bail!("workspace evidence differs from the admitted session realization");
            }
        }
        let mut evidence_resolution = resolution.clone();
        evidence_resolution.composed.derived.insert(
            ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
            evidence.to_value()?,
        );
        let bound_evidence =
            super::external_content::bind_external_realizations_in_private_workspace_with_budget(
                state,
                &evidence_resolution,
                workspace,
                private_budget
                    .as_ref()
                    .expect("private evidence uses the admitted copy budget"),
            )?
            .ok_or_else(|| anyhow!("admitted workspace evidence was not bound"))?;
        let (evidence_mounts, _, evidence_leases) = bound_evidence.into_spawn_parts();
        if !evidence_mounts.is_empty() {
            bail!("private workspace evidence unexpectedly required isolation mounts");
        }
        leases.extend(evidence_leases);
    }
    let source = if state.isolation.is_enforced() {
        super::source_closure::bind_source(state, &resolution, &workspace, source_placement)?
    } else {
        super::source_closure::bind_source_in_private_workspace_with_budget(
            state,
            &resolution,
            realization_workspace,
            private_budget
                .as_ref()
                .expect("disabled isolation has a private copy budget"),
        )?
    };
    let mut mounts = mounts;
    let (source_env, source_entry) = match source.as_ref() {
        Some(source) => {
            mounts.extend_from_slice(source.mounts());
            (
                Some(source.sealed_identity_env()),
                Some(source.execution_entry_path()),
            )
        }
        None => (None, None),
    };
    if let Some(profile) = capsule.structured_session_profile.as_ref() {
        validate_required_session_environment(profile, &capsule.process_environment)?;
        let bound_source = source
            .as_ref()
            .ok_or_else(|| anyhow!("structured-session capsule has no bound source authority"))?;
        let state_root = state_root
            .ok_or_else(|| anyhow!("structured-session capsule has no exact state root"))?;
        // The admission-compiled immutable argv is the structured workload's
        // configuration authority. An enforced generic isolation backend adds
        // a read-only overlay for the compatibility baseline, but the
        // structured-session substrate does not require one.
        if let Some(overlay) = prepare_structured_session_baseline(
            profile,
            bound_source.source_directory(),
            state_root,
            state.isolation.is_enforced(),
        )? {
            mounts.push(overlay);
        }
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let mut plan =
        ryeos_app::thread_lifecycle::PreparedItemPlan::recover_from_persistent_session_capsule(
            capsule,
            &cas,
            &state.isolation,
            &workspace,
        )?;
    // Recovery has converted the CAS objects into an owned prepared plan.
    // Release the CAS flock (and its fork-sensitive lease) before the direct
    // attachment fork; retaining it here would either make the same-thread
    // fork fail closed or let a held child inherit mutation authority.
    authority.ensure_guard(&guard)?;
    drop(cas);
    drop(guard);
    drop(authority);
    let (daemon_channel, worker_channel) = lillux::inherited_duplex_channel_pair()
        .map_err(anyhow::Error::msg)
        .context("create daemon-owned persistent-session channel")?;
    let daemon_socket = daemon_channel;
    let target_channel = ryeos_engine::isolation::IsolationTargetChannelAuthority::new(
        worker_channel,
        0,
        capsule.wire.channel_env.clone(),
    )?;
    extra_target_channels.push(target_channel);
    extra_target_channels.sort_by_key(|channel| channel.target_fd());
    let executable_search_env = (!capsule.executable_search.is_empty())
        .then(|| {
            lillux::canonical_json(&serde_json::to_value(&capsule.executable_search)?)
                .map_err(anyhow::Error::from)
        })
        .transpose()?;
    let (prepared_environment, writable_runtime_view_mounts) = prepare_session_process_environment(
        capsule,
        workspace,
        workspace_view,
        session_protocol.workspace_authority,
        state.isolation.is_enforced(),
    )?;
    let session_process_environment = (!capsule.process_environment.is_empty())
        .then(|| {
            lillux::canonical_json(&serde_json::to_value(&prepared_environment)?)
                .map_err(anyhow::Error::from)
        })
        .transpose()?;
    plan.bind_persistent_session_spawn_environment(
        external_env.as_deref(),
        external_env.as_ref().map(|_| realization_workspace),
        source_env,
        source_entry,
        executable_search_env.as_deref(),
    )?;
    let mut runtime_environment = runtime_environment.clone();
    if let Some(environment) = session_process_environment {
        let name = ryeos_state::objects::SESSION_PROCESS_ENVIRONMENT_ENV.to_owned();
        if runtime_environment
            .insert(name.clone(), environment)
            .is_some()
        {
            bail!("session process environment collides with runtime authority");
        }
    }
    let mut runtime_env_allowlist = session_protocol.runtime_env_allowlist.clone();
    if let Some(name) = session_protocol.readiness_identity_env.as_ref() {
        runtime_env_allowlist.push(name.clone());
    }
    plan.bind_persistent_session_runtime_environment(&runtime_environment, &runtime_env_allowlist)?;
    // `realization_workspace` is only the daemon-owned location for sealed
    // runtime inputs when outer isolation is disabled.  The process authority
    // remains the canonical runtime-workspace `project` child; substituting
    // the nested realization view here would change the admitted workspace
    // identity and fail the runtime-workspace layout check.
    let process = plan.spawn_persistent_session_held(
        state,
        workspace,
        workspace_view,
        mounts,
        writable_runtime_view_mounts,
        extra_target_channels,
        &capsule.lifecycle,
        session_protocol.workspace_authority,
        session_protocol.network_authority,
        state_root,
        &format!("session-{}", &capsule_hash[..24]),
        process_scope,
    )?;
    let mut lifelines: Vec<Box<dyn Send + Sync>> = Vec::with_capacity(leases.len());
    // The pool owns the exact worker epoch/process; retain its alias in the
    // same lifecycle carrier until that process is retired. A launch plan's
    // temporary descriptor retention alone ends too early for workspace close.
    if let Some(view) = workspace_view {
        lifelines.push(Box::new(view.clone()));
    }
    lifelines.extend(
        leases
            .into_iter()
            .map(|lease| Box::new(lease) as Box<dyn Send + Sync>),
    );
    if let Some(source) = source {
        lifelines.push(Box::new(source));
    }
    Ok(HeldPersistentSession {
        process,
        socket: daemon_socket,
        lifelines,
    })
}

/// Start one session-owned process from an already-admitted capsule and
/// already-ready durable workspace. The exact held identity is committed
/// before Lillux authorizes child execution. The caller holds the existing
/// hosted-root and credential operation fences through completion; cold
/// reattach supplies the newly admitted root incarnation's original lifeline.
pub fn start_exclusive_capsule(
    state: &AppState,
    capsule_hash: &str,
    workspace: &Path,
    workspace_lifeline: std::sync::Arc<ryeos_app::temp_dir_guard::TempDirGuard>,
    state_root: Option<&Path>,
    runtime_environment: &BTreeMap<String, String>,
    extra_target_channels: Vec<ryeos_engine::isolation::IsolationTargetChannelAuthority>,
    identity: &ExclusivePersistentSessionIdentity,
    observation_sink: ryeos_app::persistent_session::PersistentSessionObservationSink,
) -> Result<()> {
    let capsule = load_capsule(state, capsule_hash)?;
    validate_capsule_current_trust(&state.engine, &capsule)?;
    let exact = retained_exact_program(&capsule)?;
    validate_exact_evidence_attachments(&exact)?;
    let session_protocol = retained_session_protocol(&state.engine, &capsule)?;
    use ryeos_engine::protocols::descriptor::PersistentSessionWorkspaceAuthority;
    if session_protocol.process_mode != PersistentSessionProcessMode::ExclusiveSession
        || !matches!(
            session_protocol.cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
                | PersistentSessionCleanupAuthority::TrustedProcessGroup
        )
        || session_protocol.workspace_authority
            != PersistentSessionWorkspaceAuthority::RuntimeWorkspace
    {
        bail!("persistent-session protocol does not authorize an exclusive runtime workspace");
    }
    if !workspace_lifeline.owns_effective_path(workspace) {
        bail!("exclusive session workspace differs from its original owner lifeline");
    }
    // Reuse root admission, not a synthetic worker thread or another registry.
    // Both current LaunchOwner and exact mount incarnation are checked before
    // any realization preparation or subprocess contact can use this view.
    let workspace_view = super::runner::borrow_bound_workspace_view(
        state,
        Some(&workspace_lifeline),
        &identity.placement_thread_id,
    )?;
    let reservation = state.persistent_sessions.reserve_exclusive(
        &identity.placement_thread_id,
        &capsule.lifecycle,
        &capsule.wire,
    )?;
    let mut runtime_environment = runtime_environment.clone();
    if let Some(profile) = capsule.structured_session_profile.as_ref()
        && runtime_environment
            .insert(
                "RYEOS_STRUCTURED_SESSION_PROFILE_HASH".to_owned(),
                profile.profile_hash.clone(),
            )
            .is_some()
    {
        bail!("structured-session profile identity collides with runtime authority");
    }
    if let Some(name) = session_protocol.readiness_identity_env.as_ref() {
        if runtime_environment
            .insert(name.clone(), identity.boot_identity_hash.clone())
            .is_some()
        {
            bail!("readiness identity environment collides with runtime authority");
        }
    } else {
        bail!("exclusive persistent-session protocol requires a readiness identity slot");
    }
    // Reserve against the admitted attempt before possible process contact.
    // This owner (not a provider/kind switch) requires a dedicated execution.
    // Qualified hosts allocate a protected local scope. An explicitly trusted
    // disposable host instead retains the direct process-group lifecycle and
    // makes no containment claim. Neither path may fall back to pooled work.
    let (scope, scope_cleanup) = if session_protocol.cleanup_authority
        == PersistentSessionCleanupAuthority::LocalProcessScope
    {
        let control_timeout = state.isolation.process_scope_control_timeout()?;
        let allocation = state
            .isolation
            .plan_process_scope(&identity.worker_instance_id)?;
        state.state_store.reserve_dedicated_worker_scope(
            &identity.placement_thread_id,
            &identity.worker_instance_id,
            identity.boot_epoch,
            &allocation,
        )?;
        let scope = state
            .isolation
            .allocate_process_scope(&allocation)
            .map_err(|error| {
                let error = anyhow::Error::from(error);
                match allocation.discard_unlaunched() {
                    Ok(()) => error,
                    Err(cleanup) => error
                        .context(format!("unlaunched allocation cleanup unproved: {cleanup}"))
                        .context(ExclusiveWorkerCleanupUnproved),
                }
            })?;
        let scope_recovery = scope.recovery().clone();
        if let Err(error) = state.state_store.bind_dedicated_worker_scope(
            &identity.placement_thread_id,
            &identity.worker_instance_id,
            identity.boot_epoch,
            &scope_recovery,
        ) {
            return Err(match scope.retire_unlaunched(control_timeout) {
                Ok(()) => error,
                Err(retirement) => error.context(format!(
                    "unlaunched scope retirement failed: {retirement}; retained evidence: {scope_recovery:?}"
                )).context(ExclusiveWorkerCleanupUnproved),
            });
        }
        (Some(scope), Some((scope_recovery, control_timeout)))
    } else {
        (None, None)
    };
    let mut held = spawn_capsule_process_held(
        state,
        capsule_hash,
        &capsule,
        &exact,
        workspace,
        workspace_view.as_ref(),
        &session_protocol,
        state_root,
        &runtime_environment,
        extra_target_channels,
        scope,
    )
    .map_err(|error| {
        // Preparation itself can fail after reservation but before spawn.
        // Settle the recorded scope even on that path. An empty scope does
        // not erase an independent unproved wrapper/reap obligation.
        let error = if let Some((scope_recovery, control_timeout)) = scope_cleanup.as_ref() {
            match scope_recovery.terminate_and_wait(*control_timeout) {
                Ok(()) => error,
                Err(cleanup) => {
                    return error
                        .context(format!(
                            "reserved process scope cleanup remains unproved: {cleanup}"
                        ))
                        .context(ExclusiveWorkerCleanupUnproved);
                }
            }
        } else {
            error
        };
        if error
            .downcast_ref::<ryeos_app::persistent_session::PersistentSessionCleanupUnproved>()
            .is_some()
        {
            error.context(ExclusiveWorkerCleanupUnproved)
        } else {
            error
        }
    })?;
    held.lifelines.push(Box::new(workspace_lifeline));
    let now = lillux::time::timestamp_millis() as i64;
    let record = WorkerProcessRecord {
        worker_instance_id: identity.worker_instance_id.clone(),
        boot_identity_hash: identity.boot_identity_hash.clone(),
        session_capsule_hash: capsule_hash.to_owned(),
        boot_epoch: identity.boot_epoch,
        lifecycle_generation: identity.lifecycle_generation,
        process_identity: held.process.process_identity.clone(),
        control_channel_identity: identity.control_channel_identity.clone(),
        state: WorkerProcessState::Attached,
        daemon_generation_id: daemon_generation_id().to_owned(),
        placement_thread_id: identity.placement_thread_id.clone(),
        cleanup_state: "owned".to_owned(),
        created_at_ms: now,
        updated_at_ms: now,
    };
    if let Err(error) = state.state_store.attach_worker_process(&record) {
        let cleanup = held.process.abort_and_reap().err();
        return Err(match cleanup {
            Some(cleanup) => {
                let reason = format!("exclusive held-process attachment cleanup failed: {cleanup}");
                let evidence = state
                    .state_store
                    .fence_unproved_worker_start(&record, &reason);
                let error = error.context(reason);
                match evidence {
                    Ok(()) => error.context(ExclusiveWorkerCleanupUnproved),
                    Err(evidence) => error
                        .context(format!(
                            "persist exact unproved worker evidence failed: {evidence}"
                        ))
                        .context(ExclusiveWorkerCleanupUnproved),
                }
            }
            None => error,
        });
    }
    let running = match held.process.release_after_attachment() {
        Ok(running) => running,
        Err(error) => {
            let mut error = error;
            if let Err(settlement) = state.state_store.settle_worker_process(
                &identity.worker_instance_id,
                &identity.placement_thread_id,
                identity.boot_epoch,
                "unproved",
                "held process release failed",
            ) {
                error = error.context(format!(
                    "persist held-process release failure also failed: {settlement:#}"
                ));
            }
            return Err(error.context(ExclusiveWorkerCleanupUnproved));
        }
    };
    let started = StartedPersistentSession {
        running,
        socket: held.socket,
        lifelines: held.lifelines,
        expected_boot_identity: Some(identity.boot_identity_hash.clone()),
        observation_sink: Some(observation_sink),
    };
    let start_guard = match reservation.bind(started) {
        Ok(start_guard) => start_guard,
        Err(error) => {
            let cleanup_unproved = error
                .downcast_ref::<ryeos_app::persistent_session::PersistentSessionCleanupUnproved>()
                .is_some();
            let cleanup_state = if cleanup_unproved {
                "unproved"
            } else {
                "reaped"
            };
            let mut error = error;
            if let Err(settlement) = state.state_store.settle_worker_process(
                &identity.worker_instance_id,
                &identity.placement_thread_id,
                identity.boot_epoch,
                cleanup_state,
                "exclusive worker readiness failed",
            ) {
                error = error.context(format!(
                    "persist exclusive readiness cleanup also failed: {settlement:#}"
                ));
            }
            return Err(if cleanup_unproved {
                error.context(ExclusiveWorkerCleanupUnproved)
            } else {
                error
            });
        }
    };
    if let Err(error) = state.state_store.complete_worker_binding(
        &identity.worker_instance_id,
        &identity.placement_thread_id,
        identity.boot_epoch,
    ) {
        let mut error = error;
        let cleanup_state = match ryeos_app::dedicated_session_service::retire_worker_process(
            state,
            &identity.placement_thread_id,
            &record,
        ) {
            Ok(cleanup_state) => cleanup_state,
            Err(cleanup) => {
                error = error.context(format!(
                    "retire worker after durable readiness publication failure also failed: {cleanup:#}"
                ));
                "unproved"
            }
        };
        if let Err(settlement) = state.state_store.settle_worker_process(
            &identity.worker_instance_id,
            &identity.placement_thread_id,
            identity.boot_epoch,
            cleanup_state,
            "durable readiness publication failed",
        ) {
            error = error.context(format!(
                "persist durable readiness cleanup also failed: {settlement:#}"
            ));
        }
        return Err(if cleanup_state == "reaped" {
            error
        } else {
            error.context(ExclusiveWorkerCleanupUnproved)
        });
    }
    drop(start_guard);
    // Wake attachment-gated controllers only after the held process has been
    // released, the exclusive transport is bound, and the durable worker row
    // is live. The projection remains the authority; this process-local signal
    // only removes a polling loop and is safe to lose across restart.
    ryeos_app::dedicated_session_service::notify_projection_change(&identity.placement_thread_id);
    Ok(())
}

/// Validate selected workload ingress against the already compiled dependency
/// before releasing a worker boot. No mutable profile lookup or inference from
/// an installed binary may enable an interface.
pub fn validate_workload_client_profile(
    state: &AppState,
    capsule_hash: &str,
    request: &ryeos_runtime::workload_client::WorkloadClientRequestContract,
) -> Result<String> {
    use ryeos_runtime::workload_client::WorkloadClientIngress;
    request.validate()?;
    let capsule = load_capsule(state, capsule_hash)?;
    let profile = capsule
        .structured_session_profile
        .ok_or_else(|| anyhow!("workload client requires a compiled structured-session profile"))?;
    profile.validate()?;
    for binding in &request.bindings {
        let supported = match binding.ingress() {
            WorkloadClientIngress::Cli => {
                profile
                    .contract
                    .pointer("/workload_client/cli_endpoint_env")
                    .and_then(Value::as_str)
                    == Some(ryeos_runtime::workload_client::WORKLOAD_CLIENT_ENDPOINT_ENV)
            }
            WorkloadClientIngress::StructuredSession => profile
                .contract
                .pointer("/workload_client/structured_session")
                .is_some_and(Value::is_object),
        };
        if !supported {
            bail!("selected workload ingress is absent from the admitted protocol profile");
        }
    }
    Ok(profile.profile_hash)
}

fn validate_required_session_environment(
    profile: &ryeos_state::objects::AdmittedStructuredSessionProfile,
    environment: &BTreeMap<String, ryeos_state::objects::SessionProcessEnvironmentValue>,
) -> Result<()> {
    let names = profile
        .contract
        .get("required_process_environment")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow!("compiled profile lacks its required process environment contract")
        })?;
    for name in names {
        let name = name
            .as_str()
            .ok_or_else(|| anyhow!("invalid required environment name"))?;
        if !environment.contains_key(name) {
            bail!("required process environment `{name}` is absent from admitted session inputs");
        }
    }
    Ok(())
}

fn load_capsule(state: &AppState, hash: &str) -> Result<AdmittedPersistentSessionCapsule> {
    if !lillux::valid_hash(hash) {
        bail!("persistent-session capsule hash is not canonical");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let value = cas
        .get_object(hash)?
        .ok_or_else(|| anyhow!("persistent-session capsule {hash} is unavailable"))?;
    authority.ensure_guard(&guard)?;
    let capsule = AdmittedPersistentSessionCapsule::from_current_value(&value)?;
    if capsule.content_hash()? != hash {
        bail!("persistent-session capsule content hash changed");
    }
    Ok(capsule)
}

fn retained_exact_program(
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<PersistentSessionExactProgram> {
    serde_json::from_value(capsule.retained_exact_program()?)
        .context("decode full retained persistent-session exact program")
}

fn validate_capsule_current_trust(
    engine: &ryeos_engine::engine::Engine,
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<()> {
    retained_session_protocol(engine, capsule)?;
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        root_subject_signer_fingerprint,
        root_subject_source_identity,
        protocol_signer_fingerprint,
        executable_identity,
        runtime_identity,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("persistent-session capsule has a non-direct artifact identity");
    };
    let mut signers = Vec::new();
    if let Some(signer) = root_subject_signer_fingerprint.as_deref() {
        signers.push(("root subject", signer));
    }
    signers.push(("protocol", protocol_signer_fingerprint.as_str()));
    signers.push((
        "runtime",
        runtime_identity.runtime_signer_fingerprint.as_str(),
    ));
    if let Some(signer) = runtime_identity
        .runtime_bundle_signer_fingerprint
        .as_deref()
    {
        signers.push(("runtime bundle", signer));
    }
    if let ryeos_state::objects::DirectRootSourceIdentity::Bundle {
        manifest_signer_fingerprint,
        ..
    } = root_subject_source_identity
    {
        signers.push(("root bundle", manifest_signer_fingerprint));
    }
    if let ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
        executor_manifest_signer_fingerprint,
        ..
    } = executable_identity
    {
        signers.push(("executable bundle", executor_manifest_signer_fingerprint));
    }
    for (label, signer) in signers {
        if !engine.node_trust_store.is_trusted(signer) {
            bail!("persistent-session {label} signer is no longer trusted: {signer}");
        }
    }
    Ok(())
}

fn capsule_protocol_identity(capsule: &AdmittedPersistentSessionCapsule) -> Result<(&str, &str)> {
    match &capsule.artifact_identity {
        ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            protocol_ref,
            protocol_content_hash,
            ..
        } => Ok((protocol_ref, protocol_content_hash)),
        _ => bail!("persistent-session capsule has a non-direct artifact identity"),
    }
}

fn direct_request(
    state: &AppState,
    dependency: &PreparedExecutionDependency,
    verified: &ryeos_engine::contracts::VerifiedItem,
    executor_ref: String,
) -> Result<ResolvedExecutionRequest> {
    // An execution dependency is an implementation selected inside a signed
    // runtime launch contract, not a child action dispatched by the outer
    // caller. `resolve_execution_dependencies` has already enforced that
    // contract's kind/space/trust ceiling and captured the exact verified
    // bundle subject. Give plan compilation only the exact execute scope for
    // that captured subject; never borrow the caller's scopes and never mint a
    // wildcard. The outer launch capsule seals both the signed runtime
    // descriptor and this prepared dependency before the session can run.
    dependency.validate()?;
    let principal =
        dependency_plan_principal(state.identity.fingerprint(), &dependency.canonical_ref)?;
    let site = state.threads.site_id().to_owned();
    Ok(ResolvedExecutionRequest {
        kind: verified.resolved.kind.clone(),
        item_ref: dependency.canonical_ref.clone(),
        executor_ref,
        launch_mode: "wait".to_owned(),
        current_site_id: site.clone(),
        origin_site_id: site.clone(),
        target_site_id: None,
        requested_by: Some(state.identity.fingerprint().to_owned()),
        usage_subject: None,
        usage_subject_asserted_by: None,
        parameters: Value::Object(Default::default()),
        ref_bindings: BTreeMap::new(),
        product_selections: Vec::new(),
        resolved_item: verified.resolved.clone(),
        root_raw_content_digest: dependency.subject.raw_content_digest.clone(),
        plan_context: PlanContext {
            requested_by: principal,
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: site.clone(),
            origin_site_id: site,
            execution_hints: ExecutionHints::default(),
            scheduled_fire: None,
            validate_only: false,
        },
        // This is not a separately admitted thread root. The captured-plan
        // builder receives our FinalizedEffectiveProgram for composed policy
        // projection; never substitute the enclosing runtime's root admission.
        root_admission: None,
    })
}

fn dependency_plan_principal(node_fingerprint: &str, item_ref: &str) -> Result<EffectivePrincipal> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)?;
    if canonical.suffix.is_some() || canonical.to_string() != item_ref {
        bail!("persistent-session dependency ref is not exact and unsuffixed");
    }
    let execute_cap =
        ryeos_runtime::authorizer::canonical_cap(&canonical.kind, &canonical.bare_id, "execute");
    Ok(EffectivePrincipal::Local(Principal {
        fingerprint: node_fingerprint.to_owned(),
        scopes: vec![execute_cap],
    }))
}

pub(crate) fn lifecycle_contract(
    declaration: &PersistentSessionDecl,
) -> Result<PersistentSessionLifecycleContract> {
    let contract = PersistentSessionLifecycleContract {
        max_processes: declaration.max_processes,
        max_inflight_per_process: declaration.max_inflight_per_process,
        max_address_space_bytes: declaration.max_address_space_bytes,
        max_cpu_seconds: declaration.max_cpu_seconds,
        real_uid_process_limit: declaration.real_uid_process_limit,
        ready_timeout_ms: declaration.ready_timeout_ms,
        request_timeout_ms: declaration.request_timeout_ms,
        idle_timeout_ms: declaration.idle_timeout_ms,
    };
    contract.validate()?;
    Ok(contract)
}

pub(crate) fn wire_contract(protocol: &VerifiedProtocol) -> Result<PersistentSessionWireContract> {
    let session = validate_persistent_session_protocol(&protocol.descriptor)
        .map_err(|error| anyhow!(error))?;
    let contract = PersistentSessionWireContract {
        channel_env: session.channel_env.clone(),
        wire_protocol: session.wire_protocol.clone(),
        wire_version: session.wire_version,
        max_frame_bytes: session.max_frame_bytes,
    };
    contract.validate()?;
    Ok(contract)
}

/// Canonical identity-space root for persistent-session plans. This is never
/// opened on the host. Recovery relocates it to the daemon-owned workspace in
/// the mutable spawn copy, keeping runtime-root paths out of plan hashes and
/// retained capsules.
fn logical_admission_workspace() -> PathBuf {
    PathBuf::from("/ryeos/persistent-session-workspace")
}

fn canonical_hash(value: &Value) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(value)?.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_predecessor_protocol_gets_only_its_historical_cleanup_meaning() {
        let exclusive = r#"
kind: protocol
name: structured_session
category: ryeos/core
abi_version: v1
stdin: { shape: opaque }
stdout: { shape: opaque_bytes, mode: terminal }
env_injections: []
capabilities:
  allows_pushed_head: false
  allows_target_site: false
  allows_detached: false
lifecycle: { mode: managed }
callback_channel: none
session:
  process_mode: exclusive_session
  workspace_authority: runtime_workspace
  network_authority: node_policy
  runtime_env_allowlist: []
  readiness_identity_env: RYEOS_SESSION_BOOT_IDENTITY
  channel: inherited_unix_socket
  channel_env: RYEOS_SESSION_FD
  framing: u32_be_json
  wire_protocol: fixture.session
  wire_version: 1
  max_frame_bytes: 4096
"#;
        let descriptor = decode_retained_protocol_descriptor(exclusive).unwrap();
        assert_eq!(
            descriptor.session.unwrap().cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );

        let pooled = exclusive
            .replace("exclusive_session", "pooled_requests")
            .replace("runtime_workspace", "ephemeral_scratch")
            .replace("node_policy", "isolated")
            .replace(
                "  readiness_identity_env: RYEOS_SESSION_BOOT_IDENTITY\n",
                "",
            );
        let descriptor = decode_retained_protocol_descriptor(&pooled).unwrap();
        assert_eq!(
            descriptor.session.unwrap().cleanup_authority,
            PersistentSessionCleanupAuthority::NotRequired
        );

        // Current authoring never receives the retained-document bridge.
        assert!(
            serde_yaml::from_str::<ryeos_engine::protocols::ProtocolDescriptor>(exclusive).is_err()
        );
    }

    #[test]
    fn retained_predecessor_protocol_is_verified_before_historical_interpretation() {
        let key = lillux::crypto::SigningKey::from_bytes(&[91u8; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: fingerprint.clone(),
                verifying_key: key.verifying_key(),
                label: None,
            },
        ]);
        let body = r#"kind: protocol
name: structured_session
category: ryeos/core
abi_version: v1
stdin: { shape: opaque }
stdout: { shape: opaque_bytes, mode: terminal }
env_injections: []
capabilities:
  allows_pushed_head: false
  allows_target_site: false
  allows_detached: false
lifecycle: { mode: managed }
callback_channel: none
session:
  process_mode: exclusive_session
  workspace_authority: runtime_workspace
  network_authority: node_policy
  runtime_env_allowlist: []
  readiness_identity_env: RYEOS_SESSION_BOOT_IDENTITY
  channel: inherited_unix_socket
  channel_env: RYEOS_SESSION_FD
  framing: u32_be_json
  wire_protocol: fixture.session
  wire_version: 1
  max_frame_bytes: 4096
"#;
        let document = lillux::signature::sign_content(body, &key, "#", None);
        let content_hash = lillux::signature::content_hash(body);
        let descriptor = verify_and_decode_retained_protocol_document(
            &document,
            &content_hash,
            &fingerprint,
            &trust,
        )
        .unwrap();
        assert_eq!(
            descriptor.session.unwrap().cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );

        let tampered = document.replace("exclusive_session", "pooled_requests");
        assert!(
            verify_and_decode_retained_protocol_document(
                &tampered,
                &content_hash,
                &fingerprint,
                &trust,
            )
            .is_err()
        );
        assert!(
            verify_and_decode_retained_protocol_document(
                &document,
                &"0".repeat(64),
                &fingerprint,
                &trust,
            )
            .is_err()
        );
        let revoked = ryeos_engine::trust::TrustStore::from_signers(Vec::new());
        assert!(
            verify_and_decode_retained_protocol_document(
                &document,
                &content_hash,
                &fingerprint,
                &revoked,
            )
            .is_err()
        );
    }

    #[test]
    fn session_capsule_verification_stages_have_closed_stable_labels() {
        let labels =
            SessionCapsuleVerificationStage::ALL.map(SessionCapsuleVerificationStage::stable_label);
        assert_eq!(
            labels,
            [
                "session-capsule/load",
                "session-capsule/retained-protocol",
                "session-capsule/process-control",
                "session-capsule/exact-program",
                "session-capsule/captured-dependency",
                "session-capsule/executable-search",
                "session-capsule/process-environment",
                "session-capsule/evidence-attachments",
                "session-capsule/evidence-validation",
                "session-capsule/effective-definition",
                "session-capsule/effective-definition-digest",
                "session-capsule/current-trust",
                "session-capsule/protocol-identity",
                "session-capsule/source-closure",
                "session-capsule/external-realizations",
                "session-capsule/target-content",
                "session-capsule/persistent-session-realization",
            ]
        );
        for (stage, label) in SessionCapsuleVerificationStage::ALL.into_iter().zip(labels) {
            assert_eq!(stage.to_string(), label);
        }
        let unique = labels.into_iter().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), SessionCapsuleVerificationStage::ALL.len());
    }

    #[test]
    fn transferred_content_keeps_project_generation_without_overlaying_bundle_dependencies() {
        use ryeos_engine::contracts::ItemSpace;
        use ryeos_engine::item_resolution::ResolutionRoots;
        let project = std::path::PathBuf::from("/admitted/definition-generation");
        let roots = ResolutionRoots::from_registered(Some(project.clone()), &[]);
        let subject = SubjectResolutionAuthority::CowWorkspace {
            base_snapshot_hash: "a".repeat(64),
            current_operational_generation: "b".repeat(64),
        };
        let request = |space, roots: &ResolutionRoots, subject: &SubjectResolutionAuthority| {
            transferred_content_resolution_request(
                ryeos_engine::canonical_ref::CanonicalRef::parse("config:fixture/content").unwrap(),
                space,
                roots,
                subject,
            )
        };
        let bundle = request(ItemSpace::Bundle, &roots, &subject).unwrap();
        assert!(bundle.project_root.is_none());
        assert_eq!(
            bundle.subject_resolution_authority,
            SubjectResolutionAuthority::Projectless
        );
        let pinned = request(ItemSpace::Project, &roots, &subject).unwrap();
        assert_eq!(pinned.project_root, Some(project));
        assert_eq!(pinned.subject_resolution_authority, subject);
        assert!(
            request(
                ItemSpace::Project,
                &roots,
                &SubjectResolutionAuthority::LiveFs
            )
            .is_err()
        );
        assert!(
            request(
                ItemSpace::Project,
                &roots,
                &SubjectResolutionAuthority::Projectless
            )
            .is_err()
        );
        assert!(
            request(
                ItemSpace::Project,
                &ResolutionRoots::from_registered(None, &[]),
                &subject
            )
            .is_err()
        );
        assert!(request(ItemSpace::Node, &roots, &subject).is_err());
    }

    #[test]
    fn evidence_destination_is_strictly_below_its_signed_prefix() {
        validate_evidence_destination("evidence", "evidence/reports/result.json").unwrap();
        assert!(validate_evidence_destination("evidence", "evidence").is_err());
        assert!(validate_evidence_destination("evidence", "other/result.json").is_err());
        assert!(validate_evidence_destination("evidence", "evidence/../secret").is_err());
    }

    #[test]
    fn evidence_realization_retains_its_exact_project_mount_root() {
        let mut binding = PreparedEvidenceAttachment {
            binding_id: "report".to_owned(),
            bundle_id: "fixture".to_owned(),
            event_kind: "report".to_owned(),
            chain_id: "chain-1".to_owned(),
            event_hash: "a".repeat(64),
            attachment_name: "result.json".to_owned(),
            blob_hash: "b".repeat(64),
            size_bytes: 7,
            media_type: Some("application/json".to_owned()),
            target: "worker".to_owned(),
            destination_path: "evidence/result.json".to_owned(),
            access: ryeos_handler_protocol::EvidenceAttachmentAccessWire::ReadOnly,
            manifest_hash: "c".repeat(64),
            binding_digest: String::new(),
        };
        binding.binding_digest = binding.reproduce_binding_digest().unwrap();
        let realized = evidence_realizations(std::slice::from_ref(&binding)).unwrap();
        let entry = realized.iter().next().unwrap();
        assert_eq!(entry.id, "ev-report");
        assert_eq!(
            entry.mount_root,
            ryeos_state::objects::ExternalContentMountRoot::Project
        );
        assert_eq!(entry.mount, binding.destination_path);
        assert_eq!(entry.manifest_hash, binding.manifest_hash);
        assert_eq!(entry.total_bytes, binding.size_bytes);
        assert_eq!(
            ryeos_engine::external_realization::RealizedExternalContentSet::from_value(
                &realized.to_value().unwrap()
            )
            .unwrap(),
            realized
        );
        let mut captured = test_realization("own", "runtime/own", 'd');
        captured.mode = ryeos_state::objects::ExternalContentMode::Captured;
        let mut combined = realized.iter().cloned().collect::<Vec<_>>();
        combined.push(captured.clone());
        combined.push(test_realization(
            "ev-not-an-attachment",
            "runtime/contributed",
            'e',
        ));
        let combined =
            ryeos_engine::external_realization::RealizedExternalContentSet::new(combined).unwrap();
        let budget_entries = captured_target_content_declarations(&combined, &[binding]).unwrap();
        assert_eq!(budget_entries.len(), 2);
        let own = budget_entries
            .iter()
            .find(|entry| entry.id == "own")
            .unwrap();
        assert_eq!(own.digest.as_ref(), Some(&captured.manifest_hash));
        assert!(own.locator.is_none());
        assert!(
            budget_entries
                .iter()
                .any(|entry| entry.id == "ev-not-an-attachment")
        );
    }

    #[test]
    fn absent_content_binding_keeps_its_typed_launch_classification() {
        let error = anyhow::Error::new(
            ryeos_app::external_content_admission::ExternalContentBindingUnavailable {
                manifest_hash: "a".repeat(64),
                consumer_ref: "worker:fixture/exact".to_owned(),
            },
        )
        .context("admit prepared content dependency");
        let dispatch = classify_prepared_session_admission_error(&error).unwrap();
        assert!(matches!(
            dispatch,
            crate::dispatch_error::DispatchError::LaunchResourceNotFound {
                ref code,
                ref binding,
                ..
            } if code == "external_content_binding_unavailable"
                && binding.as_deref() == Some("worker:fixture/exact")
        ));
    }

    #[test]
    fn executable_search_requires_a_materialized_directory() {
        use ryeos_state::objects::ExternalContentManifestEntryKind::{Dir, File, Symlink};

        assert!(manifest_materializes_directory(".", [("bin/tool", File)]));
        assert!(manifest_materializes_directory(
            "bin",
            [("bin", Dir), ("bin/tool", File)]
        ));
        assert!(manifest_materializes_directory(
            "libexec",
            [("libexec/tool", File)]
        ));
        assert!(!manifest_materializes_directory(
            "bin",
            [("bin", File), ("bin/tool", File)]
        ));
        assert!(!manifest_materializes_directory("bin", [("bin", Symlink)]));
        assert!(!manifest_materializes_directory(
            "bin",
            [("sbin/tool", File)]
        ));
    }

    fn test_realization(
        id: &str,
        mount: &str,
        hash_seed: char,
    ) -> ryeos_engine::external_realization::RealizedExternalContent {
        ryeos_engine::external_realization::RealizedExternalContent {
            id: id.to_owned(),
            kind: ryeos_state::objects::ExternalContentKind::Tree,
            mode: ryeos_state::objects::ExternalContentMode::Pinned,
            manifest_hash: std::iter::repeat_n(hash_seed, 64).collect(),
            entry_count: 1,
            total_bytes: 1,
            mount_root: ryeos_state::objects::ExternalContentMountRoot::Project,
            mount: mount.to_owned(),
        }
    }

    #[test]
    fn content_target_projection_reuses_live_collision_and_search_bounds() {
        let valid = BTreeMap::from([(
            "runtime".to_owned(),
            vec![test_realization("toolchain", "opt/toolchain", 'a')],
        )]);
        let search = BTreeMap::from([(
            "runtime".to_owned(),
            vec![ExecutableSearchPathEntry {
                realization_id: "toolchain".to_owned(),
                relative_directory: "bin".to_owned(),
            }],
        )]);
        validate_content_target_aggregation(valid.clone(), search.clone()).unwrap();

        let duplicate_realization = BTreeMap::from([(
            "runtime".to_owned(),
            vec![
                test_realization("toolchain", "opt/toolchain", 'a'),
                test_realization("toolchain", "opt/other", 'b'),
            ],
        )]);
        assert!(
            validate_content_target_aggregation(duplicate_realization, search.clone()).is_err()
        );

        let duplicate_search = BTreeMap::from([(
            "runtime".to_owned(),
            vec![search["runtime"][0].clone(), search["runtime"][0].clone()],
        )]);
        assert!(validate_content_target_aggregation(valid, duplicate_search).is_err());
    }

    #[test]
    fn prepared_content_target_checks_authored_and_all_contributed_declarations() {
        use ryeos_engine::external_content::{DeclaringAuthority, ExternalContentDeclaration};
        use ryeos_state::objects::ExternalContentMountRoot::{ExecutionRuntime, Project};
        let mut contract = ryeos_engine::kind_registry::KindExternalContentDecl {
            realization_derived: "effective_external_realizations".into(),
            allowed_roots: vec![],
            allowed_mount_roots: vec![Project],
            max_declarations: 2,
            large_content: None,
        };
        let declaration = |id: &str, mount_root| {
            serde_json::from_value::<ExternalContentDeclaration>(json!({
                "id":id, "kind":"tree", "mode":"pinned", "digest":"a".repeat(64),
                "mount_root":mount_root, "mount":id
            }))
            .unwrap()
        };
        let own = json!({"external_content":[declaration("own", Project)]});
        let first = declaration("first", Project);
        let second = declaration("second", Project);
        let compile = |contract: &_, entries: &[_]| {
            combined_target_content_declarations(
                &own,
                DeclaringAuthority::Bundle("fixture"),
                contract,
                entries,
            )
        };
        assert_eq!(
            compile(&contract, std::slice::from_ref(&first))
                .unwrap()
                .len(),
            2
        );
        assert!(compile(&contract, &[first.clone(), second.clone()]).is_err());
        contract.max_declarations = 3;
        assert_eq!(
            compile(&contract, &[first.clone(), second]).unwrap().len(),
            3
        );
        assert!(compile(&contract, &[declaration("runtime", ExecutionRuntime)]).is_err());
        assert!(compile(&contract, &[declaration("own", Project)]).is_err());
        contract.allowed_mount_roots.push(ExecutionRuntime);
        assert!(compile(&contract, &[declaration("runtime", ExecutionRuntime)]).is_ok());
        // A future kind edit cannot change the copied receiving contract.
        let retained = serde_json::to_value(&contract).unwrap();
        contract.allowed_mount_roots.clear();
        let retained = serde_json::from_value(retained).unwrap();
        assert!(compile(&retained, &[first]).is_ok());
    }

    #[test]
    fn prepared_content_target_contracts_are_mandatory_and_exact_on_recovery() {
        let exact = retained_program_fixture("/fixture/worker.yaml", 'a');
        let policy = ryeos_engine::runtime_registry::LaunchContentExternalPolicy {
            allowed_mount_roots: vec![ryeos_state::objects::ExternalContentMountRoot::Project],
            max_declarations: 8,
            large_content_max_total_bytes: None,
        };
        let mut dependency = PreparedContentDependency {
            binding: "environment".into(),
            product_selections: Vec::new(),
            canonical_ref: exact.resolution_output.root_ref().into(),
            resolution: exact.resolution_output,
            targets: vec!["worker".into()],
            executable_search: vec![],
            target_content_contracts: BTreeMap::from([(
                "worker".into(),
                policy.declaration_contract(),
            )]),
            external_content_policy: policy,
        };
        dependency.validate().unwrap();
        let wire = serde_json::to_value(&dependency).unwrap();
        let decoded: PreparedContentDependency = serde_json::from_value(wire.clone()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(
            serde_json::to_value(decoded.target_content_contracts).unwrap(),
            wire["target_content_contracts"]
        );
        let mut predecessor = wire;
        predecessor
            .as_object_mut()
            .unwrap()
            .remove("target_content_contracts");
        assert!(serde_json::from_value::<PreparedContentDependency>(predecessor).is_err());
        dependency.target_content_contracts.clear();
        assert!(dependency.validate().is_err());
    }

    #[test]
    fn prepared_content_cross_site_receiver_must_admit_the_retained_ceiling() {
        use ryeos_state::objects::ExternalContentMountRoot::{ExecutionRuntime, Project};
        let retained = ryeos_engine::kind_registry::KindExternalContentDecl {
            realization_derived: "effective_external_realizations".into(),
            allowed_roots: vec![],
            allowed_mount_roots: vec![Project, ExecutionRuntime],
            max_declarations: 4,
            large_content: Some(ryeos_engine::kind_registry::KindLargeContentGrant {
                max_total_bytes: Some(100),
            }),
        };
        require_receiving_content_contract(&retained, Some(&retained)).unwrap();
        assert!(require_receiving_content_contract(&retained, None).is_err());
        let mut receiving = retained.clone();
        receiving.allowed_mount_roots = vec![Project];
        assert!(require_receiving_content_contract(&retained, Some(&receiving)).is_err());
        receiving = retained.clone();
        receiving.max_declarations = 3;
        assert!(require_receiving_content_contract(&retained, Some(&receiving)).is_err());
        receiving = retained.clone();
        receiving.large_content = None;
        assert!(require_receiving_content_contract(&retained, Some(&receiving)).is_err());
        receiving = retained.clone();
        receiving.large_content.as_mut().unwrap().max_total_bytes = Some(99);
        assert!(require_receiving_content_contract(&retained, Some(&receiving)).is_err());
        receiving.large_content.as_mut().unwrap().max_total_bytes = None;
        require_receiving_content_contract(&retained, Some(&receiving)).unwrap();
    }

    fn resource_override_declaration() -> PersistentSessionDecl {
        PersistentSessionDecl {
            target_path: vec!["supported_target".to_owned()],
            max_processes: 1,
            max_inflight_per_process: 1,
            max_address_space_bytes: 64 * 1024 * 1024,
            max_cpu_seconds: 1,
            real_uid_process_limit: 512,
            resource_overrides_path: Some(vec!["session_resources".to_owned()]),
            max_real_uid_process_limit: 4_096,
            ready_timeout_ms: 1,
            request_timeout_ms: 1,
            idle_timeout_ms: 1,
        }
    }

    #[test]
    fn signed_worker_resource_override_is_capped_and_frozen() {
        let mut declaration = resource_override_declaration();
        apply_resource_overrides(
            &mut declaration,
            &json!({"session_resources":{"real_uid_process_limit":1024}}),
        )
        .unwrap();
        assert_eq!(declaration.real_uid_process_limit, 1_024);

        let mut absent = resource_override_declaration();
        apply_resource_overrides(&mut absent, &json!({})).unwrap();
        assert_eq!(absent.real_uid_process_limit, 512);

        let mut excessive = resource_override_declaration();
        assert!(
            apply_resource_overrides(
                &mut excessive,
                &json!({"session_resources":{"real_uid_process_limit":4097}}),
            )
            .is_err()
        );

        let mut unknown = resource_override_declaration();
        assert!(
            apply_resource_overrides(&mut unknown, &json!({"session_resources":{"unknown":1}}),)
                .is_err()
        );
    }

    fn retained_program_fixture(
        source_path: &str,
        body_digest_byte: char,
    ) -> PersistentSessionExactProgram {
        let resolution = ryeos_engine::resolution::ResolutionOutput {
            root: ryeos_engine::resolution::ResolvedAncestor {
                requested_id: "worker:fixture/session".to_owned(),
                resolved_ref: "worker:fixture/session".to_owned(),
                source_path: PathBuf::from(source_path),
                source_space: ryeos_engine::contracts::ItemSpace::Bundle,
                source_root: ryeos_engine::contracts::ItemSourceRoot::Bundle {
                    name: "fixture".to_owned(),
                },
                trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
                signer_fingerprint: Some("f".repeat(64)),
                alias_resolution: None,
                added_by: ryeos_engine::resolution::ResolutionStepName::PipelineInit,
                raw_content: format!("body-{body_digest_byte}"),
                source_content_digest: body_digest_byte.to_string().repeat(64),
                raw_content_digest: body_digest_byte.to_string().repeat(64),
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: BTreeMap::new().into_iter().collect(),
            effective_trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({
                "supported_target": {
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH
                }
            })),
        };
        let digest = resolution.effective_definition_digest().unwrap();
        PersistentSessionExactProgram {
            effective_definition_digest: digest.as_str().to_owned(),
            resolution_output: ryeos_engine::resolution::RetainedResolutionOutput::capture(
                &resolution,
            ),
            evidence_attachments: Vec::new(),
        }
    }

    fn capsule_fixture(
        exact_program: &PersistentSessionExactProgram,
    ) -> AdmittedPersistentSessionCapsule {
        use ryeos_state::objects::{
            AdmittedDirectCommandClosure, AdmittedExecutionClosure, AdmittedLaunchArtifactIdentity,
            DirectExecutableIdentity, DirectRootSourceIdentity, DirectRuntimeIdentity,
            DirectRuntimeSourceSpace,
        };

        let exact_program = serde_json::to_value(exact_program).unwrap();
        let exact_program_hash = canonical_hash(&exact_program).unwrap();
        let executable_blob_hash = "e".repeat(64);
        let execution_path = ryeos_state::objects::admitted_direct_command_execution_path(
            &executable_blob_hash,
            std::path::Path::new("ryeos-session-exec"),
        )
        .unwrap();
        AdmittedPersistentSessionCapsule {
            schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
            kind: PERSISTENT_SESSION_CAPSULE_KIND.to_owned(),
            exact_program,
            exact_program_hash,
            retained_product_selections: None,
            lifecycle: PersistentSessionLifecycleContract {
                max_processes: 1,
                max_inflight_per_process: 1,
                max_address_space_bytes: 64 * 1024 * 1024,
                max_cpu_seconds: 1,
                real_uid_process_limit: 1,
                ready_timeout_ms: 1,
                request_timeout_ms: 1,
                idle_timeout_ms: 1,
            },
            wire: PersistentSessionWireContract {
                channel_env: "RYEOS_SESSION_FD".to_owned(),
                wire_protocol: "fixture.session".to_owned(),
                wire_version: 1,
                max_frame_bytes: 1024,
            },
            artifact_identity: AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                executor_ref: "native:fixture".to_owned(),
                root_subject_source_content_digest: "a".repeat(64),
                root_subject_signer_fingerprint: Some("f".repeat(64)),
                root_subject_source_identity: DirectRootSourceIdentity::Bundle {
                    manifest_hash: "b".repeat(64),
                    manifest_signer_fingerprint: "f".repeat(64),
                },
                protocol_ref: "protocol:fixture/session".to_owned(),
                protocol_content_hash: "c".repeat(64),
                protocol_signer_fingerprint: "f".repeat(64),
                execution_plan_hash: "d".repeat(64),
                executable_identity: DirectExecutableIdentity::CapturedContent {
                    content_hash: executable_blob_hash.clone(),
                },
                runtime_identity: DirectRuntimeIdentity {
                    runtime_ref: "runtime:fixture/session".to_owned(),
                    runtime_source_space: DirectRuntimeSourceSpace::Bundle,
                    runtime_content_hash: "6".repeat(64),
                    runtime_signer_fingerprint: "f".repeat(64),
                    runtime_bundle_manifest_hash: Some("7".repeat(64)),
                    runtime_bundle_signer_fingerprint: Some("f".repeat(64)),
                },
            },
            execution_closure: AdmittedExecutionClosure::DirectItemExecutor {
                execution_plan: json!({}),
                protocol_descriptor_document: "fixture protocol".to_owned(),
                command: AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash,
                    execution_path,
                },
                admitted_project_root: Some(logical_admission_workspace()),
            },
            execution_realization_hash: "8".repeat(64),
            source_binding_hash: None,
            structured_session_profile: None,
            executable_search: Vec::new(),
            process_environment: BTreeMap::new(),
            runtime_ref: "runtime:fixture/session".to_owned(),
            executor_ref: "native:fixture".to_owned(),
        }
    }

    fn downstream_identities(
        exact_program: &PersistentSessionExactProgram,
    ) -> (String, String, String) {
        let mut capsule = capsule_fixture(exact_program);
        let authority = capsule.authority();
        let realization = ryeos_state::objects::AdmittedExecutionRealization {
            schema: ryeos_state::objects::EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
            substrate_identity_hash: "1".repeat(64),
            substrate_attestation_hash: "2".repeat(64),
            launch_authority_digest: authority.digest().unwrap(),
            effective_definition_digest: exact_program.effective_definition_digest.clone(),
            artifact_identity_digest: authority.artifact_identity_digest().unwrap(),
            execution_closure_digest: authority.execution_closure_digest().unwrap(),
            contract_ref: "runtime:fixture/session".to_owned(),
            contract_digest: "3".repeat(64),
            components: Vec::new(),
            properties: BTreeMap::new(),
        };
        let realization_hash = realization.content_hash().unwrap();
        capsule.execution_realization_hash = realization_hash.clone();
        let capsule_hash = capsule.content_hash().unwrap();
        let coordinate = ryeos_provider_contract::RequestCoordinate {
            outer_effective_definition_digest: "4".repeat(64),
            transport: ryeos_provider_contract::TransportCoordinate::AdmittedLocalWorker {
                worker_ref: "worker:fixture/session".to_owned(),
                effective_definition_digest: exact_program.effective_definition_digest.clone(),
                capsule_hash: capsule_hash.clone(),
                execution_realization_hash: realization_hash.clone(),
            },
            provider_family: "local-fixture".to_owned(),
            provider_config_hash: "fixture-config".to_owned(),
            provider_config_value_digest: "5".repeat(64),
            provider_id: "local-fixture".to_owned(),
            profile_id: None,
            model_name: "fixture-model".to_owned(),
            public_headers: Vec::new(),
            credential_header_names: Vec::new(),
            body_sha256: "6".repeat(64),
            requested_output_ceiling: 1,
            credential_binding_hmac: "7".repeat(64),
            credential_authority_generation: "fixture-generation".to_owned(),
            authority_digest: "8".repeat(64),
            admitted_effect_class: Some(ryeos_effect_contract::EffectClass::Recorded),
        };
        let cache_key = coordinate.cache_key().unwrap();
        (capsule_hash, realization_hash, cache_key)
    }

    #[test]
    fn dependency_plan_authority_is_exact_and_never_wildcarded() {
        let EffectivePrincipal::Local(principal) =
            dependency_plan_principal("node-fp", "worker:fixture/session").unwrap()
        else {
            panic!("dependency plan principal must remain local node authority");
        };
        assert_eq!(principal.fingerprint, "node-fp");
        assert_eq!(principal.scopes, ["ryeos.execute.worker.fixture/session"]);
    }

    #[test]
    fn dependency_plan_authority_refuses_suffixes() {
        let error =
            dependency_plan_principal("node-fp", "worker:fixture/session@t:now").unwrap_err();
        assert!(error.to_string().contains("exact and unsuffixed"));
    }

    #[test]
    fn persistent_session_workspace_identity_is_node_path_independent() {
        let logical = logical_admission_workspace();
        assert_eq!(
            logical,
            std::path::Path::new("/ryeos/persistent-session-workspace")
        );
        assert!(!logical.starts_with("/tmp"));
        assert!(!logical.to_string_lossy().contains("cache"));
    }

    #[test]
    fn node_owned_runtime_view_is_snapshot_excluded_and_rejects_symlink_collisions() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let view = create_node_owned_runtime_view(workspace.path()).unwrap();
        assert_eq!(
            view.path().strip_prefix(workspace.path()).unwrap(),
            std::path::Path::new(".ai/cache/ryeos-runtime")
        );
        assert!(
            ryeos_state::project_sync::is_project_snapshot_floor_excluded(
                ".ai/cache/ryeos-runtime/structured-session"
            )
        );

        let colliding_workspace = tempfile::tempdir().unwrap();
        symlink("/tmp", colliding_workspace.path().join(".ai")).unwrap();
        let error = create_node_owned_runtime_view(colliding_workspace.path()).unwrap_err();
        assert!(
            format!("{error:#}").contains("runtime-view component `.ai`"),
            "got {error:#}"
        );
    }

    #[test]
    fn prepared_runtime_views_borrow_exact_workspace_not_replacement_path() {
        use ryeos_engine::protocols::descriptor::PersistentSessionWorkspaceAuthority;
        use ryeos_state::objects::{SessionProcessEnvironmentValue, SessionRuntimeViewDelivery};
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        let view = {
            let _lease = lillux::retain_fork_sensitive_descriptors();
            lillux::PinnedDirectory::open(&workspace)
                .unwrap()
                .unwrap()
                .into_inherited_descriptor_path()
                .unwrap()
        };
        let original = root.path().join("original");
        std::fs::rename(&workspace, &original).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let mut capsule = capsule_fixture(&retained_program_fixture("/fixture/worker.yaml", 'a'));
        capsule.process_environment.insert(
            "CARGO_HOME".to_owned(),
            SessionProcessEnvironmentValue::RuntimeViewDirectory {
                relative_path: "cargo/home".to_owned(),
            },
        );
        let (prepared, mounts) = prepare_session_process_environment(
            &capsule,
            &workspace,
            Some(&view),
            PersistentSessionWorkspaceAuthority::RuntimeWorkspace,
            true,
        )
        .unwrap();
        assert_eq!(mounts.len(), 1);
        assert!(matches!(
            prepared.runtime_view_delivery,
            SessionRuntimeViewDelivery::MountedNamespace { .. }
        ));
        assert!(original.join(".ai/cache/ryeos-runtime/cargo/home").is_dir());
        assert!(!workspace.join(".ai").exists());
        assert!(
            prepare_session_process_environment(
                &capsule,
                &workspace,
                None,
                PersistentSessionWorkspaceAuthority::RuntimeWorkspace,
                true,
            )
            .is_err()
        );
        let (disabled, mounts) = prepare_session_process_environment(
            &capsule,
            &workspace,
            None,
            PersistentSessionWorkspaceAuthority::RuntimeWorkspace,
            false,
        )
        .unwrap();
        assert!(matches!(
            disabled.runtime_view_delivery,
            SessionRuntimeViewDelivery::DescriptorWorkspace
        ));
        assert!(mounts.is_empty());
        assert!(!workspace.join(".ai").exists());

        let (scratch, mounts) = prepare_session_process_environment(
            &capsule,
            &workspace,
            None,
            PersistentSessionWorkspaceAuthority::EphemeralScratch,
            true,
        )
        .unwrap();
        assert!(matches!(
            scratch.runtime_view_delivery,
            SessionRuntimeViewDelivery::MountedNamespace { .. }
        ));
        assert_eq!(mounts.len(), 1);
        assert!(
            workspace
                .join(".ai/cache/ryeos-runtime/cargo/home")
                .is_dir()
        );
    }

    #[test]
    fn structured_session_baseline_does_not_require_enforced_isolation() {
        use std::os::unix::fs::PermissionsExt as _;

        let source_root = tempfile::tempdir().unwrap();
        let source_directory = lillux::PinnedDirectory::open(source_root.path())
            .unwrap()
            .unwrap();
        std::fs::write(
            source_root.path().join("baseline.toml"),
            b"setting = true\n",
        )
        .unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let profile = ryeos_state::objects::AdmittedStructuredSessionProfile {
            profile_hash: "a".repeat(64),
            contract: json!({"fixture": true}),
            schema_hashes: BTreeMap::from([("fixture.json".to_owned(), "b".repeat(64))]),
            baseline_source: "baseline.toml".to_owned(),
            baseline_destination: "config.toml".to_owned(),
        };

        let overlay = prepare_structured_session_baseline(
            &profile,
            &source_directory,
            state_root.path(),
            false,
        )
        .unwrap();

        assert!(overlay.is_none());
        let destination = state_root.path().join("config.toml");
        assert_eq!(std::fs::read(&destination).unwrap(), b"setting = true\n");
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&destination, b"workload = true\n").unwrap();
        let overlay = prepare_structured_session_baseline(
            &profile,
            &source_directory,
            state_root.path(),
            false,
        )
        .unwrap();
        assert!(overlay.is_none());
        assert_eq!(std::fs::read(&destination).unwrap(), b"setting = true\n");
        assert_eq!(
            std::fs::metadata(destination).unwrap().permissions().mode() & 0o777,
            0o400
        );
        // Enforced launch must construct its read-only overlay directly from
        // the same pinned source authority, without a worker mount in this
        // process's namespace. A nested baseline is relative to the captured
        // manifest root, exactly as it was during profile compilation.
        std::fs::create_dir(source_root.path().join("nested")).unwrap();
        std::fs::write(
            source_root.path().join("nested/baseline.toml"),
            b"nested = true\n",
        )
        .unwrap();
        let mut nested = profile;
        nested.baseline_source = "nested/baseline.toml".to_owned();
        let overlay = prepare_structured_session_baseline(
            &nested,
            &source_directory,
            state_root.path(),
            true,
        )
        .unwrap();
        assert!(overlay.is_some());
        assert_eq!(
            std::fs::read(state_root.path().join("config.toml")).unwrap(),
            b"nested = true\n"
        );
        std::os::unix::fs::symlink("baseline.toml", source_root.path().join("linked.toml"))
            .unwrap();
        nested.baseline_source = "linked.toml".to_owned();
        assert!(
            prepare_structured_session_baseline(
                &nested,
                &source_directory,
                state_root.path(),
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn persistent_session_capsule_identity_excludes_resolution_diagnostic_paths() {
        let first = retained_program_fixture("/opt/first/worker.yaml", 'a');
        let second = retained_program_fixture("/srv/second/worker.yaml", 'a');
        let first_capsule = capsule_fixture(&first);
        let second_capsule = capsule_fixture(&second);

        assert_eq!(
            first_capsule.exact_program_hash,
            second_capsule.exact_program_hash
        );
        assert_eq!(
            first_capsule.authority().digest().unwrap(),
            second_capsule.authority().digest().unwrap()
        );
        assert_eq!(
            first_capsule.content_hash().unwrap(),
            second_capsule.content_hash().unwrap()
        );
        assert_eq!(
            downstream_identities(&first),
            downstream_identities(&second)
        );
        let canonical = lillux::canonical_json(&first_capsule.exact_program).unwrap();
        assert!(!canonical.contains("/opt/first"));
        assert!(!canonical.contains("/srv/second"));

        let changed_capsule =
            capsule_fixture(&retained_program_fixture("/opt/first/worker.yaml", 'b'));
        assert_ne!(
            first_capsule.exact_program_hash,
            changed_capsule.exact_program_hash
        );
        assert_ne!(
            first_capsule.content_hash().unwrap(),
            changed_capsule.content_hash().unwrap()
        );
        assert_ne!(
            downstream_identities(&first),
            downstream_identities(&retained_program_fixture("/opt/first/worker.yaml", 'b'))
        );
    }

    #[test]
    fn persistent_session_target_is_checked_before_launch() {
        let path = vec!["supported_target".to_owned()];
        validate_session_target(
            &json!({"supported_target": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH
            }}),
            &path,
        )
        .unwrap();
        let error = validate_session_target(
            &json!({"supported_target": {"os": "other", "arch": "other"}}),
            &path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not admit this"));
    }
}
