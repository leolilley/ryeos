//! Constrained publisher boundary for one accepted native bundle candidate.
//!
//! The private verified candidate prevents this surface from degrading into a
//! generic sign-any-tree or sign-any-hash endpoint.

use anyhow::{Context as _, bail};
use ryeos_bundle_publication_contract::{
    PUBLISHER_MATERIALIZATION_RESULT_KIND, PUBLISHER_MATERIALIZATION_RESULT_SCHEMA,
    PublisherMaterializationResult, PublisherMutationContract,
};
use ryeos_state::{
    external_content::products::accepted_result::ProductBuildAcceptedResult,
    objects::ExternalContentManifestObject,
};

use super::{
    PublicationObjectReader, PublisherMaterializationProof, read_exact,
    tree::validate_native_bundle_tree,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublisherCandidateCoordinate {
    pub accepted_product_result_hash: String,
    pub selected_product_name: String,
    pub selected_product_witness_hash: String,
    pub input_content_manifest_hash: String,
}

#[derive(Debug)]
pub struct VerifiedPublisherCandidate {
    coordinate: PublisherCandidateCoordinate,
    accepted_result: ProductBuildAcceptedResult,
    input_manifest: ExternalContentManifestObject,
}

impl VerifiedPublisherCandidate {
    pub fn coordinate(&self) -> &PublisherCandidateCoordinate {
        &self.coordinate
    }

    pub fn accepted_result(&self) -> &ProductBuildAcceptedResult {
        &self.accepted_result
    }

    pub fn input_manifest(&self) -> &ExternalContentManifestObject {
        &self.input_manifest
    }
}

/// Exact result returned by the purpose-owned tree signer. Implementations may
/// create only the RyeOS bundle-signing mutation; they receive no arbitrary
/// attestation subject or claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedBundleTree {
    pub output_content_manifest_hash: String,
    pub output_manifest_item_hash: String,
    pub publisher_fingerprint: String,
    pub publisher_tool_effective_definition_digest: String,
    pub publisher_tool_artifact_identity_hash: String,
}

pub trait ConstrainedBundleTreePublisher: Send + Sync {
    fn authorize_build_recipe(
        &self,
        _request: &super::recipe::AuthorizeBuildRecipeRequest,
    ) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("publisher has no explicit release recipe authoring authority")
    }

    fn authorize_capture_recipe(
        &self,
        _request: &super::recipe::AuthorizeCaptureRecipeRequest,
    ) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("publisher has no explicit signed-capture recipe authoring authority")
    }

    fn materialize_and_sign(
        &self,
        candidate: &VerifiedPublisherCandidate,
    ) -> anyhow::Result<SignedBundleTree>;
}

pub fn verify_publisher_candidate(
    coordinate: PublisherCandidateCoordinate,
    objects: &impl PublicationObjectReader,
) -> anyhow::Result<VerifiedPublisherCandidate> {
    require_hash(
        &coordinate.accepted_product_result_hash,
        "accepted product result",
    )?;
    require_hash(
        &coordinate.selected_product_witness_hash,
        "selected product witness",
    )?;
    require_hash(&coordinate.input_content_manifest_hash, "input manifest")?;
    let accepted_result = ProductBuildAcceptedResult::from_value(&read_exact(
        objects,
        &coordinate.accepted_product_result_hash,
    )?)?;
    let selected = accepted_result
        .products
        .iter()
        .find(|product| product.product_name == coordinate.selected_product_name)
        .context("publisher candidate product is absent from accepted result")?;
    if selected.witness_hash != coordinate.selected_product_witness_hash {
        bail!("publisher candidate witness disagrees with accepted result");
    }
    let input_manifest = ExternalContentManifestObject::from_value(&read_exact(
        objects,
        &coordinate.input_content_manifest_hash,
    )?)?;
    validate_native_bundle_tree(&input_manifest)?;
    Ok(VerifiedPublisherCandidate {
        coordinate,
        accepted_result,
        input_manifest,
    })
}

/// Execute and independently verify the one allowed publisher mutation, then
/// retain its typed result. A caller cannot choose the result's accepted
/// product, input tree, mutation contract, or publisher identity independently
/// of the verified candidate and constrained signer.
pub fn materialize_publisher_candidate(
    candidate: &VerifiedPublisherCandidate,
    publisher: &(impl ConstrainedBundleTreePublisher + ?Sized),
    proof: &(impl PublisherMaterializationProof + ?Sized),
    cas: &lillux::CasStore,
) -> anyhow::Result<String> {
    let signed = publisher.materialize_and_sign(candidate)?;
    let output_manifest = ExternalContentManifestObject::from_value(&read_exact(
        cas,
        &signed.output_content_manifest_hash,
    )?)?;
    validate_native_bundle_tree(&output_manifest)?;
    let result = PublisherMaterializationResult {
        schema: PUBLISHER_MATERIALIZATION_RESULT_SCHEMA.to_owned(),
        kind: PUBLISHER_MATERIALIZATION_RESULT_KIND.to_owned(),
        accepted_product_result_hash: candidate.coordinate.accepted_product_result_hash.clone(),
        selected_product_identity: candidate.coordinate.selected_product_name.clone(),
        selected_product_witness: candidate.coordinate.selected_product_witness_hash.clone(),
        input_content_manifest_hash: candidate.coordinate.input_content_manifest_hash.clone(),
        output_content_manifest_hash: signed.output_content_manifest_hash,
        output_manifest_item_hash: signed.output_manifest_item_hash,
        publisher_fingerprint: signed.publisher_fingerprint,
        publisher_tool_effective_definition_digest: signed
            .publisher_tool_effective_definition_digest,
        publisher_tool_artifact_identity_hash: signed.publisher_tool_artifact_identity_hash,
        mutation_contract: PublisherMutationContract::RyeosBundleSignV1,
    };
    result.validate()?;
    proof.verify_closed_mutation(&result, &candidate.input_manifest, &output_manifest)?;
    Ok(cas.put_object(&result.to_value()?)?.hash)
}

fn require_hash(value: &str, label: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be a lowercase 64-hex digest");
    }
    Ok(())
}
