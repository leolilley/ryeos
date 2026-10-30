//! Authenticate contract-projected execution evidence at exact coordinates.
//!
//! Signed handlers interpret their own runtime language. They do not authorize
//! child execution: the daemon corroborates every proposed occurrence against
//! its action intent, signed terminal, capsule and retained realization.

use anyhow::{Context as _, bail};
use ryeos_engine::resolution::{ResolutionOutput, TrustClass};
use ryeos_handler_protocol::{
    ExecutionEvidenceCandidateCallWire, ExecutionEvidenceCandidateScopedAttemptWire,
    ExecutionEvidenceDescribeRequest, ExecutionEvidenceDescribeResponse,
    ExecutionEvidenceEventWire, ExecutionEvidenceLimitsWire, ExecutionEvidenceProgramWire,
    ExecutionEvidenceProjectRequest, ExecutionEvidenceProjectResponse,
    ExecutionEvidenceRequiredCallWire, ExecutionEvidenceTerminalWire,
};
use ryeos_state::external_content::products::qualification::{
    ProductQualificationEvidence, ProductQualificationExecutionProof,
    ProductQualificationParticipant, ProductQualificationProjectorIdentity,
    ProductQualificationSubordinateAttemptProof, ProductQualificationVerifier,
    VerifierProcessSettlementAuthority,
};
use ryeos_state::external_content::qualification_execution::QualificationExecutionPurposeView;
use ryeos_state::external_content::runtime_member::exact_runtime_member_hash;
use ryeos_state::objects::{
    AdmittedLaunchArtifactIdentity, AdmittedLaunchCapsule, ExternalContentRealizationSet,
    ThreadEvent, ThreadSnapshot, ThreadStatus, canonical_value_digest,
};
use serde_json::Value;

use super::{CurrentBundleVerifierIdentity, CurrentVerifierContent, CurrentVerifierContext};
use crate::{handler_context::HandlerContext, state::AppState};

fn projection_owner(artifact: &AdmittedLaunchArtifactIdentity) -> (&str, &str) {
    match artifact {
        AdmittedLaunchArtifactIdentity::ManagedRuntime {
            runtime_ref,
            runtime_content_hash,
            ..
        } => (runtime_ref, runtime_content_hash),
        AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            protocol_ref,
            protocol_content_hash,
            ..
        } => (protocol_ref, protocol_content_hash),
    }
}

/// Ephemeral accepted-root authentication. Not serializable or clonable;
/// reservation and contact claim require separately authenticated values.
/// Occurrence liveness is deliberately left to the journal transaction.
pub(crate) struct AuthenticatedConsumerRoot {
    qualification_operation_id: String,
    qualification_profile_digest: String,
    owner: String,
    coordinate: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    selection: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerifierSelection,
    purpose: ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose,
    accepted_project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    operator_grant_digest: String,
    /// Original callback expiry, absent for independent operator recovery.
    callback_expiry: Option<lillux::time::MonotonicDeadline>,
    callback_admission: Option<ConsumerCallbackAdmission>,
}

struct ConsumerCallbackAdmission {
    store: std::sync::Arc<crate::callback_token::CallbackCapabilityStore>,
    token: String,
    grant: crate::callback_token::AdmittedRemoteConsumerGrant,
}

impl AuthenticatedConsumerRoot {
    pub(crate) fn callback_expiry(&self) -> Option<lillux::time::MonotonicDeadline> {
        self.callback_expiry
    }

    pub(crate) fn with_live_callback<T>(
        &self,
        admit: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        match &self.callback_admission {
            Some(callback) => callback.store.with_remote_consumer_admission(
                &callback.token,
                &callback.grant,
                admit,
            ),
            None => admit(),
        }
    }

    pub(crate) fn callback_launch_owner(&self) -> Option<&str> {
        self.callback_admission
            .as_ref()
            .map(|callback| callback.grant.launch_owner.as_str())
    }
    pub(crate) fn qualification_operation_id(&self) -> &str {
        &self.qualification_operation_id
    }

    pub(crate) fn owner_principal(&self) -> &str {
        &self.owner
    }

    pub(crate) fn operator_grant_digest(&self) -> &str {
        &self.operator_grant_digest
    }

    pub(crate) fn accepted_project_authority(
        &self,
    ) -> &ryeos_state::objects::ExecutionProjectAuthority {
        &self.accepted_project_authority
    }

    pub(crate) fn require_attempt(
        &self,
        intent: &ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierAttemptIntent,
        qualification: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationIntent,
    ) -> anyhow::Result<()> {
        use ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose;
        if self
            .callback_expiry
            .is_some_and(|expiry| expiry.has_elapsed())
        {
            bail!("consumer callback expired before attempt admission");
        }
        if qualification.operation_id != self.qualification_operation_id
            || qualification.qualification_profile_digest != self.qualification_profile_digest
            || qualification.owner_principal != self.owner
            || intent.qualification_operation_id != self.qualification_operation_id
            || !matches!(&intent.purpose, RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. } if coordinate == &self.coordinate)
        {
            bail!("consumer attempt differs from authenticated root and occurrence authority");
        }
        Ok(())
    }

    pub(crate) fn coordinate(&self) -> &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate{
        &self.coordinate
    }

    pub(crate) fn require_termination(
        &self,
        intent: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminationIntent,
        qualification: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationIntent,
    ) -> anyhow::Result<()> {
        if self
            .callback_expiry
            .is_some_and(|expiry| expiry.has_elapsed())
            || intent.qualification_operation_id != self.qualification_operation_id
            || qualification.operation_id != self.qualification_operation_id
            || qualification.qualification_profile_digest != self.qualification_profile_digest
            || qualification.owner_principal != self.owner
            || intent.owner_principal != self.owner
        {
            bail!("consumer termination differs from live authenticated qualification authority");
        }
        Ok(())
    }

    pub(crate) fn selection(&self) -> &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerifierSelection{
        &self.selection
    }

    pub(crate) fn purpose(
        &self,
    ) -> &ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose {
        &self.purpose
    }
}

/// Load a born accepted qualification root at its exact coordinate. This
/// authenticates root/purpose/artifact and installed-profile agreement only;
/// it grants no contact, liveness or prerequisite-measurement permission.
pub(crate) fn authenticate_consumer_root(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    context: &HandlerContext,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
) -> anyhow::Result<AuthenticatedConsumerRoot> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    authenticate_consumer_root_for_owner(
        state,
        authority,
        guard,
        limits,
        &context.fingerprint,
        coordinate,
        qualification_operation_id,
    )
}

/// Authenticate the actual live invoking verifier, without synthesizing an
/// operator HandlerContext. The bearer store and admitted root independently
/// corroborate its purpose, source and current launch owner.
pub(crate) fn authenticate_consumer_callback_root(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    token: &str,
    method: &str,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
) -> anyhow::Result<AuthenticatedConsumerRoot> {
    use crate::callback_token::CallbackRuntimeMethodSurface;
    let cap = state
        .callback_tokens
        .validate_token_and_thread(token, &coordinate.accepted_root_id)?;
    let expected = CallbackRuntimeMethodSurface::qualification_remote_consumer();
    expected.authorize(method)?;
    if cap.runtime_method_surface != expected {
        bail!("consumer callback has no exact remote method ceiling");
    }
    let grant = cap
        .remote_consumer_grant
        .as_ref()
        .context("consumer callback has no admitted remote purpose")?;
    grant.validate()?;
    if cap.thread_id != grant.root_thread_id
        || cap.chain_root_id != grant.root_thread_id
        || cap.launch_owner.as_deref() != Some(grant.launch_owner.as_str())
        || cap.item_ref.as_deref() != Some(grant.purpose.verifier_ref.as_str())
        || cap.root_raw_content_digest != grant.root_raw_content_digest
        || cap.effective_definition_digest.as_deref()
            != Some(grant.purpose.verifier_realized_definition_digest.as_str())
        || grant.admitted_capsule_hash != coordinate.accepted_capsule_hash
    {
        bail!("consumer callback differs from its admitted verifier root");
    }
    state
        .state_store
        .assert_launch_owner(&cap.thread_id, &grant.launch_owner)?;
    let root = state
        .state_store
        .get_authoritative_root_thread_snapshot(&cap.thread_id)?
        .context("consumer callback root absent")?;
    if root.status != ThreadStatus::Running {
        bail!("consumer callback root is not running");
    }
    let mut admitted = authenticate_consumer_root_for_owner(
        state,
        authority,
        guard,
        limits,
        &grant.purpose.owner_fingerprint,
        coordinate,
        qualification_operation_id,
    )?;
    if admitted.purpose() != &grant.purpose {
        bail!("consumer callback purpose differs from its sealed root");
    }
    // Recheck expiry/revocation after bounded CAS verification. No cached
    // witness may mint contact after its live bearer has been withdrawn.
    state
        .callback_tokens
        .validate_token_and_thread(token, &coordinate.accepted_root_id)?;
    state
        .state_store
        .assert_launch_owner(&cap.thread_id, &grant.launch_owner)?;
    admitted.callback_expiry = Some(cap.expires_at);
    admitted.callback_admission = Some(ConsumerCallbackAdmission {
        store: state.callback_tokens.clone(),
        token: token.to_owned(),
        grant: grant.clone(),
    });
    Ok(admitted)
}

fn authenticate_consumer_root_for_owner(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    owner_fingerprint: &str,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
) -> anyhow::Result<AuthenticatedConsumerRoot> {
    authority.ensure_guard(guard)?;
    let operator_fingerprint = owner_fingerprint
        .strip_prefix("fp:")
        .context("consumer operator principal is not canonical")?;
    let operator_grant_digest =
        crate::operator_authority::admitted_operator_authority_digest(state, operator_fingerprint)?;
    let qualified = state
        .state_store
        .snapshot_qualification_operation(qualification_operation_id)?
        .context("consumer verification qualification is absent")?;
    if qualified.intent.owner_principal != owner_fingerprint
        || qualified.phase
            != crate::runtime_db::runtime_snapshot_qualification::SnapshotQualificationPhase::OccurrenceBound
        || qualified.occurrence.is_none()
    {
        bail!("consumer verification has no operator-owned bound occurrence");
    }
    let profile = state
        .node_config
        .runtime_snapshot_qualification
        .iter()
        .find(|binding| binding.digest() == qualified.intent.qualification_profile_digest)
        .context("consumer verification lost its exact signed qualification profile")?;
    let producer = state
        .node_config
        .runtime_snapshot_production
        .iter()
        .find(|binding| {
            binding.id() == profile.production_binding_id()
                && binding.digest() == profile.production_binding_digest()
        })
        .context("consumer verification lost its exact signed producer binding")?;
    let source_snapshot = state
        .state_store
        .runtime_snapshot_operation(&qualified.intent.snapshot_operation_id)?
        .context("consumer verification retained snapshot is absent")?;
    if source_snapshot.intent.production_profile_digest != producer.digest()
        || source_snapshot.intent.provider_id != producer.backend()
        || source_snapshot.intent.adapter_artifact_hash != producer.adapter_artifact_hash()
        || source_snapshot.intent.provider_group_id != producer.provider_group_id()
    {
        bail!("consumer verification snapshot differs from its signed producer");
    }
    if qualified.intent.adapter_artifact_hash != producer.adapter_artifact_hash()
        || qualified.intent.provider_spec_digest != profile.provider_spec_digest()
        || qualified.intent.settings_digest != profile.settings_digest()
        || qualified.intent.verifier_artifact_hash != profile.verifier_artifact_hash()
    {
        bail!("consumer verification differs from exact signed occurrence authority");
    }
    let selection = profile.consumer_verifier(coordinate)?;
    selection.validate_coordinate(coordinate)?;
    let root = state
        .state_store
        .get_authoritative_root_thread_snapshot(&coordinate.accepted_root_id)?
        .context("consumer verification accepted root is absent")?;
    if root.thread_id != coordinate.accepted_root_id
        || root.chain_root_id != coordinate.accepted_root_id
        || root.requested_by.as_deref() != Some(owner_fingerprint)
        || root.admitted_launch_capsule_hash.as_deref()
            != Some(coordinate.accepted_capsule_hash.as_str())
    {
        bail!("consumer verification coordinate differs from born operator-owned root");
    }
    let cas = authority.cas_store()?;
    let capsule = AdmittedLaunchCapsule::from_current_value(
        ryeos_state::object_closure::load_exact_capsule_object_with_cas(
            &cas,
            &coordinate.accepted_capsule_hash,
        )?,
    )?;
    require_terminal_invocation(&root, &capsule)?;
    capsule.verify_retained_execution_realization(
        &cas,
        &authority.large_object_store()?,
        authority.trust_store(),
    )?;
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
        &capsule,
    )?;
    let purpose = sealed
        .qualification_purpose()
        .context("consumer verification root has no sealed qualification purpose")?;
    purpose.validate()?;
    let planning = state
        .state_store
        .launch_planning_record_for_owner(&purpose.launch_id, owner_fingerprint)?
        .context("consumer verification root has no owner-bound launch reservation")?;
    if planning.state != "bound"
        || planning.reserved_thread_id != root.thread_id
        || planning.bound_thread_id.as_deref() != Some(root.thread_id.as_str())
    {
        bail!("consumer verification root differs from its accepted launch reservation");
    }
    if purpose.owner_fingerprint != owner_fingerprint {
        bail!("consumer verification differs from sealed qualification purpose");
    }
    purpose.require_consumer_invocation(qualification_operation_id, coordinate)?;
    purpose.validate_remote_consumer_coordinate(coordinate, selection)?;
    match purpose.remote_use(&coordinate.scenario_id)? {
        ryeos_state::external_content::qualification_purpose::QualificationRemoteUse::Worker {
            ..
        } => {
            let content = purpose
                .consumer_content
                .as_ref()
                .context("consumer verification has no accepted consumer content")?;
            super::require_retained_consumer_content_closure(authority, guard, limits, content)?;
            let manifest = ryeos_state::object_closure::load_exact_cas_object_with_cas(
                &cas,
                &content.runtime_realization.manifest_hash,
                limits
                    .max_object_bytes
                    .min(ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64),
            )?;
            if exact_runtime_member_hash(&manifest, &content.runtime_member.relative_path)?
                != content.runtime_member.executable_sha256
            {
                bail!("consumer verification runtime member differs from accepted exact bytes");
            }
        }
        ryeos_state::external_content::qualification_purpose::QualificationRemoteUse::Probe {
            ..
        } => {
            // A probe has no Worker, runtime realization or retained
            // consumer content; its subject is the restored product itself.
            if purpose.consumer_content.is_some() || purpose.consumer_definitions.is_some() {
                bail!("remote probe purpose carries consumer content");
            }
        }
    }
    authority.ensure_guard(guard)?;
    Ok(AuthenticatedConsumerRoot {
        qualification_operation_id: qualified.intent.operation_id.clone(),
        qualification_profile_digest: qualified.intent.qualification_profile_digest.clone(),
        owner: owner_fingerprint.to_owned(),
        coordinate: coordinate.clone(),
        selection: selection.clone(),
        purpose: purpose.clone(),
        accepted_project_authority: capsule.project_authority.clone(),
        operator_grant_digest,
        callback_expiry: None,
        callback_admission: None,
    })
}

fn projector_identity(
    identity: &ryeos_engine::handlers::VerifiedExecutionEvidenceProjectorIdentity,
) -> ProductQualificationProjectorIdentity {
    ProductQualificationProjectorIdentity {
        canonical_ref: identity.canonical_ref.clone(),
        descriptor_content_digest: identity.descriptor_content_digest.clone(),
        descriptor_signer_fingerprint: identity.descriptor_signer_fingerprint.clone(),
        binary_content_digest: identity.binary_content_digest.clone(),
        binary_manifest_digest: identity.binary_manifest_digest.clone(),
        binary_signer_fingerprint: identity.binary_signer_fingerprint.clone(),
    }
}

/// Decide whether the currently installed projector is the same executable
/// authority that interpreted the retained qualification.
///
/// `binary_manifest_digest` records the complete signed executor set of the
/// bundle generation that supplied the projector. That set can change when an
/// unrelated executable in the same bundle changes. It remains useful
/// historical provenance, but it is not part of the projector's semantic
/// identity: the signed descriptor, exact executable bytes, and their signer
/// are. Requiring the aggregate set digest here would invalidate otherwise
/// reusable qualification evidence after any unrelated core-binary release.
fn compatible_projector_identity(
    current: &ProductQualificationProjectorIdentity,
    retained: &ProductQualificationProjectorIdentity,
) -> bool {
    current.canonical_ref == retained.canonical_ref
        && current.descriptor_content_digest == retained.descriptor_content_digest
        && current.descriptor_signer_fingerprint == retained.descriptor_signer_fingerprint
        && current.binary_content_digest == retained.binary_content_digest
        && current.binary_signer_fingerprint == retained.binary_signer_fingerprint
}

fn require_terminal_invocation(
    snapshot: &ThreadSnapshot,
    capsule: &AdmittedLaunchCapsule,
) -> anyhow::Result<()> {
    let invocation = &capsule.sealed_invocation;
    if invocation.get("item_ref").and_then(Value::as_str) != Some(snapshot.item_ref.as_str())
        || invocation.get("requested_by").and_then(Value::as_str)
            != snapshot.requested_by.as_deref()
        || invocation.get("current_site_id").and_then(Value::as_str)
            != Some(snapshot.current_site_id.as_str())
        || invocation.get("origin_site_id").and_then(Value::as_str)
            != Some(snapshot.origin_site_id.as_str())
        || invocation.get("launch_mode").and_then(Value::as_str)
            != Some(snapshot.launch_mode.as_str())
        || capsule.executor_ref != snapshot.executor_ref
        || capsule.project_authority != snapshot.project_authority
        || snapshot.admitted_launch_capsule_hash.as_deref()
            != Some(capsule.content_hash()?.as_str())
    {
        bail!("qualification terminal contradicts its sealed invocation");
    }
    Ok(())
}

#[cfg(test)]
#[path = "execution_evidence_tests.rs"]
mod tests;

fn program(
    resolution: &ResolutionOutput,
    digest: &str,
) -> anyhow::Result<ExecutionEvidenceProgramWire> {
    Ok(ExecutionEvidenceProgramWire {
        canonical_ref: resolution.root.resolved_ref.clone(),
        effective_definition_digest: digest.to_owned(),
        composed: serde_json::from_value(serde_json::to_value(&resolution.composed)?)?,
        ancestor_requested_ids: resolution
            .ancestors
            .iter()
            .map(|ancestor| ancestor.requested_id.clone())
            .collect(),
    })
}

fn described(
    response: ExecutionEvidenceDescribeResponse,
) -> anyhow::Result<Vec<ExecutionEvidenceRequiredCallWire>> {
    match response {
        ExecutionEvidenceDescribeResponse::Described { required_calls } => Ok(required_calls),
        ExecutionEvidenceDescribeResponse::Refused { message } => {
            bail!("execution evidence contract refused program: {message}")
        }
    }
}

fn static_action(
    call: &ExecutionEvidenceRequiredCallWire,
) -> anyhow::Result<ryeos_runtime::callback::ActionPayload> {
    let action: ryeos_runtime::callback::ActionPayload =
        serde_json::from_value(call.request.clone())?;
    // Only ordinary static inline calls are reproducible in this qualification
    // lane. Unsupported mechanics fail closed, independently of kind names.
    if action.operation_id.is_some()
        || action.thread != "inline"
        || action.call.is_some()
        || action.facets.is_some()
        || action.launch_window.is_some()
        || !action.product_selections.is_empty()
        || !action.ref_bindings.is_empty()
    {
        bail!("qualification participant is not a static inline execution");
    }
    Ok(action)
}

fn require_hookless_participant(resolution: &ResolutionOutput) -> anyhow::Result<()> {
    if let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
    {
        let plan = ryeos_engine::hooks::EffectiveHookPlan::from_value(value)?;
        if plan.iter_layers().any(|(_, layer)| !layer.hooks.is_empty()) {
            bail!("qualification leaf participant has executable hooks");
        }
    }
    Ok(())
}

/// Load one exact retained capsule in its own state-only guarded segment.
fn load_capsule(
    authority: &ryeos_state::PinnedStateAuthority,
    capsule_hash: &str,
) -> anyhow::Result<AdmittedLaunchCapsule> {
    super::with_state_guard(authority, |_guard| {
        AdmittedLaunchCapsule::from_current_value(
            ryeos_state::object_closure::load_exact_capsule_object_with_cas(
                &authority.cas_store()?,
                capsule_hash,
            )?,
        )
    })
}

/// Read one exact signed thread history in its own state-only guarded
/// segment. Callers hold no guard; projection of these events runs later.
fn history(
    authority: &ryeos_state::PinnedStateAuthority,
    snapshot: &ThreadSnapshot,
    limits: ExecutionEvidenceLimitsWire,
) -> anyhow::Result<Vec<ThreadEvent>> {
    let mut events = Vec::new();
    let (_, observed) = super::with_state_guard(authority, |guard| {
        authority
            .visit_thread_snapshot_events(
                &snapshot.chain_root_id,
                &snapshot.thread_id,
                u64::from(limits.max_events),
                u64::from(limits.max_request_bytes),
                guard,
                |_, event| events.push(event),
            )?
            .context("qualification exact signed thread history is unavailable")
    })?;
    if ryeos_state::objects::thread_snapshot::hash_snapshot(snapshot)?
        != ryeos_state::objects::thread_snapshot::hash_snapshot(&observed)?
    {
        bail!("qualification terminal changed during exact history proof");
    }
    events.sort_by_key(|event| event.thread_seq);
    if events.len() as u64 != snapshot.last_thread_seq {
        bail!("qualification history does not cover its signed terminal");
    }
    for (index, event) in events.iter().enumerate() {
        event.validate()?;
        if event.thread_seq != index as u64 + 1
            || event.thread_id != snapshot.thread_id
            || event.chain_root_id != snapshot.chain_root_id
            || !event.durability.is_cas_stored()
        {
            bail!("qualification history is not one complete exact thread");
        }
        if matches!(
            event.event_type.as_str(),
            ryeos_state::event_types::THREAD_CONTINUED
                | ryeos_state::event_types::CONTINUATION_REQUESTED
                | ryeos_state::event_types::CONTINUATION_ACCEPTED
        ) {
            bail!("qualification execution has a continuation");
        }
    }
    Ok(events)
}

/// A direct verifier's successful terminal must sign the daemon's exact
/// compare-cleared launch attempt. The mutable runtime slot is checked only
/// against that signed digest, then Lillux rechecks whole-scope emptiness.
/// Managed graph roots have no direct process and prove their direct children
/// independently through the same function.
fn direct_process_settlement(
    state: &AppState,
    snapshot: &ThreadSnapshot,
    artifact: &AdmittedLaunchArtifactIdentity,
    events: &[ThreadEvent],
) -> anyhow::Result<Option<(String, VerifierProcessSettlementAuthority)>> {
    if !matches!(
        artifact,
        AdmittedLaunchArtifactIdentity::DirectItemExecutor { .. }
    ) {
        return Ok(None);
    }
    let digest = signed_terminal_process_settlement_digest(events)?;
    let settlement = state
        .state_store
        .latest_thread_process_settlement(&snapshot.thread_id)?
        .context("direct qualification verifier lost its exact process settlement")?;
    if settlement.thread_id != snapshot.thread_id || settlement.digest()? != digest {
        bail!("direct qualification verifier settlement differs from signed terminal");
    }
    let authority = match settlement.kind {
        crate::runtime_db::ThreadProcessSettlementKind::ReapedScopeEmpty => {
            VerifierProcessSettlementAuthority::ScopeEmpty
        }
        crate::runtime_db::ThreadProcessSettlementKind::ReapedGroupAbsent => {
            if !state
                .isolation
                .inspection()
                .process_scope_readiness
                .trusted_exclusive_session
                .ready
            {
                bail!("node policy does not admit trusted process-group verifier cleanup");
            }
            VerifierProcessSettlementAuthority::TrustedProcessGroupAbsent
        }
        crate::runtime_db::ThreadProcessSettlementKind::RecoveryDeath => {
            bail!("recovery death cannot qualify a direct verifier")
        }
    };
    crate::process::assert_reaped_process_group_absent(&settlement.process_identity)
        .context("direct qualification verifier process authority is not settled")?;
    Ok(Some((digest, authority)))
}

fn signed_terminal_process_settlement_digest(events: &[ThreadEvent]) -> anyhow::Result<String> {
    let mut completed = events
        .iter()
        .filter(|event| event.event_type == ryeos_state::event_types::THREAD_COMPLETED);
    let event = completed
        .next()
        .context("direct qualification verifier has no signed completed event")?;
    if completed.next().is_some() {
        bail!("direct qualification verifier has duplicate completed events");
    }
    let digest = event.payload["process_settlement_witness_digest"]
        .as_str()
        .context("direct qualification verifier terminal has no settlement witness")?;
    if !lillux::valid_hash(digest) {
        bail!("direct qualification verifier settlement digest is invalid");
    }
    Ok(digest.to_owned())
}

/// Project and corroborate retained execution evidence. Projection, policy
/// and verifier re-resolution start handler processes, so the caller must not
/// hold a CAS guard. CAS reads below (capsules, histories, retained blobs,
/// transcript chunks) run in their own short guarded segments. Thread
/// snapshot, action-intent, settlement and attempt-journal reads, including
/// those in `prove_participant`, go through state-store readers that take no
/// caller guard and synchronize internally. Publication re-reads the journal
/// facts this proof names under its own guard.
pub(super) fn prove(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    context: &HandlerContext,
    terminal: &ThreadSnapshot,
    capsule: &AdmittedLaunchCapsule,
    admitted: &ResolutionOutput,
    current: &CurrentBundleVerifierIdentity,
    purpose: &QualificationExecutionPurposeView<'_>,
    project_context_resolver: Option<&dyn super::QualificationProjectContextResolver>,
) -> anyhow::Result<(
    Value,
    ProductQualificationExecutionProof,
    Option<(String, VerifierProcessSettlementAuthority)>,
)> {
    super::require_unguarded_resolution("qualification execution evidence proof")?;
    require_terminal_invocation(terminal, capsule)?;
    crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(capsule)?
        .require_qualification_execution_purpose(purpose)?;
    let subject_id = purpose.subject_declaration_id();
    let subject_hash = purpose.subject_manifest_hash();
    let (contract_ref, contract_digest) = projection_owner(&capsule.artifact_identity);
    let projector = current
        .request_engine
        .resolve_execution_evidence_projector(contract_ref, contract_digest)?
        .context("admitted execution contract declares no evidence projector")?;
    let effective_program = program(admitted, &current.effective_definition_digest)?;
    let required = described(current.request_engine.describe_execution_evidence(
        &projector,
        ExecutionEvidenceDescribeRequest {
            config: projector.declaration.config.clone(),
            effective_program: effective_program.clone(),
        },
    )?)?;
    let current_required = described(current.request_engine.describe_execution_evidence(
        &projector,
        ExecutionEvidenceDescribeRequest {
            config: projector.declaration.config.clone(),
            effective_program: program(&current.resolution, &current.effective_definition_digest)?,
        },
    )?)?;
    if required != current_required {
        bail!("current execution contract changed its required participants");
    }
    let events = history(authority, terminal, projector.declaration.limits)?;
    let root_settlement_digest =
        direct_process_settlement(state, terminal, &capsule.artifact_identity, &events)?;
    let response = current.request_engine.project_execution_evidence(
        &projector,
        ExecutionEvidenceProjectRequest {
            config: projector.declaration.config.clone(),
            effective_program,
            terminal: ExecutionEvidenceTerminalWire {
                status: terminal.status.as_str().to_owned(),
                result: terminal.result.clone().unwrap_or(Value::Null),
                error: serde_json::to_value(&terminal.error)?,
                artifacts: terminal.artifacts.clone(),
            },
            events: events
                .iter()
                .map(|event| ExecutionEvidenceEventWire {
                    thread_seq: event.thread_seq,
                    event_type: event.event_type.clone(),
                    payload: event.payload.clone(),
                })
                .collect(),
        },
    )?;
    let (result, calls, subordinate_attempt) = match response {
        ExecutionEvidenceProjectResponse::Projected {
            result,
            calls,
            subordinate_attempt,
        } => (result, calls, subordinate_attempt),
        ExecutionEvidenceProjectResponse::Refused { message } => {
            bail!("execution evidence contract refused terminal: {message}")
        }
    };
    let scoped_attempt = match subordinate_attempt.as_ref() {
        Some(ryeos_handler_protocol::ExecutionEvidenceCandidateSubordinateAttemptWire::ScopedProducer { coordinate }) => Some(coordinate),
        _ => None,
    };
    let remote_proof = match subordinate_attempt.as_ref() {
        Some(candidate @ ryeos_handler_protocol::ExecutionEvidenceCandidateSubordinateAttemptWire::RemoteConsumer { .. }) => {
            Some(corroborate_remote_consumer_references(state, authority, terminal, capsule, purpose, candidate, root_settlement_digest.as_ref().map(|(digest, _)| digest.as_str()), &result)?.0)
        },
        _ => None,
    };
    let scoped_proof = if let Some(candidate) = scoped_attempt {
        // Re-resolve at proof, not only at launch/release. A clean observation
        // of a different signed recipe cannot qualify the consumer runtime.
        super::resolve_current_bundle_producer_recipe_for_purpose(
            state,
            purpose,
            &candidate.scenario_id,
        )?;
        let settlement = state
            .state_store
            .latest_thread_process_settlement(&terminal.thread_id)?
            .context("scoped qualification verifier has no exact process settlement")?;
        if root_settlement_digest
            .as_ref()
            .map(|(digest, _)| digest.as_str())
            != Some(settlement.digest()?.as_str())
        {
            bail!("scoped qualification owner differs from signed verifier settlement");
        }
        Some(
            crate::scoped_producer_observe::qualification_scoped_attempt_proof(
                state,
                authority,
                &settlement.launch_owner,
                purpose,
                candidate,
            )?,
        )
    } else {
        None
    };
    if calls.len() != required.len() {
        bail!("execution evidence omitted or added a required participant");
    }
    let mut participants = Vec::new();
    for required_call in &required {
        let mut matching = calls
            .iter()
            .filter(|call| call.call_id == required_call.call_id);
        let call = matching
            .next()
            .context("execution evidence omitted a required call")?;
        if matching.next().is_some() {
            bail!("execution evidence repeated a call");
        }
        participants.push(prove_participant(
            state,
            authority,
            limits,
            projector.declaration.limits,
            context,
            terminal,
            &current.realizations,
            subject_id,
            subject_hash,
            required_call,
            call,
            project_context_resolver,
        )?);
    }
    let subordinate_proof = match (scoped_proof, remote_proof) {
        (Some(proof), None) => {
            Some(ProductQualificationSubordinateAttemptProof::ScopedProducer { proof })
        }
        (None, Some(proof)) => {
            Some(ProductQualificationSubordinateAttemptProof::RemoteConsumer { proof })
        }
        (None, None) => None,
        _ => bail!("qualification cannot combine subordinate authority lanes"),
    };
    // A callback-free direct verifier has no subordinate operation. A
    // qualification-only callback verifier must instead name exactly one
    // daemon-corroborated scoped attempt; neither lane can silently claim
    // leafness from the absence of managed child events.
    if required.is_empty() {
        require_hookless_participant(admitted)?;
        require_hookless_participant(&current.resolution)?;
        let AdmittedLaunchArtifactIdentity::DirectItemExecutor { protocol_ref, .. } =
            &capsule.artifact_identity
        else {
            bail!("zero-participant qualification requires a callback-free subprocess contract");
        };
        let protocol = current.request_engine.protocols.require(protocol_ref)?;
        require_zero_participant_lane(
            subordinate_authority(subordinate_proof.as_ref()),
            protocol.descriptor.qualification_callback,
            protocol.descriptor.callback_channel,
        )?;
    }
    Ok((
        result,
        ProductQualificationExecutionProof {
            projection_contract_ref: contract_ref.to_owned(),
            projection_contract_digest: contract_digest.to_owned(),
            projector: projector_identity(&projector.projector),
            participants,
            subordinate_attempt: subordinate_proof,
        },
        root_settlement_digest,
    ))
}

/// Historical record authentication, not contact eligibility or semantic
/// qualification. Journal reads already validate the exact owner-measurement
/// prerequisite after occurrence termination. Never require an expired bearer.
fn corroborate_remote_consumer_references(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    terminal: &ThreadSnapshot,
    capsule: &AdmittedLaunchCapsule,
    purpose: &QualificationExecutionPurposeView<'_>,
    candidate: &ryeos_handler_protocol::ExecutionEvidenceCandidateSubordinateAttemptWire,
    authenticated_settlement_digest: Option<&str>,
    projected_result: &Value,
) -> anyhow::Result<(
    ryeos_state::external_content::products::qualification::ProductQualificationRemoteConsumerProof,
    Value,
)> {
    use crate::runtime_db::restored_verifier_attempt::RestoredVerifierAttemptPhase;
    use crate::runtime_db::runtime_snapshot_qualification_termination::QualificationTerminationPhase;
    use ryeos_external_execution_contract::restored_runtime_measurement::{
        MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES, RemoteVerificationPurpose,
        RestoredVerifierObservation,
    };
    let ryeos_handler_protocol::ExecutionEvidenceCandidateSubordinateAttemptWire::RemoteConsumer {
        operation_id,
        evidence_sha256,
        termination_operation_id,
        terminal_observation_sha256,
    } = candidate
    else {
        bail!("remote corroboration requires a remote candidate")
    };
    let attempt = state
        .state_store
        .restored_verifier_attempt(operation_id)?
        .context("remote proof has no retained verifier attempt")?;
    if attempt.phase != RestoredVerifierAttemptPhase::Observed {
        bail!("remote proof requires an observed verifier attempt");
    }
    let selection = attempt
        .consumer_selection
        .context("remote proof has no protected consumer selection")?;
    let RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. } = &attempt.intent.purpose
    else {
        bail!("remote proof cannot substitute owner measurement");
    };
    purpose.validate_remote_consumer_coordinate(coordinate, &selection)?;
    let consumer_input_blob_hash = attempt
        .consumer_input_blob_hash
        .context("remote proof has no retained canonical input")?;
    let input_bytes = super::with_state_guard(authority, |_guard| {
        authority.cas_store()?.get_blob_bounded(
            &consumer_input_blob_hash,
            ryeos_state::external_content::qualification_consumer_record::MAX_CONSUMER_INPUT_RECORD_BYTES as u64,
        )?.context("remote proof retained input blob absent")
    })?;
    if lillux::sha256_hex(&input_bytes) != consumer_input_blob_hash {
        bail!("remote proof retained input bytes differ from their journal hash");
    }
    let retained_input =
        crate::qualification_consumer_input::RetainedConsumerInput::parse(&input_bytes)?;
    let input = retained_input.input_ref();
    input.validate_attempt(&attempt.intent)?;
    input
        .purpose()
        .require_consumer_invocation(&attempt.intent.qualification_operation_id, coordinate)?;
    if !input
        .purpose()
        .execution_view()?
        .has_same_enclosing_purpose(purpose)
        || *input.selection() != selection
    {
        bail!("remote proof input differs from the exact accepted purpose or selection");
    }
    if coordinate.accepted_root_id != terminal.thread_id
        || terminal.admitted_launch_capsule_hash.as_deref()
            != Some(coordinate.accepted_capsule_hash.as_str())
        || capsule.content_hash()? != coordinate.accepted_capsule_hash
    {
        bail!("remote proof differs from its authenticated terminal capsule");
    }
    let Some(RestoredVerifierObservation::ConsumerRuntime { observation }) = attempt.observation
    else {
        bail!("remote proof has no retained consumer observation");
    };
    observation.validate_for_intent(&attempt.intent)?;
    if observation.evidence_sha256 != *evidence_sha256 || observation.contact_deadline_exceeded {
        bail!("remote proof substituted or exceeded its evidence observation");
    }
    let termination = state
        .state_store
        .qualification_termination_operation(termination_operation_id)?
        .context("remote proof has no retained termination")?;
    if termination.phase != QualificationTerminationPhase::Terminal
        || termination.intent.qualification_operation_id
            != attempt.intent.qualification_operation_id
        || termination.intent.occurrence_id != attempt.intent.restored_occurrence_id
        || termination.intent.owner_principal != purpose.owner_fingerprint()
    {
        bail!("remote proof termination has no exact settled occurrence and owner");
    }
    let terminal_observation = termination
        .observation
        .context("remote proof terminal observation is absent")?;
    terminal_observation.validate_for(&termination.intent)?;
    if terminal_observation.contact_deadline_exceeded
        || canonical_value_digest(&serde_json::to_value(&terminal_observation)?)?
            != *terminal_observation_sha256
    {
        bail!("remote proof terminal observation is not exact and timely");
    }
    let settlement = state
        .state_store
        .latest_thread_process_settlement(&terminal.thread_id)?
        .context("remote proof enclosing verifier settlement is absent")?;
    if authenticated_settlement_digest != Some(settlement.digest()?.as_str()) {
        bail!("remote proof enclosing settlement differs from authenticated verifier settlement");
    }
    let bytes = super::with_state_guard(authority, |_guard| {
        authority
            .cas_store()?
            .get_blob_bounded(evidence_sha256, MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES)?
            .context("remote proof evidence blob is absent")
    })?;
    let evidence = observation.verify_evidence_bytes(&bytes)?;
    match input {
        crate::qualification_consumer_input::ConsumerInputRef::Worker(record) => {
            corroborate_qualification_worker_transcript(
                state,
                authority,
                record,
                &attempt.intent.qualification_operation_id,
                projected_result,
            )?;
        }
        crate::qualification_consumer_input::ConsumerInputRef::Probe(record) => {
            corroborate_remote_probe(state, record, &evidence, projected_result)?;
        }
    }
    Ok((ryeos_state::external_content::products::qualification::ProductQualificationRemoteConsumerProof {
        launch_owner_digest: canonical_value_digest(&serde_json::to_value(&settlement.launch_owner)?)?,
        consumer_input_blob_hash,
        intent: attempt.intent, selection, observation,
        termination_intent: termination.intent, terminal_observation,
    }, evidence))
}

/// Mechanical agreement the daemon can prove for a Worker-less probe: the
/// retained input is the signed scenario's own, its prerequisite is still the
/// retained owner measurement, the result names the same subject, and the
/// guest's reported archive-member digests are the retained ones. The probe's
/// language stays with the signed verifier.
fn corroborate_remote_probe(
    state: &AppState,
    input: &ryeos_state::external_content::qualification_consumer_record::RemoteProbeInputRecord,
    evidence: &Value,
    projected_result: &Value,
) -> anyhow::Result<()> {
    use ryeos_state::external_content::products::qualification::ProductQualificationResult;
    input.validate()?;
    let result = ProductQualificationResult::from_value(projected_result)?;
    if result.subject_manifest_hash != input.purpose.subject_manifest_hash {
        bail!("probe result changed the authenticated input subject");
    }
    let scenario = input
        .purpose
        .policy_source
        .policy
        .producer_scenarios
        .get(&input.coordinate.scenario_id)
        .context("probe proof has no signed scenario")?;
    if !input.matches_scenario(scenario) {
        bail!("probe proof input differs from the signed scenario");
    }
    let prerequisite = state
        .state_store
        .restored_verifier_attempt(&input.coordinate.prerequisite_measurement_attempt_id)?
        .context("probe prerequisite measurement attempt is absent")?;
    let measurement = prerequisite
        .observation
        .as_ref()
        .context("probe prerequisite measurement observation is absent")?
        .owner_measurement()?;
    if measurement.contact_deadline_exceeded
        || measurement.digest()? != input.coordinate.prerequisite_measurement_observation_digest
        || measurement.measurement != input.prerequisite
    {
        bail!("probe prerequisite differs from the retained owner measurement");
    }
    let source = input
        .purpose
        .remote_verifier_sources
        .get(&input.coordinate.scenario_id)
        .context("probe proof has no retained verifier source")?;
    require_evidence_member_digests(evidence, &source.archive_member_digests)
}

/// The guest reports, under the delivery's closed field, the sha256 of each
/// extra executable it was handed. They must equal the digests the retained
/// signed source recorded: the claim then names the exact member bytes.
fn require_evidence_member_digests(
    evidence: &Value,
    retained: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let reported: std::collections::BTreeMap<String, String> =
        match evidence.get(
            ryeos_external_execution::restoration_verifier_delivery::CONSUMER_EVIDENCE_ARCHIVE_MEMBERS_FIELD,
        ) {
            None => std::collections::BTreeMap::new(),
            Some(value) => serde_json::from_value(value.clone())
                .context("probe evidence archive member digests are malformed")?,
        };
    if reported.values().any(|digest| !lillux::valid_hash(digest)) || &reported != retained {
        bail!("probe evidence archive member digests differ from the retained signed source");
    }
    Ok(())
}

/// Join the independent verifier's Q reference to the daemon-owned completed
/// transport and exact accepted program. This grants neither fresh execution
/// nor callback access. Product-language interpretation remains in the signed
/// verifier; these checks authenticate its actual input, bytes and settlement.
fn corroborate_qualification_worker_transcript(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    input: &ryeos_state::external_content::qualification_consumer_record::ConsumerInputRecord,
    qualification_operation_id: &str,
    projected_result: &Value,
) -> anyhow::Result<()> {
    use ryeos_state::external_content::products::qualification::ProductQualificationResult;
    use ryeos_state::external_execution::qualification_session::AdmittedExternalQualificationSessionProgram;

    input.validate()?;
    let result = ProductQualificationResult::from_value(projected_result)?;
    if result.subject_manifest_hash != input.purpose.subject_manifest_hash {
        bail!("Q result changed the authenticated input subject");
    }
    let content = input
        .purpose
        .consumer_content
        .as_ref()
        .context("Q proof has no accepted consumer content")?;
    let limits = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let (witness_hash, witness_source, _) = input.purpose.subject.captured_product()?;
    // Retained Worker bytes and the retained product are state-only reads.
    // The current-generation joins below resolve signed items and run with no
    // guard; the retained transport and transcript are read in a later segment.
    let (profile, source_owner, product) = super::with_state_guard(authority, |guard| {
        let (profile, source_owner) =
            crate::source_closure_admission::compile_retained_structured_worker_profile(
                authority,
                guard,
                &content.worker_source,
            )?;
        let product = super::super::product_receipt::load_product_source(
            state,
            authority,
            guard,
            limits,
            &input.purpose.owner_fingerprint,
            witness_hash,
            witness_source,
            super::super::product_receipt::ProductSourceVerification::Retained,
        )?;
        Ok((profile, source_owner, product))
    })?;
    let expected_program = AdmittedExternalQualificationSessionProgram::from_authenticated_inputs(
        &input.purpose,
        &profile,
        &product,
    )?;
    // All current Worker/environment/execution, producer and independent
    // verifier source joins share one checked generation. This establishes
    // current selection, without requiring callback/contact eligibility.
    let purpose = input.purpose.execution_view()?;
    let contract = state.engine.with_checked_bundle_generation(|generation| {
        let (_, contract) = super::derive_qualification_worker_resolution(
            &input.purpose,
            &expected_program,
            &profile,
            &source_owner,
            &input.coordinate,
            generation,
        )?;
        super::resolve_current_bundle_producer_recipe_for_purpose_in_generation(
            state,
            generation,
            &purpose,
            &input.coordinate.scenario_id,
        )?;
        let verifier = super::prepare_remote_consumer_verifier_from_admitted_sources_in_generation(
            state,
            generation,
            purpose.policy_source(),
            purpose.producer_recipe_sources(),
            purpose.subject_manifest_hash(),
            &input.coordinate.scenario_id,
        )?;
        if Some(
            &verifier.retained_identity(purpose.policy_source(), &input.coordinate.scenario_id)?,
        ) != purpose.remote_verifier_source(&input.coordinate.scenario_id)
        {
            bail!("Q proof changed the accepted independent verifier source");
        }
        verifier.require_protected_selection(&input.selection)?;
        Ok(contract)
    })?;
    let operation = qualification_operation_id;
    super::with_state_guard(authority, |guard| {
        let retained = state
            .state_store
            .qualification_historical_transport_evidence_admitted(
                guard,
                &input.coordinate,
                operation,
                &input.purpose.owner_fingerprint,
            )?;
        if retained.owner.program != expected_program {
            bail!("Q applied program differs from exact accepted consumer/runtime inputs");
        }
        let local = state
            .state_store
            .qualification_local_start(&retained.placement_thread_id)?
            .context("Q proof lost its original local credential custody")?;
        let credential = local
            .credential_custody
            .as_ref()
            .context("Q proof lost its selected credential profile")?;
        if credential.profile_id != contract.credential_profile_id
            || credential.owner_principal != input.purpose.owner_fingerprint
            || credential.contract_digest != contract.digest()?
            || credential.released_at_ms.is_none()
        {
            bail!("Q proof changed its accepted credential use or lacks exact release");
        }
        let reference = crate::qualification_consumer_observation::read_retained_reference(
            state,
            authority,
            guard,
            &input.coordinate,
            operation,
            &input.purpose.owner_fingerprint,
        )?;
        let request = input
            .purpose
            .policy_source
            .policy
            .producer_scenarios
            .get(&input.coordinate.scenario_id)
            .and_then(|scenario| scenario.consumer_worker_request.as_ref())
            .context("Q proof has no original signed Worker setup/turn")?;
        require_qualification_worker_request(
            request,
            &reference.manifest.setup_request_digest,
            &reference.manifest.request_digest,
            reference.manifest.controller_canary.as_ref(),
        )?;
        if reference.manifest_hash != result.q_transcript_manifest_hash()?
            || reference.manifest.controller_canary != local.controller_canary_terminal
            || local.worker_incarnation_digest.as_deref()
                != Some(reference.manifest.process_incarnation_digest.as_str())
        {
            bail!("Q proof substituted its manifest, process incarnation or signed request bytes");
        }
        // Use the shared bounded structural decoder on every retained chunk.
        // The finite verifier separately interprets these same canonical bytes.
        let cas = authority.cas_store()?;
        let chunks = reference
            .manifest
            .chunks
            .iter()
            .map(|chunk| {
                cas.get_blob_bounded(&chunk.blob_hash, chunk.size_bytes)?
                    .context("Q proof lost a manifest-owned transcript chunk")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        reference.manifest.verify_chunks(&chunks)?;
        Ok(())
    })
}

/// Compare the sent setup/turn to the sealed request and the exact retained
/// adapter binding. Template bytes are not the actual turn when a canary was
/// selected. This checks commitments only; no provider grammar is interpreted.
fn require_qualification_worker_request(
    request: &ryeos_state::external_content::products::qualification::ProductQualificationConsumerWorkerRequest,
    setup_digest: &str,
    turn_digest: &str,
    canary: Option<&ryeos_state::external_execution::qualification_controller_canary::QControllerCanaryEvidence>,
) -> anyhow::Result<()> {
    if let Some(canary) = canary {
        canary.validate()?;
    }
    let body = request.bound_controller_canary_body(canary.map(|value| &value.binding))?;
    if setup_digest != canonical_value_digest(&request.session_setup)?
        || turn_digest != canonical_value_digest(&body)?
    {
        bail!("Q proof substituted its signed setup or actual bound request bytes");
    }
    Ok(())
}

fn subordinate_authority(
    proof: Option<&ProductQualificationSubordinateAttemptProof>,
) -> Option<ryeos_engine::protocols::QualificationCallbackAuthority> {
    use ryeos_engine::protocols::QualificationCallbackAuthority;
    proof.map(|proof| match proof {
        ProductQualificationSubordinateAttemptProof::ScopedProducer { .. } => {
            QualificationCallbackAuthority::ScopedProducer
        }
        ProductQualificationSubordinateAttemptProof::RemoteConsumer { .. } => {
            QualificationCallbackAuthority::RemoteConsumer
        }
    })
}

/// Historical joins use retained authority and signed process settlement,
/// never a live callback bearer. Reconstruct the full proof rather than only
/// checking that the claimed operation IDs occur in a journal.
fn verify_retained_remote_consumer(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    evidence: &ProductQualificationEvidence,
    remote: &ryeos_state::external_content::products::qualification::ProductQualificationRemoteConsumerProof,
    history_limits: ExecutionEvidenceLimitsWire,
) -> anyhow::Result<()> {
    // Refuses an absent or invalid closure policy before this traversal; the
    // capsule loads below use their fixed protocol ceiling, not these limits.
    state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let capsule = load_capsule(authority, &evidence.verifier.admitted_launch_capsule_hash)?;
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
        &capsule,
    )?;
    let purpose = sealed
        .qualification_purpose()
        .context("remote retained root has no qualification purpose")?;
    let purpose = purpose.execution_view()?;
    let terminal = state
        .state_store
        .get_authoritative_root_thread_snapshot(&evidence.verifier.thread_id)?
        .context("remote retained verifier terminal absent")?;
    require_terminal_invocation(&terminal, &capsule)?;
    if terminal.status != ThreadStatus::Completed
        || terminal.error.is_some()
        || ryeos_state::objects::thread_snapshot::hash_snapshot(&terminal)?
            != evidence.verifier.terminal_snapshot_hash
    {
        bail!("remote retained verifier differs from its successful terminal");
    }
    let events = history(authority, &terminal, history_limits)?;
    let settlement =
        direct_process_settlement(state, &terminal, &capsule.artifact_identity, &events)?;
    if settlement.as_ref().map(|(digest, _)| digest.as_str())
        != evidence
            .verifier
            .process_settlement_witness_digest
            .as_deref()
        || settlement.as_ref().map(|(_, authority)| *authority)
            != evidence.verifier.process_settlement_authority
    {
        bail!("remote retained enclosing process settlement changed");
    }
    let candidate =
        ryeos_handler_protocol::ExecutionEvidenceCandidateSubordinateAttemptWire::RemoteConsumer {
            operation_id: remote.intent.operation_id.clone(),
            evidence_sha256: remote.observation.evidence_sha256.clone(),
            termination_operation_id: remote.termination_intent.operation_id.clone(),
            terminal_observation_sha256: canonical_value_digest(&serde_json::to_value(
                &remote.terminal_observation,
            )?)?,
        };
    let (reconstructed, _) = corroborate_remote_consumer_references(
        state,
        authority,
        &terminal,
        &capsule,
        &purpose,
        &candidate,
        settlement.as_ref().map(|(digest, _)| digest.as_str()),
        &serde_json::to_value(&evidence.result)?,
    )?;
    if &reconstructed != remote {
        bail!("remote retained proof differs from full historical corroboration");
    }
    // Only a Worker consumer ran a Q conversation, so only it owns a
    // transcript manifest; a probe's evidence must carry none.
    if purpose
        .policy_source()
        .policy
        .consumer_execution_context
        .is_some()
    {
        if evidence
            .qualification_worker_transcript_manifest_hash
            .as_deref()
            != Some(evidence.result.q_transcript_manifest_hash()?)
        {
            bail!("remote retained proof changed its qualification Worker transcript");
        }
    } else if evidence
        .qualification_worker_transcript_manifest_hash
        .is_some()
    {
        bail!("remote probe proof carries a qualification Worker transcript");
    }
    Ok(())
}

fn require_zero_participant_lane(
    subordinate: Option<ryeos_engine::protocols::QualificationCallbackAuthority>,
    protocol_authority: Option<ryeos_engine::protocols::QualificationCallbackAuthority>,
    callback_channel: ryeos_engine::protocol_vocabulary::CallbackChannel,
) -> anyhow::Result<()> {
    use ryeos_engine::protocol_vocabulary::CallbackChannel;
    if subordinate != protocol_authority
        || !matches!(
            (subordinate, callback_channel),
            (None, CallbackChannel::None) | (Some(_), CallbackChannel::Http)
        )
    {
        bail!(
            "zero-participant qualification protocol differs from its exact subordinate authority"
        );
    }
    Ok(())
}

fn prove_participant(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    _limits: ryeos_state::object_closure::ObjectClosureLimits,
    history_limits: ExecutionEvidenceLimitsWire,
    context: &HandlerContext,
    parent: &ThreadSnapshot,
    inherited: &ExternalContentRealizationSet,
    subject_id: &str,
    subject_hash: &str,
    required: &ExecutionEvidenceRequiredCallWire,
    call: &ExecutionEvidenceCandidateCallWire,
    project_context_resolver: Option<&dyn super::QualificationProjectContextResolver>,
) -> anyhow::Result<ProductQualificationParticipant> {
    let action = static_action(required)?;
    if ryeos_runtime::callback::dispatch_action_digest(&action)? != call.action_digest {
        bail!("execution evidence action differs from its static contract");
    }
    let intent = state
        .state_store
        .get_runtime_action_intent(&call.operation_id)?
        .context("qualification occurrence has no daemon action intent")?;
    if intent.operation_id != call.operation_id
        || intent.chain_root_id != parent.chain_root_id
        || intent.first_caller_thread_id != parent.thread_id
        || intent.child_thread_id != call.child_thread_id
        || intent.mode != crate::runtime_db::RuntimeActionMode::Inline
        || intent.child_project_authority.is_some()
        || intent.admitted_launch_capsule_hash.is_some()
        || intent.launch_metadata.is_some()
        || intent.initial_events.is_some()
        || intent.workspace_operation.is_some()
    {
        bail!("qualification occurrence contradicts its daemon-owned inline intent");
    }
    // An inline in-process child lives in its parent's chain with the parent
    // as upstream, so it is read at the parent's chain position.
    let (child, continued, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_continuation_presence(
            &parent.chain_root_id,
            &intent.child_thread_id,
        )?
        .context("qualification participant exact terminal is unavailable")?;
    require_inline_child_placement(&child, parent, &intent.child_thread_id)?;
    if continued
        || child.status != ThreadStatus::Completed
        || child.error.is_some()
        || child.finished_at.is_none()
        || child.requested_by != parent.requested_by
        || child.item_ref != action.item_id
        || child.launch_mode != "wait"
        || child.project_authority != parent.project_authority.clone().for_child()?
        || child.current_site_id != parent.current_site_id
        || child.origin_site_id != parent.origin_site_id
    {
        bail!("qualification participant is not the exact successful owned child");
    }
    let capsule_hash = child
        .admitted_launch_capsule_hash
        .as_deref()
        .context("qualification participant has no capsule")?;
    let (capsule, realization) = super::with_state_guard(authority, |_guard| {
        let cas = authority.cas_store()?;
        let capsule = AdmittedLaunchCapsule::from_current_value(
            ryeos_state::object_closure::load_exact_capsule_object_with_cas(&cas, capsule_hash)?,
        )?;
        let realization = capsule.verify_retained_execution_realization(
            &cas,
            &authority.large_object_store()?,
            authority.trust_store(),
        )?;
        Ok((capsule, realization))
    })?;
    require_terminal_invocation(&child, &capsule)?;
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
        &capsule,
    )?;
    let resolution = sealed.admitted_effective_resolution()?;
    if resolution.effective_trust_class != TrustClass::TrustedBundle
        || capsule.project_authority != child.project_authority
        || capsule.executor_ref != child.executor_ref
        || sealed.item_ref() != action.item_id
        || capsule.sealed_invocation.get("parameters") != Some(&action.params)
        || capsule.sealed_invocation.get("ref_bindings") != Some(&serde_json::json!({}))
        || capsule.sealed_invocation.get("resolved_ref_bindings") != Some(&serde_json::json!({}))
        || !sealed.product_selections().is_empty()
    {
        bail!("qualification participant capsule contradicts its static invocation");
    }
    require_hookless_participant(resolution)?;
    let retained = capsule
        .external_realization_set()?
        .context("qualification participant has no inherited inputs")?;
    if &retained != inherited || inherited.is_empty() {
        bail!("qualification participant did not inherit the exact admitted inputs");
    }
    let raw_result = child
        .result
        .as_ref()
        .context("qualification participant has no result")?;
    if canonical_value_digest(raw_result)? != call.result_digest {
        bail!("qualification receipt result differs from the authoritative child result");
    }
    let logical_root =
        ProductQualificationVerifier::project_root_from_closure(&capsule.execution_closure)?;
    let current = super::resolve_current_bundle_verifier_identity_against_admitted(
        state,
        authority,
        context,
        &action.item_id,
        &action.params,
        CurrentVerifierContext {
            content: CurrentVerifierContent::Inherited(inherited),
            logical_project_root: logical_root.as_deref(),
            binding_subject_authority: Some(sealed.resolution_subject_authority()),
            sealed_request: Some(&sealed),
            project_context_resolver,
            pinned_admission: None,
        },
        Some(resolution),
    )?;
    if current.artifact_identity != capsule.artifact_identity
        || current.effective_definition_digest != sealed.effective_definition_digest().as_str()
    {
        bail!("qualification participant no longer has its exact current execution identity");
    }
    // This bounded lane admits one level of participants, each independently
    // proved callback-free, rather than recursively trusting omitted children.
    let AdmittedLaunchArtifactIdentity::DirectItemExecutor { protocol_ref, .. } =
        &capsule.artifact_identity
    else {
        bail!("qualification participant contract is not callback-free direct execution");
    };
    if current
        .request_engine
        .protocols
        .require(protocol_ref)?
        .descriptor
        .callback_channel
        != ryeos_engine::protocol_vocabulary::CallbackChannel::None
    {
        bail!("qualification participant protocol admits callbacks");
    }
    let _source = crate::source_closure_admission::recover_source_closure(
        state,
        current.request_engine.as_ref(),
        resolution,
    )?;
    let child_events = history(authority, &child, history_limits)?;
    let process_settlement =
        direct_process_settlement(state, &child, &capsule.artifact_identity, &child_events)?;
    Ok(ProductQualificationParticipant {
        call_id: required.call_id.clone(),
        operation_id: call.operation_id.clone(),
        request_hash: intent.request_hash,
        action_digest: call.action_digest.clone(),
        inherited_realizations_digest: canonical_value_digest(&retained.to_value()?)?,
        verifier: ProductQualificationVerifier {
            chain_root_id: child.chain_root_id.clone(),
            thread_id: child.thread_id.clone(),
            admitted_launch_capsule_hash: capsule_hash.to_owned(),
            canonical_ref: action.item_id,
            effective_definition_digest: current.effective_definition_digest,
            exact_program_hash: capsule.exact_program_hash.clone(),
            admitted_parameters_digest: sealed.admitted_parameters_digest()?,
            launch_authority_digest: capsule.launch_authority_digest()?,
            execution_realization_hash: capsule.execution_realization_hash.clone(),
            artifact_identity: capsule.artifact_identity.clone(),
            admitted_project_root: logical_root,
            substrate_identity_hash: realization.substrate_identity_hash,
            subject_declaration_id: subject_id.to_owned(),
            subject_manifest_hash: subject_hash.to_owned(),
            terminal_snapshot_hash: ryeos_state::objects::thread_snapshot::hash_snapshot(&child)?,
            process_settlement_witness_digest: process_settlement
                .as_ref()
                .map(|(digest, _)| digest.clone()),
            process_settlement_authority: process_settlement.map(|(_, authority)| authority),
            result_digest: call.result_digest.clone(),
        },
    })
}

fn require_inline_child_placement(
    child: &ThreadSnapshot,
    parent: &ThreadSnapshot,
    expected_child_id: &str,
) -> anyhow::Result<()> {
    // The daemon action intent above owns dispatch-parent linkage. An inline
    // in-process child is a thread in its parent's chain with the parent as
    // upstream, exactly like an inline tool leaf; any other placement is not
    // the child that intent dispatched.
    if child.thread_id != expected_child_id
        || child.thread_id == child.chain_root_id
        || child.chain_root_id != parent.chain_root_id
        || child.upstream_thread_id.as_deref() != Some(parent.thread_id.as_str())
    {
        bail!("qualification participant is not the exact inline child of its parent");
    }
    Ok(())
}

/// Fresh consumption rechecks current handler and participant identities. It
/// does not reopen historical threads or reinterpret a retained attestation.
///
/// Callers must not hold a CAS guard: projector description and participant
/// re-resolution start handler processes. Retained capsules, histories and
/// observations are read in their own short guarded segments.
pub(in crate::operator_external_content) fn verify_current(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    context: &HandlerContext,
    evidence: &ProductQualificationEvidence,
    current: &CurrentBundleVerifierIdentity,
    project_context_resolver: Option<&dyn super::QualificationProjectContextResolver>,
) -> anyhow::Result<()> {
    super::require_unguarded_resolution("current qualification evidence check")?;
    let proof = &evidence.execution_proof;
    let projector = current
        .request_engine
        .resolve_execution_evidence_projector(
            &proof.projection_contract_ref,
            &proof.projection_contract_digest,
        )?
        .context("current execution contract no longer supports qualification evidence")?;
    if !compatible_projector_identity(&projector_identity(&projector.projector), &proof.projector) {
        bail!("qualification evidence projector identity changed");
    }
    let required = described(current.request_engine.describe_execution_evidence(
        &projector,
        ExecutionEvidenceDescribeRequest {
            config: projector.declaration.config.clone(),
            effective_program: program(&current.resolution, &current.effective_definition_digest)?,
        },
    )?)?;
    if required.len() != proof.participants.len() {
        bail!("qualification required participant set changed");
    }
    if required.is_empty() {
        let AdmittedLaunchArtifactIdentity::DirectItemExecutor { protocol_ref, .. } =
            &current.artifact_identity
        else {
            bail!("zero-participant qualification no longer has direct execution");
        };
        let protocol = state.engine.protocols.require(protocol_ref)?;
        require_zero_participant_lane(
            subordinate_authority(proof.subordinate_attempt.as_ref()),
            protocol.descriptor.qualification_callback,
            protocol.descriptor.callback_channel,
        )?;
    }
    if let Some(ProductQualificationSubordinateAttemptProof::RemoteConsumer { proof: remote }) =
        &proof.subordinate_attempt
    {
        verify_retained_remote_consumer(
            state,
            authority,
            evidence,
            remote,
            projector.declaration.limits,
        )?;
    }
    if let Some(scoped) = proof.subordinate_attempt.as_ref().and_then(ryeos_state::external_content::products::qualification::ProductQualificationSubordinateAttemptProof::scoped_producer) {
        let AdmittedLaunchArtifactIdentity::DirectItemExecutor { protocol_ref, .. } =
            &current.artifact_identity
        else {
            bail!("current scoped qualification verifier is not a direct execution");
        };
        let protocol = state.engine.protocols.require(protocol_ref)?;
        if protocol.descriptor.qualification_callback != Some(ryeos_engine::protocols::QualificationCallbackAuthority::ScopedProducer)
            || protocol.descriptor.callback_channel
                != ryeos_engine::protocol_vocabulary::CallbackChannel::Http
            || scoped.callback_method_surface_digest
                != crate::callback_token::CallbackRuntimeMethodSurface::qualification_scoped_producer()
                    .exact_surface_digest()?
        {
            bail!("current scoped qualification callback authority changed");
        }
        let current_policy = super::resolve_current_bundle_qualification_policy(
            state,
            &evidence.policy_source.canonical_ref,
        )?;
        if current_policy != evidence.policy_source {
            bail!("current scoped qualification policy changed");
        }
        let scenario = current_policy
            .policy
            .producer_scenarios
            .get(&scoped.scenario_id)
            .context("current scoped qualification scenario disappeared")?;
        let scenario_recipe_ref = scenario
            .recipe_ref
            .as_deref()
            .context("current scoped qualification scenario has no producer recipe")?;
        let current_recipe = state.engine.with_checked_bundle_generation(|generation| {
            super::resolve_bundle_producer_recipe_in_generation(generation, scenario_recipe_ref)
        })?;
        // Retained evidence is re-verified in later daemon processes, whose
        // bundle generation identity differs; only the signed content identity
        // of the recipe is comparable across them.
        let current_source = current_recipe.source_identity()?;
        if current_source.content_identity() != scoped.producer_source.content_identity() {
            bail!("current scoped qualification recipe source changed");
        }
        super::require_direct_consumer_target(
            &current_policy,
            &current_recipe,
            &evidence.result.subject_manifest_hash,
        )?;

        // The signed qualification carries the daemon's historical proof, but
        // fresh admission must still find the exact settled local attempt and
        // committed observation. Reuse issuance corroboration so a copied
        // proof cannot stand in for a missing or different journal record.
        // Refuses an absent or invalid closure policy before this traversal; the
        // capsule loads below use their fixed protocol ceiling, not these limits.
        state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
            .closure_limits()?;
        let capsule = load_capsule(authority, &evidence.verifier.admitted_launch_capsule_hash)?;
        let sealed =
            crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
                &capsule,
            )?;
        let purpose = sealed
            .qualification_purpose()
            .context("scoped qualification verifier lost its sealed launch purpose")?;
        let (subject_witness_hash, subject_witness_source, _) = purpose
            .subject
            .captured_product()
            .context("scoped qualification verifier has no captured product subject")?;
        if purpose.policy_source != evidence.policy_source
            || subject_witness_hash != evidence.product_witness_hash.as_str()
            || subject_witness_source != &evidence.witness_source
            || purpose.subject_manifest_hash != evidence.result.subject_manifest_hash
            || purpose.subject_declaration_id
                != evidence.policy_source.policy.subject_declaration_id
            || purpose.consumer_content != evidence.consumer_content
            || purpose.verifier_ref != evidence.verifier.canonical_ref
            || purpose.verifier_realized_definition_digest
                != evidence.verifier.effective_definition_digest
            || purpose.admitted_parameters_digest != evidence.verifier.admitted_parameters_digest
        {
            bail!("scoped qualification evidence differs from sealed launch purpose");
        }
        let settlement = state
            .state_store
            .latest_thread_process_settlement(&evidence.verifier.thread_id)?
            .context("scoped qualification verifier lost its exact process settlement")?;
        let settlement_digest = settlement.digest()?;
        if settlement.thread_id != evidence.verifier.thread_id
            || Some(settlement_digest.as_str())
                != evidence
                    .verifier
                    .process_settlement_witness_digest
                    .as_deref()
            || matches!(
                settlement.kind,
                crate::runtime_db::ThreadProcessSettlementKind::RecoveryDeath
            )
        {
            bail!("scoped qualification verifier settlement differs from retained proof");
        }
        let candidate = ExecutionEvidenceCandidateScopedAttemptWire {
            attempt_id: scoped.attempt_id.clone(),
            scenario_id: scoped.scenario_id.clone(),
            observation_object_hash: scoped.observation_object_hash.clone(),
        };
        let purpose_view = purpose.execution_view()?;
        let observed = crate::scoped_producer_observe::qualification_scoped_attempt_proof(
            state,
            authority,
            &settlement.launch_owner,
            &purpose_view,
            &candidate,
        )?;
        if observed != *scoped {
            bail!("scoped qualification proof differs from settled daemon observation");
        }
    }
    for required in required {
        let retained = proof
            .participants
            .iter()
            .find(|participant| participant.call_id == required.call_id)
            .context("qualification required participant is absent")?;
        let action = static_action(&required)?;
        if action.item_id != retained.verifier.canonical_ref
            || ryeos_runtime::callback::dispatch_action_digest(&action)? != retained.action_digest
            || canonical_value_digest(&current.realizations.to_value()?)?
                != retained.inherited_realizations_digest
        {
            bail!("qualification participant's current request or inputs changed");
        }
        // Refuses an absent or invalid closure policy before this traversal; the
        // capsule loads below use their fixed protocol ceiling, not these limits.
        state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
            .closure_limits()?;
        let child_capsule =
            load_capsule(authority, &retained.verifier.admitted_launch_capsule_hash)?;
        let child_sealed =
            crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
                &child_capsule,
            )?;
        if child_sealed.item_ref() != action.item_id
            || child_sealed.admitted_parameters_digest()? != canonical_value_digest(&action.params)?
        {
            bail!("qualification participant capsule differs from its current action");
        }
        let child = super::resolve_current_bundle_verifier_identity_against_admitted(
            state,
            authority,
            context,
            &action.item_id,
            &action.params,
            CurrentVerifierContext {
                content: CurrentVerifierContent::Inherited(&current.realizations),
                logical_project_root: retained.verifier.admitted_project_root.as_deref(),
                binding_subject_authority: Some(child_sealed.resolution_subject_authority()),
                sealed_request: Some(&child_sealed),
                project_context_resolver,
                pinned_admission: None,
            },
            None,
        )?;
        require_hookless_participant(&child.resolution)?;
        if child.artifact_identity != retained.verifier.artifact_identity
            || child.effective_definition_digest != retained.verifier.effective_definition_digest
        {
            bail!("qualification participant current execution identity changed");
        }
    }
    Ok(())
}
