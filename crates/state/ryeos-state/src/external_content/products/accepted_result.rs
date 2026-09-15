//! Immutable accepted output products, not a build cache or execution grant.
//!
//! The app authorizes the operator, current publication heads, source/policy
//! resolution and completed producer. This owner authenticates the supplied
//! attestations and binds their exact accepted result. Historical producer
//! coordinates stay non-owning testimony.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::publication::{ProductCaptureCoordinate, VerifiedProductWitness};
use super::qualification::{ProductQualificationEvidence, ProductQualificationPolicySource};
use super::qualification_publication::{QualificationCoordinate, VerifiedQualificationWitness};
use super::{MAX_PRODUCTS, ProductCaptureEvidence, ProductSource, validate_hash, validate_name};
use crate::objects::canonical_value_digest;

pub const PRODUCT_BUILD_ACCEPTED_RESULT_KIND: &str = "product_build_accepted_result";
pub const PRODUCT_BUILD_ACCEPTED_RESULT_SCHEMA: &str = "ryeos.product_build_accepted_result.v1";
pub const MAX_PRODUCT_BUILD_ACCEPTED_RESULT_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductBuildAcceptedProduct {
    pub product_name: String,
    pub witness_hash: String,
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub qualification_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductBuildAcceptedResult {
    pub schema: String,
    pub kind: String,
    pub owner_principal: String,
    pub producer_ref: String,
    pub producer_project_snapshot_hash: String,
    pub producer_effective_definition_digest: String,
    pub producer_parameters_digest: String,
    pub producer_partition_identity: String,
    pub products: Vec<ProductBuildAcceptedProduct>,
}

/// In-process inputs, never a deserializable caller authority projection.
pub struct ProductBuildAcceptance<'a> {
    pub product: &'a VerifiedProductWitness,
    pub qualification: Option<ProductBuildQualificationAcceptance<'a>>,
}

/// These current-source facts must come from the app's trusted resolution
/// owner, not from an invocation parameter or the attestation being checked.
pub struct ProductBuildQualificationAcceptance<'a> {
    pub proof: &'a VerifiedQualificationWitness,
    pub current_policy: &'a ProductQualificationPolicySource,
    pub current_verifier_effective_definition_digest: &'a str,
    pub current_verifier_artifact_identity: &'a crate::objects::AdmittedLaunchArtifactIdentity,
    pub required_claims: &'a [String],
    pub observed_at: &'a str,
}

impl ProductBuildAcceptedResult {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        if lillux::canonical_json(value)?.len() > MAX_PRODUCT_BUILD_ACCEPTED_RESULT_BYTES {
            bail!("accepted product build result exceeds its byte bound");
        }
        let result: Self = serde_json::from_value(value.clone())?;
        result.validate()?;
        Ok(result)
    }

    /// Structural validation does not replace authenticated evidence or the
    /// app's current eligibility checks before publishing or consuming a hit.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCT_BUILD_ACCEPTED_RESULT_SCHEMA
            || self.kind != PRODUCT_BUILD_ACCEPTED_RESULT_KIND
        {
            bail!("unsupported accepted product build result contract");
        }
        validate_hash(
            "accepted product owner",
            self.owner_principal
                .strip_prefix("fp:")
                .context("accepted product owner must be a fingerprint principal")?,
        )?;
        super::validate_canonical_unsuffixed_ref("accepted producer", &self.producer_ref)?;
        for hash in [
            &self.producer_project_snapshot_hash,
            &self.producer_effective_definition_digest,
            &self.producer_parameters_digest,
            &self.producer_partition_identity,
        ] {
            validate_hash("accepted producer identity", hash)?;
        }
        if self.products.is_empty() || self.products.len() > MAX_PRODUCTS {
            bail!("accepted product result requires a bounded nonempty product set");
        }
        let mut previous: Option<&str> = None;
        for product in &self.products {
            validate_name(&product.product_name)?;
            validate_hash("accepted product witness", &product.witness_hash)?;
            if let Some(hash) = &product.qualification_hash {
                validate_hash("accepted product qualification", hash)?;
            }
            if previous.is_some_and(|previous| previous >= product.product_name.as_str()) {
                bail!("accepted products must be strictly ordered and unique by name");
            }
            previous = Some(&product.product_name);
        }
        if lillux::canonical_json(&serde_json::to_value(self)?)?.len()
            > MAX_PRODUCT_BUILD_ACCEPTED_RESULT_BYTES
        {
            bail!("accepted product build result exceeds its byte bound");
        }
        Ok(())
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        canonical_value_digest(&self.to_value()?)
    }

    /// Construct only from actual signed node testimony. This does not launch
    /// a producer, create a replay entry, or infer current-head eligibility.
    pub fn from_authenticated_products(
        owner: &str,
        node_key: &lillux::crypto::VerifyingKey,
        inputs: &[ProductBuildAcceptance<'_>],
    ) -> anyhow::Result<Self> {
        if inputs.is_empty() || inputs.len() > MAX_PRODUCTS {
            bail!("accepted build requires a bounded nonempty product set");
        }
        let first = authenticate_product(inputs[0].product, owner, node_key)?;
        let partition = first
            .producer_partition_identity
            .clone()
            .context("accepted build requires workspace-output-backed products")?;
        let mut products = Vec::with_capacity(inputs.len());
        for input in inputs {
            let evidence = authenticate_product(input.product, owner, node_key)?;
            if !matches!(
                evidence.declaration.source,
                ProductSource::WorkspaceOutput { .. }
            ) || evidence.producer != first.producer
                || evidence.root_producer != first.root_producer
                || evidence.producer_partition_identity.as_deref() != Some(&partition)
                || evidence.chain_root_id != first.chain_root_id
                || evidence.thread_id != first.thread_id
                || evidence.admitted_launch_capsule_hash != first.admitted_launch_capsule_hash
                || evidence.workspace_output_capture_hash != first.workspace_output_capture_hash
                || evidence.result_project_snapshot_hash != first.result_project_snapshot_hash
                || evidence.recipe_binding != first.recipe_binding
                || evidence.recipe_ref != first.recipe_ref
                || evidence.recipe_raw_content_digest != first.recipe_raw_content_digest
                || evidence.declarations_hash != first.declarations_hash
                || evidence.relationships != first.relationships
                || evidence.capture_policy_digest != first.capture_policy_digest
            {
                bail!("accepted products disagree on their exact admitted producer or partition");
            }
            let qualification_hash = input
                .qualification
                .as_ref()
                .map(|qualification| -> anyhow::Result<String> {
                    let proof = qualification.proof;
                    let actual = ProductQualificationEvidence::verify_attestation_for_owner(
                        &proof.attestation,
                        node_key,
                        owner,
                    )?;
                    if canonical_value_digest(&proof.attestation.to_value())?
                        != proof.attestation_hash
                        || actual != proof.evidence
                        || QualificationCoordinate::from_evidence(&actual)?.coordinate_id()?
                            != proof.coordinate_id
                        || actual.product_witness_hash != input.product.attestation_hash
                        || actual.product_coordinate
                            != ProductCaptureCoordinate::from_evidence(&evidence)?
                        || actual.result.subject_manifest_hash != evidence.manifest_hash
                    {
                        bail!(
                            "accepted qualification does not authenticate the exact product witness"
                        );
                    }
                    if proof.attestation.is_expired_at(qualification.observed_at)? {
                        bail!("accepted product qualification has expired");
                    }
                    actual.validate_current_policy(
                        qualification.current_policy,
                        qualification.current_verifier_effective_definition_digest,
                        qualification.required_claims,
                    )?;
                    actual.validate_current_artifact(
                        qualification.current_verifier_artifact_identity,
                    )?;
                    Ok(proof.attestation_hash.clone())
                })
                .transpose()?;
            products.push(ProductBuildAcceptedProduct {
                product_name: evidence.declaration.name,
                witness_hash: input.product.attestation_hash.clone(),
                qualification_hash,
            });
        }
        products.sort_by(|left, right| left.product_name.cmp(&right.product_name));
        for required in first
            .declarations
            .products
            .iter()
            .filter(|product| product.required)
        {
            if products
                .binary_search_by(|product| product.product_name.cmp(&required.name))
                .is_err()
            {
                bail!("accepted build omitted a required declared product");
            }
        }
        let result = Self {
            schema: PRODUCT_BUILD_ACCEPTED_RESULT_SCHEMA.into(),
            kind: PRODUCT_BUILD_ACCEPTED_RESULT_KIND.into(),
            owner_principal: owner.into(),
            producer_ref: first.root_producer.canonical_ref,
            producer_project_snapshot_hash: first.root_producer.producer_project_snapshot_hash,
            producer_effective_definition_digest: first.root_producer.effective_definition_digest,
            producer_parameters_digest: first.root_producer.admitted_parameters_digest,
            producer_partition_identity: partition,
            products,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate_against_authenticated_products(
        &self,
        owner: &str,
        node_key: &lillux::crypto::VerifyingKey,
        inputs: &[ProductBuildAcceptance<'_>],
    ) -> anyhow::Result<()> {
        self.validate()?;
        if self != &Self::from_authenticated_products(owner, node_key, inputs)? {
            bail!("accepted result contradicts its authenticated products");
        }
        Ok(())
    }
}

fn authenticate_product(
    witness: &VerifiedProductWitness,
    owner: &str,
    node_key: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<ProductCaptureEvidence> {
    let evidence = ProductCaptureEvidence::verify_attestation_for_owner(
        &witness.attestation,
        node_key,
        owner,
    )?;
    if canonical_value_digest(&witness.attestation.to_value())? != witness.attestation_hash
        || evidence != witness.evidence
        || ProductCaptureCoordinate::from_evidence(&evidence)?.coordinate_id()?
            != witness.coordinate_id
    {
        bail!("accepted product witness contradicts its authenticated node testimony");
    }
    Ok(evidence)
}

#[cfg(test)]
mod tests;
