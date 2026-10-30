//! Typed proof that one launch is a remote-consumer qualification verifier.
//!
//! The executor's launch gates refuse the qualification callback protocol for
//! every caller. A gate admits that route only when it holds this proof and the
//! proof names the very item whose protocol is being checked. The proof is a
//! necessary condition only: the runner still demands the born-root custody
//! (owner-bound planning row, authoritative root snapshot and the minted
//! callback grant) before any callback authority exists.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerVerificationInputSelection;
use ryeos_state::external_content::products::qualification::ProductQualificationPolicySource;
use ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose;

use crate::operator_external_content::product_qualification::launch::require_invocation_lane;
use crate::operator_external_content::qualification_launch::PreparedQualificationLaunch;
use crate::thread_lifecycle::RootExecutionAdmission;

/// The remote scenario a launch will consume and the verifier item it runs.
///
/// Fields are private and there is no `Default` or deserializer, so a value
/// exists only when a prepared launch or a sealed admission has been checked
/// against the signed policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConsumerLaunchProof {
    scenario_id: String,
    verifier_ref: String,
}

impl RemoteConsumerLaunchProof {
    /// Derive the prospective proof from an authenticated, prepared launch.
    /// `Ok(None)` when the launch carries no consumer invocation.
    pub fn from_prepared(prepared: &PreparedQualificationLaunch) -> Result<Option<Self>> {
        let policy = prepared.policy_source();
        Self::from_parts(
            policy,
            prepared.consumer_invocation(),
            &policy.policy.verifier_ref,
        )
    }

    /// Re-derive the proof from the purpose sealed on an admitted root.
    /// `Ok(None)` when the admission is not a qualification or carries no
    /// consumer invocation.
    pub fn from_admission(admission: &RootExecutionAdmission) -> Result<Option<Self>> {
        admission
            .qualification_purpose()
            .map(Self::from_purpose)
            .transpose()
            .map(Option::flatten)
    }

    fn from_purpose(purpose: &QualificationLaunchPurpose) -> Result<Option<Self>> {
        purpose.validate()?;
        let proof = Self::from_parts(
            &purpose.policy_source,
            purpose.consumer_invocation.as_ref(),
            &purpose.verifier_ref,
        )?;
        let Some(proof) = proof else {
            return Ok(None);
        };
        ensure!(
            purpose
                .remote_verifier_sources
                .contains_key(&proof.scenario_id),
            "sealed qualification purpose has no verifier source for the invoked scenario"
        );
        purpose.remote_use(&proof.scenario_id)?;
        Ok(Some(proof))
    }

    fn from_parts(
        policy: &ProductQualificationPolicySource,
        invocation: Option<&ConsumerVerificationInputSelection>,
        verifier_ref: &str,
    ) -> Result<Option<Self>> {
        let Some(invocation) = invocation else {
            return Ok(None);
        };
        require_invocation_lane(policy, Some(invocation))?;
        let scenario = policy
            .policy
            .producer_scenarios
            .get(&invocation.scenario_id)
            .context("consumer invocation scenario is not signed in policy")?;
        ensure!(
            scenario.remote_verifier.is_some(),
            "consumer invocation scenario is not remote"
        );
        ensure!(
            verifier_ref == policy.policy.verifier_ref,
            "consumer invocation verifier differs from signed policy"
        );
        Ok(Some(Self {
            scenario_id: invocation.scenario_id.clone(),
            verifier_ref: verifier_ref.to_owned(),
        }))
    }

    /// Fixture constructor for gate tests that cannot assemble a signed
    /// policy. Production code obtains a proof only from a checked launch.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(scenario_id: &str, verifier_ref: &str) -> Self {
        Self {
            scenario_id: scenario_id.to_owned(),
            verifier_ref: verifier_ref.to_owned(),
        }
    }

    pub fn scenario_id(&self) -> &str {
        &self.scenario_id
    }

    /// True only for the verifier item this launch was proven to run.
    pub fn authorizes(&self, hop_ref: &str) -> bool {
        self.verifier_ref == hop_ref
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::external_content::products::qualification::{
        PRODUCT_QUALIFICATION_POLICY_SCHEMA, ProductQualificationConsumerExecutionContext,
        ProductQualificationPolicy, ProductQualificationProducerScenario,
        ProductQualificationRemoteVerifierSelection, VerifierProcessSettlementAuthority,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    const VERIFIER: &str = "tool:fixtures/consumer-verifier";

    fn scenario(remote: bool) -> ProductQualificationProducerScenario {
        ProductQualificationProducerScenario {
            recipe_ref: None,
            remote_verifier: remote.then(|| ProductQualificationRemoteVerifierSelection {
                binary_ref: "bin:fixtures/guest-verifier".into(),
                guest_target_triple: "x86_64-unknown-linux-musl".into(),
                archive_members: BTreeMap::new(),
            }),
            consumer_worker_request: None,
            remote_probe_input: Some(json!({"controller":{"scenario":"probe"}})),
        }
    }

    fn policy(remote: bool) -> ProductQualificationPolicySource {
        ProductQualificationPolicySource {
            canonical_ref: "config:fixtures/qualification_policy".into(),
            raw_content_digest: "c".repeat(64),
            effective_definition_digest: "d".repeat(64),
            publisher_fingerprint: "e".repeat(64),
            policy: ProductQualificationPolicy {
                schema: PRODUCT_QUALIFICATION_POLICY_SCHEMA.into(),
                verifier_ref: VERIFIER.into(),
                subject_declaration_id: "runtime".into(),
                allowed_claims: vec!["command_probe".into()],
                minimum_verifier_process_settlement: VerifierProcessSettlementAuthority::ScopeEmpty,
                verifier_parameters: json!({"scope":"bounded_fixture"}),
                consumer_execution_context: None,
                producer_scenarios: BTreeMap::from([("probe".to_owned(), scenario(remote))]),
            },
        }
    }

    fn invocation(scenario_id: &str) -> ConsumerVerificationInputSelection {
        ConsumerVerificationInputSelection {
            scenario_id: scenario_id.into(),
            qualification_operation_id: "a".repeat(64),
            prerequisite_measurement_attempt_id: "b".repeat(64),
        }
    }

    #[test]
    fn launch_without_an_invocation_has_no_proof() {
        for remote in [true, false] {
            assert_eq!(
                RemoteConsumerLaunchProof::from_parts(&policy(remote), None, VERIFIER).unwrap(),
                None
            );
        }
    }

    #[test]
    fn remote_scenario_invocation_yields_a_proof_for_exactly_the_verifier() {
        let proof = RemoteConsumerLaunchProof::from_parts(
            &policy(true),
            Some(&invocation("probe")),
            VERIFIER,
        )
        .unwrap()
        .unwrap();
        assert_eq!(proof.scenario_id(), "probe");
        assert!(proof.authorizes(VERIFIER));
        assert!(!proof.authorizes("tool:fixtures/other"));
        assert!(!proof.authorizes(""));
    }

    #[test]
    fn non_remote_or_unsigned_scenario_is_refused() {
        assert!(
            RemoteConsumerLaunchProof::from_parts(
                &policy(false),
                Some(&invocation("probe")),
                VERIFIER
            )
            .is_err()
        );
        assert!(
            RemoteConsumerLaunchProof::from_parts(
                &policy(true),
                Some(&invocation("absent")),
                VERIFIER
            )
            .is_err()
        );
    }

    #[test]
    fn lane_mismatch_is_refused() {
        let mut worker_lane = policy(true);
        worker_lane.policy.consumer_execution_context =
            Some(ProductQualificationConsumerExecutionContext {
                worker_ref: "worker:codex/external-hosted-authoring".into(),
                product_declaration_id: "guest-runtime".into(),
                environment_ref: "config:codex/environments/external-authoring".into(),
                worker_execution_ref: "worker_execution:codex/bounded-turn".into(),
                environment_binding: "environment".into(),
            });
        // A probe scenario cannot be consumed through the Worker lane.
        assert!(
            RemoteConsumerLaunchProof::from_parts(
                &worker_lane,
                Some(&invocation("probe")),
                VERIFIER
            )
            .is_err()
        );
    }

    #[test]
    fn verifier_differing_from_signed_policy_is_refused() {
        assert!(
            RemoteConsumerLaunchProof::from_parts(
                &policy(true),
                Some(&invocation("probe")),
                "tool:fixtures/other"
            )
            .is_err()
        );
    }
}
