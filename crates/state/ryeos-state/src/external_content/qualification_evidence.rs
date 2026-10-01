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

pub const CONTENT_QUALIFICATION_EVIDENCE_SCHEMA: &str = "ryeos.content_qualification_evidence.v6";
pub const CONTENT_QUALIFICATION_ATTESTATION_POLICY: &str = "ryeos.content_qualification.v1";
pub const CONTENT_QUALIFICATION_CLAIM: &str = "activated_content_qualified";

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
    /// Signing must be called by the authenticated proof/publication owner.
    /// This structural helper does not corroborate execution or grant claims.
    pub fn sign_attestation(
        &self,
        signer: &dyn crate::Signer,
        issued_at: String,
        expires_at: Option<String>,
    ) -> anyhow::Result<crate::objects::Attestation> {
        self.validate()?;
        crate::objects::Attestation::unsigned(
            self.result.subject_manifest_hash.clone(),
            CONTENT_QUALIFICATION_CLAIM.into(),
            CONTENT_QUALIFICATION_ATTESTATION_POLICY.into(),
            issued_at,
            expires_at,
            serde_json::to_value(self)?,
        )
        .sign(signer)
    }

    pub fn from_attestation(attestation: &crate::objects::Attestation) -> anyhow::Result<Self> {
        attestation.validate()?;
        if attestation.claim != CONTENT_QUALIFICATION_CLAIM
            || attestation.policy != CONTENT_QUALIFICATION_ATTESTATION_POLICY
        {
            bail!("attestation is not activated-content qualification testimony");
        }
        let evidence = Self::from_value(&attestation.evidence)?;
        if attestation.subject_hash != evidence.result.subject_manifest_hash {
            bail!("content qualification attestation contradicts its admitted subject");
        }
        Ok(evidence)
    }

    /// Exact issuer and launch-owner authentication. Expiry, published-head
    /// authority and current-source eligibility are separate admission checks.
    pub fn verify_attestation_for_owner(
        attestation: &crate::objects::Attestation,
        node_key: &lillux::crypto::VerifyingKey,
        owner_fingerprint: &str,
    ) -> anyhow::Result<Self> {
        attestation.verify_with_key(node_key)?;
        let evidence = Self::from_attestation(attestation)?;
        if evidence.purpose.owner_fingerprint != owner_fingerprint {
            bail!("content qualification belongs to another launch owner");
        }
        Ok(evidence)
    }

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
        if let Some(scoped) = self.execution_proof.subordinate_attempt.as_ref().and_then(super::products::qualification::ProductQualificationSubordinateAttemptProof::scoped_producer)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_testimony_refuses_predecessor_schema_and_subordinate_slot() {
        let capsule =
            crate::external_execution::admission::test_support::content_candidate_capsule(
                &"a".repeat(64),
            )
            .unwrap();
        let evidence = capsule
            .retained_external_runtime_content_qualification
            .unwrap()
            .evidence;
        let current = serde_json::to_value(evidence).unwrap();
        ContentQualificationEvidence::from_value(&current).unwrap();
        let mut predecessor = current.clone();
        predecessor["schema"] = serde_json::json!("ryeos.content_qualification_evidence.v5");
        assert!(ContentQualificationEvidence::from_value(&predecessor).is_err());
        let mut missing = current.clone();
        missing["execution_proof"]
            .as_object_mut()
            .unwrap()
            .remove("subordinate_attempt");
        assert!(ContentQualificationEvidence::from_value(&missing).is_err());
        missing["execution_proof"]["scoped_attempt"] = Value::Null;
        assert!(ContentQualificationEvidence::from_value(&missing).is_err());
        let mut widened = current;
        widened["execution_proof"]["scoped_attempt"] = Value::Null;
        assert!(ContentQualificationEvidence::from_value(&widened).is_err());
    }
}
