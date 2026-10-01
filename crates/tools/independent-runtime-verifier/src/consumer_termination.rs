//! Product-edge agreement for an authenticated retained provider-death record.
//! This parser does not authenticate its caller or establish guest settlement.
//! The enclosing verifier must obtain the record from the protected callback;
//! qualification admission independently rejoins the retained journal.

use anyhow::{Result, ensure};
use ryeos_external_execution_contract::{
    restored_runtime_measurement::{RemoteVerificationPurpose, RestoredVerifierAttemptIntent},
    runtime_snapshot::{
        RuntimeSnapshotQualificationTerminalObservation,
        RuntimeSnapshotQualificationTerminationIntent,
    },
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalPhase {
    Terminal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalRecord {
    intent: RuntimeSnapshotQualificationTerminationIntent,
    phase: TerminalPhase,
    observation: RuntimeSnapshotQualificationTerminalObservation,
    created_at_ms: i64,
    updated_at_ms: i64,
}

/// Exact references for later daemon admission, not a transferable death grant.
pub struct ConsumerProviderTerminalReference {
    pub operation_id: String,
    pub observation_sha256: String,
}

pub fn check_provider_terminal_for_attempt(
    value: &serde_json::Value,
    attempt: &RestoredVerifierAttemptIntent,
    expected_owner: &str,
) -> Result<ConsumerProviderTerminalReference> {
    ensure!(
        serde_json::to_vec(value)?.len() <= 4096,
        "provider terminal record exceeds bound"
    );
    ensure!(
        matches!(
            &attempt.purpose,
            RemoteVerificationPurpose::ConsumerRuntime { .. }
        ) && attempt.operation_id == attempt.derived_operation_id()?,
        "provider terminal join requires an exact consumer attempt"
    );
    let retained: TerminalRecord = serde_json::from_value(value.clone())?;
    let TerminalPhase::Terminal = retained.phase;
    retained.observation.validate_for(&retained.intent)?;
    ensure!(
        retained.intent.schema == 1
            && retained.intent.operation_id == retained.intent.derived_operation_id()?
            && retained.intent.qualification_operation_id == attempt.qualification_operation_id
            && retained.intent.occurrence_id == attempt.restored_occurrence_id
            && retained.intent.owner_principal == expected_owner
            && retained.intent.attempt_deadline_ms > 0
            && !retained.observation.contact_deadline_exceeded
            && retained.created_at_ms > 0
            && retained.updated_at_ms >= retained.created_at_ms,
        "provider terminal record differs from exact consumer occurrence or owner"
    );
    Ok(ConsumerProviderTerminalReference {
        operation_id: retained.intent.operation_id,
        observation_sha256: lillux::sha256_hex(&ryeos_external_execution_contract::canonical_json(
            &retained.observation,
        )?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;
    use serde_json::json;

    fn fixture() -> (RestoredVerifierAttemptIntent, serde_json::Value) {
        let hash = "a".repeat(64);
        let mut attempt = RestoredVerifierAttemptIntent {
            schema: 2,
            operation_id: String::new(),
            qualification_operation_id: hash.clone(),
            restored_occurrence_id: "sbx-consumer".into(),
            verifier_artifact_hash: hash.clone(),
            upload_sha256: hash.clone(),
            upload_bytes: 1,
            attempt_deadline_ms: 100,
            purpose: RemoteVerificationPurpose::ConsumerRuntime {
                coordinate: ConsumerRuntimeVerificationCoordinate {
                    schema: 1,
                    accepted_root_id: "T-root".into(),
                    accepted_capsule_hash: hash.clone(),
                    qualification_purpose_digest: hash.clone(),
                    scenario_id: "consumer".into(),
                    scenario_source_digest: hash.clone(),
                    subject_digest: hash.clone(),
                    use_digest: hash.clone(),
                    prerequisite_measurement_attempt_id: hash.clone(),
                    prerequisite_measurement_observation_digest: hash.clone(),
                },
                nonce_hex: hash.clone(),
                guest_runtime_manifest_hash: hash.clone(),
            },
        };
        attempt.operation_id = attempt.derived_operation_id().unwrap();
        let mut intent = RuntimeSnapshotQualificationTerminationIntent {
            schema: 1,
            operation_id: String::new(),
            qualification_operation_id: hash.clone(),
            occurrence_id: attempt.restored_occurrence_id.clone(),
            owner_principal: "owner".into(),
            provider_id: "provider".into(),
            provider_spec_digest: hash.clone(),
            attempt_deadline_ms: 100,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let observation = RuntimeSnapshotQualificationTerminalObservation {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: intent.occurrence_id.clone(),
            provider_response_sha256: hash,
            terminated_at: "2026-10-01T00:00:00Z".into(),
            contact_deadline_exceeded: false,
        };
        (
            attempt,
            json!({"intent":intent, "phase":"terminal", "observation":observation, "created_at_ms":1, "updated_at_ms":2}),
        )
    }

    #[test]
    fn consumer_terminal_join_requires_exact_retained_death_not_pending_or_exit() {
        let (attempt, value) = fixture();
        let reference = check_provider_terminal_for_attempt(&value, &attempt, "owner").unwrap();
        assert_eq!(
            reference.operation_id,
            value["intent"]["operation_id"].as_str().unwrap()
        );
        assert_eq!(reference.observation_sha256.len(), 64);
        assert!(check_provider_terminal_for_attempt(&value, &attempt, "other-owner").is_err());
        for phase in ["reserved", "attempt_pending", "quarantined", "child_exited"] {
            let mut bad = value.clone();
            bad["phase"] = json!(phase);
            assert!(check_provider_terminal_for_attempt(&bad, &attempt, "owner").is_err());
        }
        for field in [
            "qualification_operation_id",
            "occurrence_id",
            "operation_id",
        ] {
            let mut bad = value.clone();
            bad["intent"][field] = json!("other");
            assert!(check_provider_terminal_for_attempt(&bad, &attempt, "owner").is_err());
        }
        let mut bad = value.clone();
        bad["observation"]["contact_deadline_exceeded"] = json!(true);
        assert!(check_provider_terminal_for_attempt(&bad, &attempt, "owner").is_err());
        bad = value.clone();
        bad["observation"] = serde_json::Value::Null;
        assert!(check_provider_terminal_for_attempt(&bad, &attempt, "owner").is_err());
        bad = value;
        bad["namespace_settled"] = json!(true);
        assert!(check_provider_terminal_for_attempt(&bad, &attempt, "owner").is_err());
    }
}
