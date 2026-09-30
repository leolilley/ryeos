//! Canonical accepted input record for a hosted qualification consumer.
//!
//! This is retained data agreement, not accepted-root authentication, provider
//! contact authority, or a verifier's interpretation of the runtime protocol.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeVerificationCoordinate, ConsumerRuntimeVerifierSelection,
    RemoteVerificationPurpose, RestoredVerifierAttemptIntent,
};
use serde::{Deserialize, Serialize};

use super::qualification_purpose::QualificationLaunchPurpose;
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
