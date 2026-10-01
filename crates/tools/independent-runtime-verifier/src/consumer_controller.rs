//! Product-edge interpretation of protected consumer observation responses.
//! This owns no contact journal or qualification authority. One explicit fresh
//! invocation requests START through the protected callback; recovery never
//! does. The caller obtains observations through that same protected surface.

use anyhow::{Result, ensure};
use lillux::time::MonotonicDeadline;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeChallenge, ConsumerRuntimeVerifierSelection,
    MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES, RestoredVerifierAttemptIntent,
    RestoredVerifierObservation,
};
use serde::Deserialize;

use crate::{consumer_protocol::ConsumerRetainedEvidence, consumer_record::ConsumerInputRecord};

/// Explicit invocation choice. Recovery cannot regenerate the preparation or
/// invoke START, even when exact observation reports absence.
pub enum ConsumerControllerInvocation {
    Fresh,
    Recover,
}

pub struct SettledConsumerObservation {
    pub challenge: ConsumerRuntimeChallenge,
    pub evidence_sha256: String,
    pub evidence: ConsumerRetainedEvidence,
    pub provider_terminal: crate::consumer_termination::ConsumerProviderTerminalReference,
}

pub enum ConsumerControllerOutcome {
    Unresolved(ConsumerControllerObservation),
    Settled(SettledConsumerObservation),
}

/// One bounded callback beat, not a second scheduler or contact owner. The
/// existing daemon journal alone reserves/contact/reconciles the attempt.
/// The enclosing admitted verifier retains this private directory across
/// recovery and supplies its original, non-renewable caller deadline.
pub async fn run_controller_beat(
    client: &ryeos_runtime::callback_uds::UdsRuntimeClient,
    private_directory: &lillux::PinnedDirectory,
    thread_id: &str,
    selection: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerVerificationInputSelection,
    invocation: ConsumerControllerInvocation,
    deadline: MonotonicDeadline,
) -> Result<ConsumerControllerOutcome> {
    use crate::consumer_record::RetainedControllerConsumerRecord;
    use anyhow::Context as _;
    ensure!(!deadline.has_elapsed(), "consumer controller beat expired");
    selection.validate()?;
    let qualification_operation_id = &selection.qualification_operation_id;
    let inputs = client
        .consumer_verification_inputs(thread_id, serde_json::to_value(selection)?, deadline)
        .await
        .context("protected consumer input read refused")?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer input read exceeded caller deadline"
    );
    let coordinate: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate =
        serde_json::from_value(inputs.get("coordinate").context("protected consumer coordinate absent")?.clone())?;
    selection.require_coordinate(&coordinate)?;
    ensure!(
        coordinate.accepted_root_id == thread_id,
        "protected consumer inputs name another invoking root"
    );
    let coordinate_value = serde_json::to_value(&coordinate)?;
    let retained = match invocation {
        ConsumerControllerInvocation::Fresh => {
            RetainedControllerConsumerRecord::prepare_fresh(private_directory, inputs, &coordinate)?
        }
        ConsumerControllerInvocation::Recover => {
            RetainedControllerConsumerRecord::reopen(private_directory, inputs, &coordinate)?
        }
    };
    ensure!(
        retained
            .record()
            .scripted_configuration()?
            .controller
            .inputs
            == *selection,
        "consumer invoking selectors differ from the retained signed policy"
    );
    if matches!(invocation, ConsumerControllerInvocation::Fresh) {
        let bytes = retained.bytes_for_contact()?;
        let record = std::str::from_utf8(bytes)?;
        ensure!(
            !deadline.has_elapsed(),
            "consumer preparation exceeded caller deadline"
        );
        match client
            .consumer_verification_start(
                &coordinate.accepted_root_id,
                qualification_operation_id,
                coordinate_value.clone(),
                record,
                deadline,
            )
            .await
        {
            Ok(_) => {}
            // Lost acknowledgment never authorizes byte replay or another
            // START. Only the exact protected observation below is allowed.
            Err(ryeos_runtime::callback::CallbackError::Transport(_)) => {}
            Err(error) => {
                return Err(anyhow::Error::new(error).context("consumer start refused; no retry"));
            }
        }
    }
    ensure!(
        !deadline.has_elapsed(),
        "consumer start/restore exceeded caller deadline"
    );
    retained.bytes_for_contact()?;
    let value = client
        .consumer_verification_observe(
            &coordinate.accepted_root_id,
            qualification_operation_id,
            coordinate_value.clone(),
            deadline,
        )
        .await
        .context("exact consumer observation unavailable; no relaunch")?;
    // An authenticated observed attempt has finished remote contact. Settle
    // its provider occurrence even if later product semantics refuse; provider
    // death does not repair a bad transcript or prove namespace settlement.
    let response: ObservationResponse = serde_json::from_value(value.clone())?;
    let terminal = if matches!(
        response,
        ObservationResponse::Retained {
            attempt: AttemptRecord {
                phase: AttemptPhase::Observed,
                ..
            },
            ..
        }
    ) {
        Some(
            client
                .consumer_verification_settle(
                    &coordinate.accepted_root_id,
                    qualification_operation_id,
                    coordinate_value,
                    deadline,
                )
                .await
                .context("consumer provider settlement remains uncertain")?,
        )
    } else {
        None
    };
    retained.bytes_for_contact()?;
    match check_observation_response(
        &value,
        retained.record(),
        qualification_operation_id,
        None,
        deadline,
    )? {
        ConsumerControllerObservation::Observed {
            challenge,
            evidence,
            evidence_sha256,
        } => {
            let provider_terminal =
                crate::consumer_termination::check_provider_terminal_for_attempt(
                    terminal
                        .as_ref()
                        .context("observed consumer lacks protected provider settlement")?,
                    &challenge.intent,
                    &retained.record().purpose.owner_fingerprint,
                )?;
            ensure!(
                !deadline.has_elapsed(),
                "consumer settlement join exceeded caller deadline"
            );
            Ok(ConsumerControllerOutcome::Settled(
                SettledConsumerObservation {
                    challenge,
                    evidence,
                    evidence_sha256,
                    provider_terminal,
                },
            ))
        }
        unresolved => Ok(ConsumerControllerOutcome::Unresolved(unresolved)),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AttemptPhase {
    Reserved,
    AttemptPending,
    Quarantined,
    Observed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttemptRecord {
    intent: RestoredVerifierAttemptIntent,
    consumer_selection: Option<ConsumerRuntimeVerifierSelection>,
    phase: AttemptPhase,
    observation: Option<RestoredVerifierObservation>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ObservationResponse {
    NotReserved {
        operation_id: String,
    },
    Retained {
        attempt: AttemptRecord,
        evidence: Option<serde_json::Value>,
    },
}

/// No variant permits a new start. Absence/pending require exact observation;
/// quarantine requires authoritative recovery, not product-side retries.
pub enum ConsumerControllerObservation {
    NotReserved {
        operation_id: String,
    },
    Pending {
        operation_id: String,
    },
    Quarantined {
        operation_id: String,
    },
    Observed {
        challenge: ConsumerRuntimeChallenge,
        evidence: ConsumerRetainedEvidence,
        evidence_sha256: String,
    },
}

pub fn check_observation_response(
    value: &serde_json::Value,
    original: &ConsumerInputRecord,
    qualification_operation_id: &str,
    expected_operation_id: Option<&str>,
    deadline: MonotonicDeadline,
) -> Result<ConsumerControllerObservation> {
    ensure!(
        !deadline.has_elapsed(),
        "consumer controller observation deadline expired"
    );
    original.validate()?;
    ensure!(
        lillux::valid_hash(qualification_operation_id),
        "consumer qualification identity is invalid"
    );
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() as u64 <= MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES + 64 * 1024,
        "consumer callback response exceeds bound"
    );
    let response: ObservationResponse = serde_json::from_slice(&bytes)?;
    let check_id = |operation_id: &str| -> Result<()> {
        ensure!(
            lillux::valid_hash(operation_id)
                && expected_operation_id.is_none_or(|expected| expected == operation_id),
            "consumer observation changed exact attempt identity"
        );
        Ok(())
    };
    let (attempt, evidence) = match response {
        ObservationResponse::NotReserved { operation_id } => {
            check_id(&operation_id)?;
            ensure!(
                !deadline.has_elapsed(),
                "consumer controller observation deadline expired"
            );
            return Ok(ConsumerControllerObservation::NotReserved { operation_id });
        }
        ObservationResponse::Retained { attempt, evidence } => (attempt, evidence),
    };
    check_id(&attempt.intent.operation_id)?;
    original.validate_attempt(&attempt.intent)?;
    ensure!(
        attempt.intent.qualification_operation_id == qualification_operation_id
            && attempt.intent.operation_id == attempt.intent.derived_operation_id()?
            && attempt.consumer_selection.as_ref() == Some(&original.selection)
            && attempt.created_at_ms > 0
            && attempt.updated_at_ms >= attempt.created_at_ms,
        "consumer retained callback changed original attempt inputs"
    );
    match attempt.phase {
        AttemptPhase::Reserved | AttemptPhase::AttemptPending | AttemptPhase::Quarantined => {
            ensure!(
                attempt.observation.is_none() && evidence.is_none(),
                "non-observed consumer attempt exposed terminal evidence"
            );
            let operation_id = attempt.intent.operation_id;
            ensure!(
                !deadline.has_elapsed(),
                "consumer controller observation deadline expired"
            );
            Ok(if matches!(attempt.phase, AttemptPhase::Quarantined) {
                ConsumerControllerObservation::Quarantined { operation_id }
            } else {
                ConsumerControllerObservation::Pending { operation_id }
            })
        }
        AttemptPhase::Observed => {
            let Some(RestoredVerifierObservation::ConsumerRuntime { observation }) =
                attempt.observation
            else {
                anyhow::bail!("observed consumer attempt lacks consumer evidence");
            };
            ensure!(
                !observation.contact_deadline_exceeded,
                "consumer contact exceeded its retained deadline"
            );
            let evidence =
                evidence.ok_or_else(|| anyhow::anyhow!("consumer evidence bytes are absent"))?;
            let bytes = ryeos_external_execution_contract::canonical_json(&evidence)?;
            let challenge = ConsumerRuntimeChallenge {
                schema: 1,
                intent: attempt.intent,
                selection: original.selection.clone(),
            };
            let evidence = ConsumerRetainedEvidence::parse_for_observation(
                &bytes,
                original,
                &challenge,
                &observation,
                deadline,
            )?;
            Ok(ConsumerControllerObservation::Observed {
                challenge,
                evidence,
                evidence_sha256: observation.evidence_sha256,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn observation_envelope_is_closed_and_has_no_retry_shape() {
        let value = json!({"status":"not_reserved", "operation_id":"a".repeat(64)});
        assert!(serde_json::from_value::<ObservationResponse>(value.clone()).is_ok());
        for field in ["retry", "start", "record", "evidence"] {
            let mut bad = value.clone();
            bad[field] = json!(true);
            assert!(serde_json::from_value::<ObservationResponse>(bad).is_err());
        }
        for status in ["retry", "complete", "completed", "unknown"] {
            let mut bad = value.clone();
            bad["status"] = json!(status);
            assert!(serde_json::from_value::<ObservationResponse>(bad).is_err());
        }
    }
}
