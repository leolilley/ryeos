//! Finite consumer claims proposed by the independent verifier.
//!
//! The five ordinary claims combine the actual full Q observations with the
//! finite-v2 audited source contract for these exact admitted inputs. They are
//! not aliases for the independent diagnostic or claims inferred from names.
//! `bounded_candidate_capture` combines Q's observed changed, bounded one-file
//! native capture with the audited ordinary capture path's bounded traversal,
//! input-anchor/exclusion checks and immutable snapshot retention. It does not
//! assert that this Q occurrence exported an ordinary candidate project.
//! `candidate_only_execution` means candidate effects use the selected external
//! environment. The contract's enumerated trusted controller services and
//! bounded CodeMode computation remain allowed; this is not a claim that all
//! computation occurs remotely.
//! `controller_credential_exclusion` combines the original positive canary,
//! actual same-Q denial and terminal recheck with the audited original HOME,
//! descriptor, mount, source and environment separation. It does not mean the
//! trusted controller lacks its own model credentials.
//! `native_writer_exclusion` requires the exact namespace's writer death before
//! capture and retention, together with the audited writable-alias ownership.
//! Provider termination alone does not prove this ordering or writer death.
//! `no_local_execution_fallback` combines the correlated same-Q local refusal
//! with the audited effect-family routing across the closed ordinary language.
//! Recovery preserves its original admitted capsule; this does not upgrade a
//! predecessor thread or broader profile into the finite-v2 contract.
//!
//! `independent_candidate_diagnostic` means the separate restored diagnostic
//! passed its complete scripted protocol, canary, patch, inner/outer native
//! observation and provider-terminal checks. It is not a denial observation
//! from the production Q Worker.
//! `qualification_worker_command` means the actual Q setup/turn transcript
//! passed the signed finite command, output and exit-status expectations.
//! No claim attests this enclosing verifier's own process settlement;
//! the issuer must independently corroborate that after successful exit.

use anyhow::{Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge;
use ryeos_state::external_content::products::qualification::{
    PRODUCT_QUALIFICATION_RESULT_SCHEMA, ProductQualificationResult,
};

use crate::consumer_record::ConsumerInputRecord;
use crate::consumer_termination::ConsumerProviderTerminalReference;
use crate::consumer_worker_transcript::VerifiedConsumerWorkerTranscript;

const SUPPORTED_CLAIMS: [&str; 7] = [
    "bounded_candidate_capture",
    "candidate_only_execution",
    "controller_credential_exclusion",
    "independent_candidate_diagnostic",
    "native_writer_exclusion",
    "no_local_execution_fallback",
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
    checked_q: &crate::consumer_worker_semantics::VerifiedQConversation,
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
    checked_q.require_for(record, transcript)?;
    // Re-establish the original finite-v2 source/helper pins and the exact
    // admitted use at the result boundary. scripted_configuration requires
    // qualification_use and compares it with the signed verifier parameters;
    // neither a policy name nor a diagnostic-only success can supply it.
    record.product_contract()?;
    record.scripted_configuration()?;
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
    fn policy_cannot_select_unaudited_or_stronger_meanings() {
        for unsupported in [
            "ordinary_candidate_export_observed",
            "all_computation_is_remote",
            "provider_terminal_proves_writer_death",
            "future_claim",
        ] {
            assert!(selected_claims(&claims(&[unsupported]), &[]).is_err());
            assert!(selected_claims(&claims(&SUPPORTED_CLAIMS), &claims(&[unsupported])).is_err());
        }
    }

    #[test]
    fn ordinary_requirements_are_supported_only_when_each_is_allowed() {
        let ordinary = claims(ryeos_state::external_execution::admission::REQUIRED_CLAIMS);
        assert_eq!(selected_claims(&ordinary, &ordinary).unwrap(), ordinary);
        for missing in 0..ordinary.len() {
            let mut allowed = ordinary.clone();
            allowed.remove(missing);
            assert!(selected_claims(&allowed, &ordinary).is_err());
        }
        let diagnostic = claims(&[
            "independent_candidate_diagnostic",
            "qualification_worker_command",
        ]);
        assert!(selected_claims(&diagnostic, &ordinary).is_err());
        assert_eq!(
            selected_claims(&diagnostic, &diagnostic).unwrap(),
            diagnostic
        );
        // A wider implemented vocabulary never adds unallowed claims. Both
        // underlying probes remain mandatory regardless of this selection.
        for name in &ordinary {
            let allowed = vec![name.clone()];
            assert_eq!(selected_claims(&allowed, &[]).unwrap(), allowed);
        }
        assert_eq!(
            selected_claims(&claims(&SUPPORTED_CLAIMS), &ordinary).unwrap(),
            claims(&SUPPORTED_CLAIMS)
        );
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
