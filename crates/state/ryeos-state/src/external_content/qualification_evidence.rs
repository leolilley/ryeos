//! Acquired-content testimony. Structural validation is not source authentication,
//! execution corroboration, publication authority, or runtime qualification.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::products::qualification::{
    MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES, ProductQualificationExecutionProof,
    ProductQualificationResult, ProductQualificationVerifier, validate_qualification_execution,
};
use super::qualification_purpose::{QualificationLaunchPurpose, QualificationSubject};

pub const CONTENT_QUALIFICATION_EVIDENCE_SCHEMA: &str = "ryeos.content_qualification_evidence.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentQualificationEvidence {
    pub schema: String,
    /// Exact sealed purpose, not a reconstruction from current source records.
    pub purpose: QualificationLaunchPurpose,
    pub verifier: ProductQualificationVerifier,
    pub execution_proof: ProductQualificationExecutionProof,
    pub result: ProductQualificationResult,
}

impl ContentQualificationEvidence {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        if serde_json::to_vec(value)?.len() > MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES {
            bail!("content qualification evidence exceeds its bound");
        }
        let evidence: Self = serde_json::from_value(value.clone())
            .context("decode content qualification evidence")?;
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != CONTENT_QUALIFICATION_EVIDENCE_SCHEMA {
            bail!("unsupported content qualification evidence schema");
        }
        self.purpose.validate()?;
        let QualificationSubject::ActivatedContent { content } = &self.purpose.subject else {
            bail!("content qualification evidence requires activated content");
        };
        if self
            .purpose
            .policy_source
            .policy
            .consumer_execution_context
            .is_some()
            || self.purpose.consumer_content.is_some()
            || self.purpose.consumer_definitions.is_some()
        {
            bail!("content consumer-context qualification evidence is not supported");
        }
        validate_qualification_execution(
            &self.purpose.policy_source,
            &self.verifier,
            &self.execution_proof,
            &self.result,
            &self.purpose.required_claims,
        )?;
        if self.verifier.canonical_ref != self.purpose.verifier_ref
            || self.verifier.effective_definition_digest
                != self.purpose.verifier_realized_definition_digest
            || self.verifier.admitted_parameters_digest != self.purpose.admitted_parameters_digest
            || self.verifier.subject_declaration_id != self.purpose.subject_declaration_id
            || self.verifier.subject_manifest_hash != self.purpose.subject_manifest_hash
            || self.verifier.subject_manifest_hash != content.manifest_hash
        {
            bail!("content qualification execution contradicts its sealed purpose");
        }
        if let Some(scoped) = &self.execution_proof.scoped_attempt
            && self
                .purpose
                .producer_recipe_sources
                .get(&scoped.scenario_id)
                != Some(&scoped.producer_source)
        {
            bail!("content qualification scoped attempt differs from its sealed recipe source");
        }
        if serde_json::to_vec(self)?.len() > MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES {
            bail!("content qualification evidence exceeds its bound");
        }
        Ok(())
    }
}
