//! Finite consumer claims proposed by the independent verifier.
//!
//! `independent_candidate_diagnostic` means the separate restored diagnostic
//! passed its complete scripted protocol, canary, patch, inner/outer native
//! observation and provider-terminal checks. It is not a denial observation
//! from the production Q Worker.
//! `qualification_worker_command` means the actual Q setup/turn transcript
//! passed the signed finite command, output and exit-status expectations.
//! Neither claim attests this enclosing verifier's own process settlement;
//! the issuer must independently corroborate that after successful exit.

use anyhow::{Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge;
use ryeos_state::external_content::products::qualification::{
    PRODUCT_QUALIFICATION_RESULT_SCHEMA, ProductQualificationResult,
};

use crate::consumer_record::ConsumerInputRecord;
use crate::consumer_termination::ConsumerProviderTerminalReference;
use crate::consumer_worker_transcript::VerifiedConsumerWorkerTranscript;

const SUPPORTED_CLAIMS: [&str; 2] = [
    "independent_candidate_diagnostic",
    "qualification_worker_command",
];

/// Policy selects only implemented meanings; it cannot invent a successful
/// probe by naming a claim. Both probes remain mandatory in the controller.
pub(crate) fn selected_claims(allowed: &[String], required: &[String]) -> Result<Vec<String>> {
    ensure!(
        !allowed.is_empty()
            && allowed.windows(2).all(|pair| pair[0] < pair[1])
            && required.windows(2).all(|pair| pair[0] < pair[1]),
        "consumer claims must be a nonempty allowance and sorted unique sets"
    );
    ensure!(
        allowed
            .iter()
            .chain(required)
            .all(|claim| SUPPORTED_CLAIMS.contains(&claim.as_str())),
        "consumer policy requests an unsupported claim"
    );
    ensure!(
        required
            .iter()
            .all(|claim| allowed.binary_search(claim).is_ok()),
        "consumer policy omits a required claim from its allowance"
    );
    Ok(SUPPORTED_CLAIMS
        .into_iter()
        .filter(|claim| allowed.iter().any(|allowed| allowed.as_str() == *claim))
        .map(str::to_owned)
        .collect())
}

/// Called only after both semantic checks and protected provider settlement.
/// Exact references remain proposals until the issuer joins its own journals.
pub(crate) fn checked_result(
    record: &ConsumerInputRecord,
    challenge: &ConsumerRuntimeChallenge,
    evidence_sha256: &str,
    terminal: &ConsumerProviderTerminalReference,
    transcript: &VerifiedConsumerWorkerTranscript,
) -> Result<ProductQualificationResult> {
    record.validate_attempt(&challenge.intent)?;
    challenge.validate()?;
    ensure!(
        record.selection == challenge.selection,
        "consumer result changed verifier selection"
    );
    crate::consumer_worker_transcript::validate_reference_for_record(
        &transcript.reference,
        record,
    )?;
    let claims = selected_claims(
        &record.purpose.policy_source.policy.allowed_claims,
        &record.purpose.required_claims,
    )?;
    for hash in [
        challenge.intent.operation_id.as_str(),
        evidence_sha256,
        terminal.operation_id.as_str(),
        terminal.observation_sha256.as_str(),
    ] {
        ensure!(
            lillux::valid_hash(hash),
            "consumer result reference is not a canonical hash"
        );
    }
    let result = ProductQualificationResult {
        schema: PRODUCT_QUALIFICATION_RESULT_SCHEMA.to_owned(),
        subject_manifest_hash: record.purpose.subject_manifest_hash.clone(),
        claims,
        probe_evidence: serde_json::json!({
            "remote_consumer_attempt": {
                "operation_id": challenge.intent.operation_id,
                "evidence_sha256": evidence_sha256,
                "termination_operation_id": terminal.operation_id,
                "terminal_observation_sha256": terminal.observation_sha256,
            },
            "qualification_worker_transcript_manifest_hash": transcript.reference.manifest_hash,
        }),
    };
    result.validate()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn supported_policy_can_select_only_a_narrower_claim() {
        for values in [
            &SUPPORTED_CLAIMS[..],
            &SUPPORTED_CLAIMS[..1],
            &SUPPORTED_CLAIMS[1..],
        ] {
            let allowed = claims(values);
            assert_eq!(selected_claims(&allowed, &allowed).unwrap(), allowed);
        }
    }

    #[test]
    fn policy_cannot_turn_diagnostic_checks_into_production_exclusion() {
        for unsupported in [
            "controller_credential_exclusion",
            "native_writer_exclusion",
            "future_claim",
        ] {
            assert!(selected_claims(&claims(&[unsupported]), &[]).is_err());
            assert!(selected_claims(&claims(&SUPPORTED_CLAIMS), &claims(&[unsupported])).is_err());
        }
    }

    #[test]
    fn missing_allowance_or_noncanonical_sets_refuse() {
        assert!(selected_claims(&[], &[]).is_err());
        assert!(
            selected_claims(
                &claims(&SUPPORTED_CLAIMS[..1]),
                &claims(&SUPPORTED_CLAIMS[1..])
            )
            .is_err()
        );
        assert!(
            selected_claims(&claims(&[SUPPORTED_CLAIMS[1], SUPPORTED_CLAIMS[0]]), &[]).is_err()
        );
        assert!(
            selected_claims(&claims(&[SUPPORTED_CLAIMS[0], SUPPORTED_CLAIMS[0]]), &[]).is_err()
        );
        assert!(
            selected_claims(
                &claims(&SUPPORTED_CLAIMS),
                &claims(&[SUPPORTED_CLAIMS[0], SUPPORTED_CLAIMS[0]])
            )
            .is_err()
        );
    }
}
