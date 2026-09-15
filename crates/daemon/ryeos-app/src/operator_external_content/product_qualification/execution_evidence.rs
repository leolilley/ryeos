//! Authenticate contract-projected execution evidence at exact coordinates.
//!
//! Signed handlers interpret their own runtime language. They do not authorize
//! child execution: the daemon corroborates every proposed occurrence against
//! its action intent, signed terminal, capsule and retained realization.

use anyhow::{Context as _, bail};
use ryeos_engine::resolution::{ResolutionOutput, TrustClass};
use ryeos_handler_protocol::{
    ExecutionEvidenceCandidateCallWire, ExecutionEvidenceDescribeRequest,
    ExecutionEvidenceDescribeResponse, ExecutionEvidenceEventWire, ExecutionEvidenceLimitsWire,
    ExecutionEvidenceProgramWire, ExecutionEvidenceProjectRequest,
    ExecutionEvidenceProjectResponse, ExecutionEvidenceRequiredCallWire,
    ExecutionEvidenceTerminalWire,
};
use ryeos_state::external_content::products::qualification::{
    ProductQualificationEvidence, ProductQualificationExecutionProof,
    ProductQualificationParticipant, ProductQualificationProjectorIdentity,
    ProductQualificationVerifier,
};
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

fn history(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    snapshot: &ThreadSnapshot,
    limits: ExecutionEvidenceLimitsWire,
) -> anyhow::Result<Vec<ThreadEvent>> {
    let mut events = Vec::new();
    let (_, observed) = authority
        .visit_thread_snapshot_events(
            &snapshot.chain_root_id,
            &snapshot.thread_id,
            u64::from(limits.max_events),
            u64::from(limits.max_request_bytes),
            guard,
            |_, event| events.push(event),
        )?
        .context("qualification exact signed thread history is unavailable")?;
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

#[allow(clippy::too_many_arguments)]
pub(super) fn prove(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    context: &HandlerContext,
    terminal: &ThreadSnapshot,
    capsule: &AdmittedLaunchCapsule,
    admitted: &ResolutionOutput,
    current: &CurrentBundleVerifierIdentity,
    subject_id: &str,
    subject_hash: &str,
) -> anyhow::Result<(Value, ProductQualificationExecutionProof)> {
    require_terminal_invocation(terminal, capsule)?;
    let (contract_ref, contract_digest) = projection_owner(&capsule.artifact_identity);
    let projector = state
        .engine
        .resolve_execution_evidence_projector(contract_ref, contract_digest)?
        .context("admitted execution contract declares no evidence projector")?;
    let effective_program = program(admitted, &current.effective_definition_digest)?;
    let required = described(state.engine.describe_execution_evidence(
        &projector,
        ExecutionEvidenceDescribeRequest {
            config: projector.declaration.config.clone(),
            effective_program: effective_program.clone(),
        },
    )?)?;
    let current_required = described(state.engine.describe_execution_evidence(
        &projector,
        ExecutionEvidenceDescribeRequest {
            config: projector.declaration.config.clone(),
            effective_program: program(&current.resolution, &current.effective_definition_digest)?,
        },
    )?)?;
    if required != current_required {
        bail!("current execution contract changed its required participants");
    }
    let events = history(authority, guard, terminal, projector.declaration.limits)?;
    let response = state.engine.project_execution_evidence(
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
    let (result, calls) = match response {
        ExecutionEvidenceProjectResponse::Projected { result, calls } => (result, calls),
        ExecutionEvidenceProjectResponse::Refused { message } => {
            bail!("execution evidence contract refused terminal: {message}")
        }
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
            guard,
            limits,
            context,
            terminal,
            &current.realizations,
            subject_id,
            subject_hash,
            required_call,
            call,
        )?);
    }
    // Callback-free subprocesses cannot hide daemon-dispatched participants.
    // A callback-capable contract must account for its calls through its own
    // projector; mere direct-vs-managed classification never proves leafness.
    if required.is_empty() {
        require_hookless_participant(admitted)?;
        require_hookless_participant(&current.resolution)?;
        let AdmittedLaunchArtifactIdentity::DirectItemExecutor { protocol_ref, .. } =
            &capsule.artifact_identity
        else {
            bail!("zero-participant qualification requires a callback-free subprocess contract");
        };
        let protocol = state.engine.protocols.require(protocol_ref)?;
        if protocol.descriptor.callback_channel
            != ryeos_engine::protocol_vocabulary::CallbackChannel::None
        {
            bail!("zero-participant qualification protocol can dispatch callbacks");
        }
    }
    Ok((
        result,
        ProductQualificationExecutionProof {
            projection_contract_ref: contract_ref.to_owned(),
            projection_contract_digest: contract_digest.to_owned(),
            projector: projector_identity(&projector.projector),
            participants,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn prove_participant(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    context: &HandlerContext,
    parent: &ThreadSnapshot,
    inherited: &ExternalContentRealizationSet,
    subject_id: &str,
    subject_hash: &str,
    required: &ExecutionEvidenceRequiredCallWire,
    call: &ExecutionEvidenceCandidateCallWire,
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
        || intent.incompatible_launch_metadata.is_some()
        || intent.initial_events.is_some()
        || intent.workspace_operation.is_some()
    {
        bail!("qualification occurrence contradicts its daemon-owned inline intent");
    }
    let child = state
        .state_store
        .get_authoritative_root_thread_snapshot(&intent.child_thread_id)?
        .context("qualification participant terminal is unavailable")?;
    let (child, continued, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_continuation_presence(
            &child.chain_root_id,
            &child.thread_id,
        )?
        .context("qualification participant exact terminal is unavailable")?;
    require_inline_child_root(&child, &intent.child_thread_id)?;
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
    let capsule = AdmittedLaunchCapsule::from_current_value(
        ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &authority.cas_store()?,
            capsule_hash,
            limits.max_object_bytes,
        )?,
    )?;
    let realization = capsule.verify_retained_execution_realization(
        &authority.cas_store()?,
        &authority.large_object_store()?,
        authority.trust_store(),
    )?;
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
        guard,
        context,
        &action.item_id,
        &action.params,
        CurrentVerifierContext {
            content: CurrentVerifierContent::Inherited(inherited),
            logical_project_root: logical_root.as_deref(),
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
    if state
        .engine
        .protocols
        .require(protocol_ref)?
        .descriptor
        .callback_channel
        != ryeos_engine::protocol_vocabulary::CallbackChannel::None
    {
        bail!("qualification participant protocol admits callbacks");
    }
    let _source =
        crate::source_closure_admission::recover_source_closure(state, &state.engine, resolution)?;
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
            result_digest: call.result_digest.clone(),
        },
    })
}

fn require_inline_child_root(
    child: &ThreadSnapshot,
    expected_child_id: &str,
) -> anyhow::Result<()> {
    // The daemon action intent above owns dispatch-parent linkage. An inline
    // dispatch creates an independent root; upstream_thread_id instead links
    // continuations and is forbidden by the existing root-creation contract.
    if child.thread_id != expected_child_id
        || child.chain_root_id != child.thread_id
        || child.upstream_thread_id.is_some()
    {
        bail!("qualification participant is not the exact inline child root");
    }
    Ok(())
}

/// Fresh consumption rechecks current handler and participant identities. It
/// does not reopen historical threads or reinterpret a retained attestation.
pub(in crate::operator_external_content) fn verify_current(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    context: &HandlerContext,
    evidence: &ProductQualificationEvidence,
    current: &CurrentBundleVerifierIdentity,
) -> anyhow::Result<()> {
    let proof = &evidence.execution_proof;
    let projector = state
        .engine
        .resolve_execution_evidence_projector(
            &proof.projection_contract_ref,
            &proof.projection_contract_digest,
        )?
        .context("current execution contract no longer supports qualification evidence")?;
    if projector_identity(&projector.projector) != proof.projector {
        bail!("qualification evidence projector identity changed");
    }
    let required = described(state.engine.describe_execution_evidence(
        &projector,
        ExecutionEvidenceDescribeRequest {
            config: projector.declaration.config.clone(),
            effective_program: program(&current.resolution, &current.effective_definition_digest)?,
        },
    )?)?;
    if required.len() != proof.participants.len() {
        bail!("qualification required participant set changed");
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
        let child = super::resolve_current_bundle_verifier_identity_against_admitted(
            state,
            authority,
            guard,
            context,
            &action.item_id,
            &action.params,
            CurrentVerifierContext {
                content: CurrentVerifierContent::Inherited(&current.realizations),
                logical_project_root: retained.verifier.admitted_project_root.as_deref(),
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
