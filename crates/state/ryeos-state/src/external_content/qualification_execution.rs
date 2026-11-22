//! Common inputs to contract-driven qualification execution.
//!
//! This borrowed view does not admit a launch or authenticate a subject. Its
//! private constructor validates the enclosing purpose, and its digest covers
//! that entire purpose, including its source-specific provenance. Execution
//! owners must still join it to the authenticated sealed root and launch owner.
//! No durable schema or second qualification workflow is introduced here.

use std::collections::BTreeMap;

use anyhow::Result;

use super::products::qualification::{
    ProductProducerRecipeSourceIdentity, ProductQualificationConsumerContentIdentity,
    ProductQualificationPolicySource,
};
use super::qualification_purpose::QualificationLaunchPurpose;
use crate::objects::canonical_value_digest;

/// Validated immutable data for execution and evidence projection. It is
/// deliberately neither deserializable nor independently constructible.
#[derive(Debug)]
pub struct QualificationExecutionPurposeView<'a> {
    purpose: &'a QualificationLaunchPurpose,
    enclosing_purpose_digest: String,
    policy_source: &'a ProductQualificationPolicySource,
    consumer_content: Option<&'a ProductQualificationConsumerContentIdentity>,
    producer_recipe_sources: &'a BTreeMap<String, ProductProducerRecipeSourceIdentity>,
    remote_verifier_sources: &'a BTreeMap<
        String,
        super::products::qualification::remote_verifier_source::QualificationRemoteVerifierSource,
    >,
    subject_declaration_id: &'a str,
    subject_manifest_hash: &'a str,
}

impl<'a> QualificationExecutionPurposeView<'a> {
    pub(crate) fn from_purpose(purpose: &'a QualificationLaunchPurpose) -> Result<Self> {
        purpose.validate()?;
        Ok(Self {
            purpose,
            enclosing_purpose_digest: canonical_value_digest(&serde_json::to_value(purpose)?)?,
            policy_source: &purpose.policy_source,
            consumer_content: purpose.consumer_content.as_ref(),
            producer_recipe_sources: &purpose.producer_recipe_sources,
            remote_verifier_sources: &purpose.remote_verifier_sources,
            subject_declaration_id: &purpose.subject_declaration_id,
            subject_manifest_hash: &purpose.subject_manifest_hash,
        })
    }

    /// Compare against a view freshly derived from the authenticated sealed
    /// purpose. Shared fields alone cannot establish the original provenance,
    /// launch owner, or verifier identity.
    pub fn has_same_enclosing_purpose(&self, sealed: &Self) -> bool {
        self.enclosing_purpose_digest == sealed.enclosing_purpose_digest
    }

    /// Delegate the full subject/source/use join to the same validated sealed
    /// purpose; a partial execution view must not reconstruct that authority.
    pub fn validate_remote_consumer_coordinate(
        &self,
        coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
        selection: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerifierSelection,
    ) -> Result<()> {
        self.purpose
            .validate_remote_consumer_coordinate(coordinate, selection)
    }

    pub fn owner_fingerprint(&self) -> &'a str {
        &self.purpose.owner_fingerprint
    }

    pub fn policy_source(&self) -> &'a ProductQualificationPolicySource {
        self.policy_source
    }

    pub fn consumer_content(&self) -> Option<&'a ProductQualificationConsumerContentIdentity> {
        self.consumer_content
    }

    pub fn producer_recipe_sources(
        &self,
    ) -> &'a BTreeMap<String, ProductProducerRecipeSourceIdentity> {
        self.producer_recipe_sources
    }

    pub fn subject_declaration_id(&self) -> &'a str {
        self.subject_declaration_id
    }

    pub fn remote_verifier_source(
        &self,
        scenario_id: &str,
    ) -> Option<&'a super::products::qualification::remote_verifier_source::QualificationRemoteVerifierSource>{
        self.remote_verifier_sources.get(scenario_id)
    }

    pub fn subject_manifest_hash(&self) -> &'a str {
        self.subject_manifest_hash
    }
}
