//! Signed finite session setup and turn sends for a qualification-only
//! local Worker. The owner retains contact, Delta, pushed batches, Final, and
//! turn facts under the Q occurrence gate and checked Bundle generation.
//! These communication facts grant no qualification claim.

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;
use ryeos_state::objects::canonical_value_digest;
use serde_json::Value;

use crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot;
use crate::runtime_db::external_execution::{
    ExternalAllocationOwner, QualificationWorkerCommandPhase, QualificationWorkerCommandRecord,
    QualificationWorkerProcessPhase,
};
use crate::runtime_db::runtime_snapshot_qualification::SnapshotQualificationPhase;
use crate::state::AppState;

/// Exact request selected from the accepted policy and current signed Worker
/// protocol. Holding this snapshot does not authorize contact; the sender
/// reselects it inside its one-use journal gate.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedQualificationWorkerRequest {
    pub session_setup: Value,
    pub setup_digest: String,
    pub body: Value,
    pub digest: String,
    pub wire_max_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualificationWorkerCommandOutcome {
    FinalPendingTurn {
        request_digest: String,
        final_response_digest: String,
    },
    TurnCompleted {
        request_digest: String,
        final_response_digest: String,
        turn_id: String,
    },
}

/// Supply the selected signed scenario's bounded body and its canonical
/// digest to one synchronous caller. The callback remains inside the Q
/// occurrence contact gate and checked Bundle generation, but must separately
/// obtain its one-use command journal authority before any Worker send.
pub fn with_signed_qualification_worker_request<T>(
    state: &AppState,
    placement: &str,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    binding_digest: &str,
    root: &PreparedExternalQualificationRoot,
    run: impl FnOnce(SignedQualificationWorkerRequest) -> Result<T>,
) -> Result<T> {
    crate::external_qualification_local::with_qualification_local_owner(
        state,
        placement,
        coordinate,
        binding_digest,
        root,
        |_| {
            state.engine.with_checked_bundle_generation(|generation| {
                let allocation = state
                    .state_store
                    .external_allocation(placement)?
                    .context("Q Worker request lost its allocation")?;
                let ExternalAllocationOwner::QualificationCandidate(owner) =
                    &allocation.reservation.owner
                else {
                    bail!("Q Worker request has no qualification owner");
                };
                ensure!(
                    owner.coordinate == *coordinate && owner.program == *root.program(),
                    "Q Worker request changed its accepted owner"
                );
                let qualification = state
                    .state_store
                    .snapshot_qualification_operation(&owner.qualification_operation_id)?
                    .context("Q Worker request lost its restored qualification")?;
                ensure!(
                    qualification.intent.operation_id == owner.qualification_operation_id
                        && qualification.intent.owner_principal == owner.operator_principal
                        && qualification.phase == SnapshotQualificationPhase::OccurrenceBound
                        && qualification.occurrence.as_ref()
                            == Some(&owner.qualification_occurrence),
                    "Q Worker request changed its restored occurrence"
                );
                let profile = state
                    .node_config
                    .runtime_snapshot_qualification
                    .iter()
                    .find(|binding| {
                        binding.digest() == qualification.intent.qualification_profile_digest
                    })
                    .context("Q Worker request lost its signed qualification profile")?;
                ensure!(
                    qualification.intent.verifier_artifact_hash == profile.verifier_artifact_hash(),
                    "Q Worker request changed its signed qualification verifier"
                );
                let selection = profile.consumer_verifier(coordinate)?;
                let purpose = root.purpose();
                purpose.validate_remote_consumer_coordinate(coordinate, selection)?;
                let source = purpose
                    .remote_verifier_sources
                    .get(&coordinate.scenario_id)
                    .context("Q Worker request has no accepted selected verifier source")?;
                ensure!(
                    source.scenario_source_digest == selection.scenario_source_digest
                        && source.executor.payload_blob_hash == selection.verifier_artifact_hash,
                    "Q Worker request differs from its selected signed verifier source"
                );
                // This rechecks the current policy and all Worker definitions
                // against the accepted root in this same checked generation.
                let resolution = root.current_local_worker_resolution(generation)?;
                let execution = state
                    .engine
                    .kinds
                    .get("worker")
                    .and_then(|schema| schema.execution.as_ref())
                    .context("Q Worker request has no signed execution schema")?;
                let protocol_ref = match execution
                    .terminator
                    .as_ref()
                    .context("Q Worker request has no signed terminator")?
                {
                    ryeos_engine::kind_registry::TerminatorDecl::Subprocess { protocol } => {
                        protocol
                            .resolve(&resolution.composed.composed)
                            .map_err(anyhow::Error::msg)?
                    }
                    ryeos_engine::kind_registry::TerminatorDecl::InProcess { .. } => {
                        bail!("Q Worker request has no subprocess protocol")
                    }
                };
                let verified_protocol = state.engine.protocols.require(&protocol_ref)?;
                let session = ryeos_engine::protocols::validate_persistent_session_protocol(
                    &verified_protocol.descriptor,
                )
                .map_err(anyhow::Error::msg)?;
                let scenario = purpose
                    .policy_source
                    .policy
                    .producer_scenarios
                    .get(&coordinate.scenario_id)
                    .context("Q Worker request has no selected signed scenario")?;
                ensure!(
                    scenario.remote_verifier.as_ref() == Some(&source.selection),
                    "Q Worker request changed its signed scenario verifier"
                );
                let request = scenario
                    .consumer_worker_request
                    .as_ref()
                    .context("selected Q scenario has no signed Worker request")?;
                request.validate()?;
                let session_setup = request.session_setup.clone();
                let setup_digest = canonical_value_digest(&session_setup)?;
                let body = request.body.clone();
                let digest = canonical_value_digest(&body)?;
                run(SignedQualificationWorkerRequest {
                    session_setup,
                    setup_digest,
                    body,
                    digest,
                    wire_max_bytes: session.max_frame_bytes,
                })
            })
        },
    )
}

pub fn select_signed_qualification_worker_request(
    state: &AppState,
    placement: &str,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    binding_digest: &str,
    root: &PreparedExternalQualificationRoot,
) -> Result<SignedQualificationWorkerRequest> {
    with_signed_qualification_worker_request(state, placement, coordinate, binding_digest, root, Ok)
}

fn command_outcome(
    record: &QualificationWorkerCommandRecord,
) -> Result<Option<QualificationWorkerCommandOutcome>> {
    if record.phase == QualificationWorkerCommandPhase::OutcomeUnknown {
        bail!("Q Worker command has an unknown contacted outcome");
    }
    if record.setup_owner_send_settled_at_ms.is_none() || record.owner_send_settled_at_ms.is_none()
    {
        return Ok(None);
    }
    let Some(final_response_digest) = &record.final_response_digest else {
        return Ok(None);
    };
    Ok(Some(
        if record.phase == QualificationWorkerCommandPhase::TurnCompleted {
            QualificationWorkerCommandOutcome::TurnCompleted {
                request_digest: record.request_digest.clone(),
                final_response_digest: final_response_digest.clone(),
                turn_id: record
                    .turn_id
                    .clone()
                    .context("Q completed turn lost its exact id")?,
            }
        } else {
            QualificationWorkerCommandOutcome::FinalPendingTurn {
                request_digest: record.request_digest.clone(),
                final_response_digest: final_response_digest.clone(),
            }
        },
    ))
}

/// Read only the exact retained Final/turn join for a bounded executor wait.
/// A pushed batch can have been emitted before Request and queued in the pool
/// reader until after PossibleContact; its State alone proves no Request
/// causality. Neither variant is a semantic result or qualification claim.
pub fn read_qualification_worker_command_outcome(
    state: &AppState,
    placement: &str,
    expected_request_digest: &str,
) -> Result<Option<QualificationWorkerCommandOutcome>> {
    let record = state
        .state_store
        .qualification_worker_command(placement)?
        .context("Q Worker command has no retained request")?;
    ensure!(
        record.request_digest == expected_request_digest,
        "Q Worker command changed its signed request"
    );
    command_outcome(&record)
}

fn persist_state_observations(
    state: &AppState,
    placement: &str,
    request_digest: &str,
    body: &Value,
    maximum: usize,
) -> Result<()> {
    let Some(observations) = body.get("session_observations") else {
        return Ok(());
    };
    let observations = observations
        .as_array()
        .context("Q Worker session observations are not an array")?;
    ensure!(
        observations.len() <= maximum,
        "Q Worker session observations exceed one event bound"
    );
    for observation in observations {
        if observation.get("kind").and_then(Value::as_str) == Some("state") {
            state
                .state_store
                .record_qualification_worker_turn_observation(
                    placement,
                    request_digest,
                    observation,
                )?;
        }
    }
    Ok(())
}

/// The reader's pushed ObservationBatch arrives on a different thread while
/// the sender holds the occurrence gate. Commit its complete content, then
/// each State transition, before returning the transport ACK. Exact replay
/// after a committed batch but lost ACK repeats these idempotent writes. The
/// command phase proves receipt order, not when the Worker emitted the batch.
pub fn ingest_qualification_worker_observation_batch(
    state: &AppState,
    placement: &str,
    request_digest: &str,
    wire_max_bytes: u32,
    body: Value,
) -> Result<Value> {
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let ack = state
        .state_store
        .record_qualification_worker_observation_batch(
            placement,
            request_digest,
            &body,
            wire_max_bytes,
        )?;
    persist_state_observations(state, placement, request_digest, &body, 128 * 16)?;
    Ok(serde_json::json!({
        "through_sequence": ack.through_sequence,
        "batch_digest": ack.batch_digest,
    }))
}

fn persist_request_delta(
    state: &AppState,
    placement: &str,
    request_digest: &str,
    wire_max_bytes: u32,
    delta: Value,
) -> Result<Option<Value>> {
    let progress = state
        .state_store
        .record_qualification_worker_progress_delta(
            placement,
            request_digest,
            &delta,
            wire_max_bytes,
        )?;
    let observations = delta
        .get("session_observations")
        .and_then(Value::as_array)
        .context("Q Worker command progress has no observations array")?;
    ensure!(
        observations.len() == 1
            && observations[0].get("kind").and_then(Value::as_str) == Some("state"),
        "Q Worker command Delta is not one turn transition"
    );
    state
        .state_store
        .record_qualification_worker_turn_observation(
            placement,
            request_digest,
            &observations[0],
        )?;
    Ok(Some(serde_json::json!({
        "command_progress_digest": progress
            .delta_response_digest
            .context("Q Worker Delta lost its retained digest before ACK")?,
    })))
}

/// Execute the signed setup and turn inside the restored occurrence gate and
/// one checked Bundle generation. Each PossibleContact is durable before its
/// exclusive-pool send, and turn contact requires settled setup. The callback has no authority to select bytes
/// or extend the retained channel deadline. This returns communication facts
/// only; semantic completion and qualification remain separate.
pub fn execute_signed_qualification_worker_request(
    state: &AppState,
    placement: &str,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    binding_digest: &str,
    root: &PreparedExternalQualificationRoot,
    expected_request: &SignedQualificationWorkerRequest,
    expected_process: &crate::process::ExecutionProcessIdentity,
    mut send_once: impl FnMut(
        Value,
        lillux::time::MonotonicDeadline,
        &mut dyn FnMut(Value) -> Result<Option<Value>>,
    ) -> Result<Value>,
) -> Result<QualificationWorkerCommandOutcome> {
    with_signed_qualification_worker_request(
        state,
        placement,
        coordinate,
        binding_digest,
        root,
        |selected| {
            ensure!(
                selected == *expected_request,
                "Q Worker request changed after prestart signed selection"
            );
            let process = state
                .state_store
                .qualification_worker_process(placement)?
                .context("Q Worker command has no exact attached process")?;
            ensure!(
                process.phase == QualificationWorkerProcessPhase::Attached
                    && process.process_identity == *expected_process,
                "Q Worker command changed its held-start process identity"
            );
            let channel = state.state_store.external_execution_channel(placement)?;
            ensure!(
                channel.digest()? == binding_digest,
                "Q Worker command changed its released channel"
            );
            let now = lillux::time::timestamp_millis();
            let remaining = channel
                .execution_deadline_ms
                .checked_sub(now)
                .filter(|remaining| *remaining > 0)
                .context("Q Worker command passed its retained channel deadline")?;
            let deadline = root.cap_execution_deadline(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining)?),
            ));
            state.persistent_sessions.ensure_exclusive_request_fits(
                placement,
                &selected.body,
                selected.wire_max_bytes,
            )?;
            state.persistent_sessions.ensure_exclusive_request_fits(
                placement,
                &selected.session_setup,
                selected.wire_max_bytes,
            )?;
            let reserved = state.state_store.reserve_qualification_worker_command(
                placement,
                &selected.setup_digest,
                &selected.digest,
            )?;
            ensure!(
                reserved.phase == QualificationWorkerCommandPhase::Reserved
                    && reserved.process_incarnation_digest
                        == expected_process.owner_incarnation_digest()?,
                "Q Worker command was already contacted or changed process"
            );
            // Each stage consumes fresh admission at its own durable contact
            // boundary. Setup has no replay or replacement policy; a lost Final
            // fences the conversation rather than granting turn contact.
            let admission = root.authenticate_invocation(state)?;
            state
                .state_store
                .mark_qualification_worker_setup_possible_contact(
                    state,
                    placement,
                    &selected.setup_digest,
                    admission,
                )?;
            let setup = (|| -> Result<()> {
                ensure!(
                    !deadline.has_elapsed(),
                    "Q session setup expired before send"
                );
                let mut reject_delta = |_delta| -> Result<Option<Value>> {
                    bail!("Q session setup cannot emit a turn progress Delta")
                };
                let final_body =
                    send_once(selected.session_setup.clone(), deadline, &mut reject_delta)?;
                validate_setup_final(&final_body, selected.wire_max_bytes)?;
                state.state_store.record_qualification_worker_setup_final(
                    placement,
                    &selected.setup_digest,
                    &final_body,
                    selected.wire_max_bytes,
                )?;
                state
                    .state_store
                    .mark_qualification_worker_setup_send_settled_success(
                        placement,
                        &selected.setup_digest,
                    )?;
                Ok(())
            })();
            if let Err(error) = setup {
                return Err(fence_unknown_command(state, placement, error));
            }
            // Successful setup does not authorize a turn after revocation or
            // expiry. This independent gate verifies setup settlement too.
            let admission = root.authenticate_invocation(state)?;
            state
                .state_store
                .mark_qualification_worker_command_possible_contact(
                    state,
                    placement,
                    &selected.digest,
                    admission,
                )?;
            let digest = selected.digest.clone();
            let wire_max_bytes = selected.wire_max_bytes;
            let mut on_delta =
                |delta| persist_request_delta(state, placement, &digest, wire_max_bytes, delta);
            let contacted = (|| -> Result<QualificationWorkerCommandOutcome> {
                ensure!(
                    !deadline.has_elapsed(),
                    "Q Worker command expired before its one permitted send"
                );
                let final_body = send_once(selected.body, deadline, &mut on_delta)?;
                ensure!(
                    lillux::canonical_json(&final_body)?.len() <= wire_max_bytes as usize,
                    "Q Worker Final exceeds its selected signed wire maximum"
                );
                persist_state_observations(state, placement, &digest, &final_body, 16)?;
                state.state_store.record_qualification_worker_final(
                    placement,
                    &digest,
                    &final_body,
                    wire_max_bytes,
                )?;
                let record = state
                    .state_store
                    .mark_qualification_worker_send_settled_success(placement, &digest)?;
                command_outcome(&record)?.context("Q Worker Final was not retained")
            })();
            match contacted {
                Ok(outcome) => Ok(outcome),
                Err(error) => Err(fence_unknown_command(state, placement, error)),
            }
        },
    )
}

/// The structured bridge owns session binding. Require its exact bounded
/// common binding observation; never extract Codex-specific response pointers
/// or interpolate an upstream ID in the daemon's turn payload.
fn validate_setup_final(body: &Value, wire_max_bytes: u32) -> Result<()> {
    ensure!(
        lillux::canonical_json(body)?.len() <= wire_max_bytes as usize,
        "Q session setup Final exceeds its signed wire maximum"
    );
    let response = body
        .get("response")
        .and_then(Value::as_object)
        .context("Q session setup Final has no structured response")?;
    ensure!(
        !response.contains_key("error"),
        "Q session setup returned an error response"
    );
    let observations = body
        .get("session_observations")
        .and_then(Value::as_array)
        .context("Q session setup Final has no binding observations")?;
    ensure!(
        observations.len() <= 16,
        "Q session setup observations exceed bound"
    );
    let mut bindings = observations.iter().filter(|observation| {
        observation.get("kind").and_then(Value::as_str) == Some("remote_thread")
    });
    let binding = bindings
        .next()
        .context("Q session setup did not bind an upstream session")?;
    ensure!(
        bindings.next().is_none(),
        "Q session setup bound multiple upstream sessions"
    );
    let id = binding
        .get("id")
        .and_then(Value::as_str)
        .context("Q session setup bound no textual upstream session")?;
    ensure!(
        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
        "Q session setup bound an invalid upstream session"
    );
    ensure!(
        observations.iter().all(|observation| {
            observation.get("kind").and_then(Value::as_str) != Some("state")
        }),
        "Q session setup cannot supply turn state testimony"
    );
    Ok(())
}

fn fence_unknown_command(state: &AppState, placement: &str, error: anyhow::Error) -> anyhow::Error {
    match state
        .state_store
        .mark_qualification_worker_command_outcome_unknown(placement)
    {
        Ok(_) => error,
        Err(fence) => error.context(format!(
            "Q Worker command unknown-outcome fence failed: {fence:#}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{QualificationWorkerCommandOutcome, command_outcome, validate_setup_final};
    use crate::runtime_db::external_execution::{
        QualificationWorkerCommandPhase, QualificationWorkerCommandRecord,
    };

    #[test]
    fn setup_requires_single_bounded_binding_without_turn_testimony() {
        let good = serde_json::json!({"response":{"result":{}},"session_observations":[{"kind":"remote_thread","id":"upstream"}]});
        validate_setup_final(&good, 4096).unwrap();
        for observations in [
            serde_json::json!([]),
            serde_json::json!([{"kind":"remote_thread","id":""}]),
            serde_json::json!([{"kind":"remote_thread","id":"one"},{"kind":"remote_thread","id":"two"}]),
            serde_json::json!([{"kind":"remote_thread","id":"one"},{"kind":"state","next":"turn_running"}]),
        ] {
            assert!(
                validate_setup_final(
                    &serde_json::json!({"response":{"result":{}},"session_observations":observations}),
                    4096
                )
                .is_err()
            );
        }
        assert!(validate_setup_final(&good, 1).is_err());
        let mut error = good.clone();
        error["response"]["error"] = serde_json::json!({"code":-1});
        assert!(validate_setup_final(&error, 4096).is_err());
    }

    #[test]
    fn completed_observations_require_owner_send_settlement() {
        let mut record = QualificationWorkerCommandRecord {
            placement_thread_id: "Q-test".into(),
            accepted_root_id: "root".into(),
            accepted_capsule_hash: "hash".into(),
            qualification_operation_id: "operation".into(),
            qualification_occurrence_id: "occurrence".into(),
            channel_binding_digest: "channel".into(),
            process_incarnation_digest: "process".into(),
            setup_request_digest: "setup-request".into(),
            setup_contacted_at_ms: Some(1),
            setup_final_response_digest: Some("setup-final".into()),
            setup_final_response: None,
            setup_final_wire_max_bytes: None,
            setup_owner_send_settled_at_ms: Some(2),
            request_digest: "request".into(),
            phase: QualificationWorkerCommandPhase::TurnCompleted,
            turn_id: Some("turn-one".into()),
            turn_started_at_ms: Some(3),
            turn_start_digest: Some("start".into()),
            turn_completed_at_ms: Some(4),
            turn_completion_digest: Some("complete".into()),
            delta_response_digest: None,
            delta_response: None,
            delta_wire_max_bytes: None,
            final_response_digest: Some("final".into()),
            final_response: None,
            final_wire_max_bytes: None,
            reserved_at_ms: 1,
            contacted_at_ms: Some(2),
            completed_at_ms: Some(5),
            owner_send_settled_at_ms: None,
        };
        assert_eq!(command_outcome(&record).unwrap(), None);
        record.owner_send_settled_at_ms = Some(6);
        record.setup_owner_send_settled_at_ms = None;
        assert_eq!(command_outcome(&record).unwrap(), None);
        record.setup_owner_send_settled_at_ms = Some(2);
        assert!(matches!(
            command_outcome(&record).unwrap(),
            Some(QualificationWorkerCommandOutcome::TurnCompleted { .. })
        ));
        record.phase = QualificationWorkerCommandPhase::OutcomeUnknown;
        assert!(command_outcome(&record).is_err());
    }
}
