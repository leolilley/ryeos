//! Canonical accepted input record for a hosted qualification consumer.
//!
//! This is retained data agreement, not accepted-root authentication, provider
//! contact authority, or a verifier's interpretation of the runtime protocol.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeVerificationCoordinate, ConsumerRuntimeVerifierSelection,
    RESTORED_OWNER_MEASUREMENT_PROTOCOL, RemoteVerificationPurpose, RestoredOwnerMeasurement,
    RestoredVerifierAttemptIntent,
};
use serde::{Deserialize, Serialize};

use super::products::qualification::ProductQualificationProducerScenario;
use super::qualification_purpose::{QualificationLaunchPurpose, QualificationRemoteUse};
use crate::external_execution::admission::ExternalCandidateRequirement;

pub const MAX_CONSUMER_INPUT_RECORD_BYTES: usize = 96 * 1024;

/// Exact input shared by the controller and the independently admitted guest
/// verifier. Protocol-specific checks belong to the selected verifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerInputRecord {
    pub schema: u32,
    pub purpose: QualificationLaunchPurpose,
    pub coordinate: ConsumerRuntimeVerificationCoordinate,
    pub selection: ConsumerRuntimeVerifierSelection,
    pub requirement: ExternalCandidateRequirement,
    /// Product-owned challenge data. RyeOS retains it verbatim and never uses
    /// it to select a product, executable, provider, or contact operation.
    pub scenario_input: serde_json::Value,
}

impl ConsumerInputRecord {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
            "consumer input record exceeds its bound"
        );
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        ensure!(
            lillux::canonical_json(&value)?.as_bytes() == bytes,
            "consumer input record is not canonical"
        );
        let record: Self = serde_json::from_value(value)?;
        record.validate()?;
        Ok(record)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes();
        ensure!(
            bytes.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
            "consumer input record exceeds its bound"
        );
        Ok(bytes)
    }

    pub fn validate_attempt(&self, intent: &RestoredVerifierAttemptIntent) -> Result<()> {
        self.validate()?;
        self.purpose
            .require_consumer_invocation(&intent.qualification_operation_id, &self.coordinate)?;
        let RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. } = &intent.purpose else {
            anyhow::bail!("owner measurement cannot consume a consumer input archive");
        };
        ensure!(
            coordinate == &self.coordinate
                && intent.verifier_artifact_hash == self.selection.verifier_artifact_hash,
            "consumer input archive differs from exact run attempt"
        );
        ensure!(
            intent.upload_bytes > 0
                && intent.upload_bytes <= self.selection.archive_budget.maximum_framed_bytes
                && lillux::valid_hash(&intent.upload_sha256)
                && intent.attempt_deadline_ms > 0,
            "consumer attempt exceeds protected archive budget or has invalid upload identity"
        );
        intent.consumer_challenge_digest()?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 2, "unsupported consumer input record schema");
        ensure!(
            self.scenario_input.is_object()
                && serde_json::to_vec(&self.scenario_input)?.len()
                    <= MAX_CONSUMER_INPUT_RECORD_BYTES,
            "consumer scenario input must be a bounded object"
        );
        self.purpose
            .validate_remote_consumer_coordinate(&self.coordinate, &self.selection)?;
        self.requirement.validate()?;
        let content = self
            .purpose
            .consumer_content
            .as_ref()
            .context("consumer input record has no accepted content")?;
        let qualified_use = content
            .qualification_use
            .as_ref()
            .context("consumer input record has no admitted use")?;
        ensure!(
            self.purpose
                .policy_source
                .policy
                .consumer_execution_context
                .is_some(),
            "consumer input record has no signed consumer context"
        );
        ensure!(
            self.requirement.qualification_requirement_digest()?
                == qualified_use.requirement_digest
                && self.requirement.runtime_product_declaration_id
                    == content.runtime_realization.id
                && self.requirement.runtime_recipe.executable_relative_path
                    == content.runtime_member.relative_path,
            "consumer input recipe differs from accepted use"
        );
        Ok(())
    }
}

pub const REMOTE_PROBE_INPUT_RECORD_SCHEMA: &str = "ryeos.remote_probe_input.v1";
const MAX_REMOTE_PROBE_SCENARIO_INPUT_BYTES: usize = 8 * 1024;

/// Exact input shared by the controller and an independently admitted guest
/// verifier that runs without a Worker consumer. The prerequisite is the
/// daemon-retained owner measurement of the restored product, never a value
/// the verifier supplies; `scenario_input` is the signed scenario's opaque
/// probe input, retained verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteProbeInputRecord {
    pub schema: String,
    pub purpose: QualificationLaunchPurpose,
    pub coordinate: ConsumerRuntimeVerificationCoordinate,
    pub selection: ConsumerRuntimeVerifierSelection,
    pub prerequisite: RestoredOwnerMeasurement,
    pub scenario_input: serde_json::Value,
}

impl RemoteProbeInputRecord {
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        ensure!(
            lillux::canonical_json(value)?.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
            "remote probe input record exceeds its bound"
        );
        let record: Self =
            serde_json::from_value(value.clone()).context("decode remote probe input record")?;
        record.validate()?;
        Ok(record)
    }

    pub fn to_value(&self) -> Result<serde_json::Value> {
        self.validate()?;
        let value = serde_json::to_value(self)?;
        ensure!(
            lillux::canonical_json(&value)?.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
            "remote probe input record exceeds its bound"
        );
        Ok(value)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        Ok(lillux::canonical_json(&self.to_value()?)?.into_bytes())
    }

    /// True when the retained input is exactly the signed scenario's probe
    /// input. Presence of the scenario in the purpose's policy is checked by
    /// `validate`; this compares against a policy scenario the caller holds.
    pub fn matches_scenario(&self, scenario: &ProductQualificationProducerScenario) -> bool {
        scenario.consumer_worker_request.is_none()
            && scenario.remote_probe_input.as_ref() == Some(&self.scenario_input)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == REMOTE_PROBE_INPUT_RECORD_SCHEMA,
            "unsupported remote probe input record schema"
        );
        ensure!(
            self.scenario_input.is_object()
                && lillux::canonical_json(&self.scenario_input)?.len()
                    <= MAX_REMOTE_PROBE_SCENARIO_INPUT_BYTES,
            "remote probe scenario input must be a bounded object"
        );
        self.purpose
            .validate_remote_consumer_coordinate(&self.coordinate, &self.selection)?;
        ensure!(
            self.purpose
                .policy_source
                .policy
                .consumer_execution_context
                .is_none(),
            "remote probe input record cannot accompany a signed consumer context"
        );
        let QualificationRemoteUse::Probe { input } =
            self.purpose.remote_use(&self.coordinate.scenario_id)?
        else {
            anyhow::bail!("remote probe input record scenario is not a probe");
        };
        ensure!(
            input == &self.scenario_input,
            "remote probe input differs from the signed scenario"
        );
        ensure!(
            self.prerequisite.schema == 1
                && self.prerequisite.protocol == RESTORED_OWNER_MEASUREMENT_PROTOCOL
                && lillux::valid_hash(&self.prerequisite.challenge_digest)
                && lillux::valid_hash(&self.prerequisite.manifest_hash)
                && lillux::valid_hash(&self.prerequisite.owner_executable_sha256)
                && !self.prerequisite.controller_public_root.is_empty()
                && self.prerequisite.manifest_hash == self.purpose.subject_manifest_hash,
            "remote probe prerequisite is not the measured subject"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::canonical_value_digest;
    use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget;

    fn probe_record() -> RemoteProbeInputRecord {
        let (source, policy) =
            super::super::products::qualification::remote_verifier_source::tests::probe_fixture();
        let mut purpose = super::super::products::qualification::tests::launch_purpose();
        purpose.policy_source = policy;
        purpose
            .remote_verifier_sources
            .insert("probe".into(), source.clone());
        purpose.consumer_invocation = Some(
            ryeos_external_execution_contract::restored_runtime_measurement::ConsumerVerificationInputSelection {
                scenario_id: "probe".into(),
                qualification_operation_id: "a".repeat(64),
                prerequisite_measurement_attempt_id: "b".repeat(64),
            },
        );
        purpose.validate().unwrap();
        let selection = ConsumerRuntimeVerifierSelection {
            scenario_source_digest: source.scenario_source_digest.clone(),
            verifier_artifact_hash: source.executor.payload_blob_hash.clone(),
            archive_budget: ConsumerArchiveBudget::new(16, 4096, 8192).unwrap(),
        };
        let input = source.remote_probe_input.clone().unwrap();
        let coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-pure-data-fixture".into(),
            accepted_capsule_hash: "a".repeat(64),
            qualification_purpose_digest: canonical_value_digest(
                &serde_json::to_value(&purpose).unwrap(),
            )
            .unwrap(),
            scenario_id: "probe".into(),
            scenario_source_digest: selection.scenario_source_digest.clone(),
            subject_digest: canonical_value_digest(
                &serde_json::to_value(&purpose.subject).unwrap(),
            )
            .unwrap(),
            use_digest: canonical_value_digest(&input).unwrap(),
            prerequisite_measurement_attempt_id: "b".repeat(64),
            prerequisite_measurement_observation_digest: "c".repeat(64),
        };
        RemoteProbeInputRecord {
            schema: REMOTE_PROBE_INPUT_RECORD_SCHEMA.into(),
            prerequisite: RestoredOwnerMeasurement {
                schema: 1,
                protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
                challenge_digest: "d".repeat(64),
                manifest_hash: purpose.subject_manifest_hash.clone(),
                owner_executable_sha256: "e".repeat(64),
                controller_public_root: "ed25519:fixture".into(),
            },
            purpose,
            coordinate,
            selection,
            scenario_input: input,
        }
    }

    #[test]
    fn probe_purpose_splits_use_and_round_trips() {
        let record = probe_record();
        let QualificationRemoteUse::Probe { input } = record.purpose.remote_use("probe").unwrap()
        else {
            panic!("probe scenario resolved to a Worker use");
        };
        assert_eq!(input, &record.scenario_input);
        assert_eq!(
            record
                .purpose
                .remote_use("probe")
                .unwrap()
                .use_digest()
                .unwrap(),
            record.coordinate.use_digest
        );
        assert!(record.purpose.remote_use("absent").is_err());
        let wire = serde_json::to_value(&record.purpose).unwrap();
        let back: QualificationLaunchPurpose = serde_json::from_value(wire).unwrap();
        assert_eq!(back, record.purpose);
        let mut wrong_use = record.coordinate.clone();
        wrong_use.use_digest = "0".repeat(64);
        assert!(
            record
                .purpose
                .validate_remote_consumer_coordinate(&wrong_use, &record.selection)
                .is_err()
        );
    }

    #[test]
    fn record_round_trips_refuses_unknown_fields_and_matches_its_scenario() {
        let record = probe_record();
        let value = record.to_value().unwrap();
        assert_eq!(RemoteProbeInputRecord::from_value(&value).unwrap(), record);
        assert_eq!(
            lillux::canonical_json(&value).unwrap().into_bytes(),
            record.canonical_bytes().unwrap()
        );
        let mut unknown = value.clone();
        unknown["requirement"] = serde_json::json!({});
        assert!(RemoteProbeInputRecord::from_value(&unknown).is_err());
        let mut wrong_schema = value.clone();
        wrong_schema["schema"] = serde_json::json!("ryeos.remote_probe_input.v0");
        assert!(RemoteProbeInputRecord::from_value(&wrong_schema).is_err());
        let scenario = &record.purpose.policy_source.policy.producer_scenarios["probe"];
        assert!(record.matches_scenario(scenario));
        let mut changed = scenario.clone();
        changed.remote_probe_input = Some(serde_json::json!({"probe":"other"}));
        assert!(!record.matches_scenario(&changed));
        let mut diverged = record.clone();
        diverged.scenario_input = serde_json::json!({"probe":"other"});
        assert!(diverged.validate().is_err());
        let mut wrong_prerequisite = record.clone();
        wrong_prerequisite.prerequisite.manifest_hash = "9".repeat(64);
        assert!(wrong_prerequisite.validate().is_err());
        let mut worker_lane = record;
        worker_lane
            .purpose
            .policy_source
            .policy
            .consumer_execution_context =
            Some(super::super::products::qualification::tests::test_consumer_context());
        assert!(worker_lane.validate().is_err());
    }
}
