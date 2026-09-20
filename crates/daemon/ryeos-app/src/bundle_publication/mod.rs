//! Context-aware admission for native bundle publication objects.
//!
//! Wire decoding is intentionally insufficient here.  A generation is usable
//! only after its referenced CAS objects have been resolved and their exact
//! identities agree.  The publisher's closed tree mutation remains an
//! independently supplied proof: this module must not infer it from a
//! signature-shaped output.

use anyhow::{Context as _, bail};
use ryeos_bundle_publication_contract::{BundleGeneration, PublisherMaterializationResult};
use ryeos_state::{
    external_content::products::accepted_result::ProductBuildAcceptedResult,
    objects::ExternalContentManifestObject,
};
use serde_json::Value;

pub mod consumer;
pub mod publisher;

pub mod catalog;

pub mod admitted_build;
pub mod producer;
pub mod recipe;
pub mod standalone_publisher;
pub mod tree;

pub mod attestation;

/// Minimal read authority needed by publication verification.
pub trait PublicationObjectReader {
    fn get_object(&self, hash: &str) -> anyhow::Result<Option<Value>>;
}

impl PublicationObjectReader for lillux::CasStore {
    fn get_object(&self, hash: &str) -> anyhow::Result<Option<Value>> {
        lillux::CasStore::get_object(self, hash)
    }
}

/// Authority that proves the publisher performed exactly its declared closed
/// mutation.  Implementations may use authenticated execution evidence or a
/// fixture-derived verifier; mere presence of signed-manifest bytes is not a
/// proof.
pub trait PublisherMaterializationProof: Send + Sync {
    fn verify_closed_mutation(
        &self,
        result: &PublisherMaterializationResult,
        input: &ExternalContentManifestObject,
        output: &ExternalContentManifestObject,
    ) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasePolicyBinding {
    pub catalog_namespace: String,
    pub bundle_publication_policy_section_digest: String,
    pub trust_epoch: u64,
}

/// Current publication-policy authority for the generation's remaining typed
/// evidence. This boundary verifies the signed manifest item, publisher
/// identity, source retention (when present), and every qualification,
/// provenance, and SBOM attestation. Keeping it injected prevents a structural
/// CAS walk from being mistaken for trust or release-policy admission.
pub trait BundleReleaseEvidenceProof: Send + Sync {
    fn verify_release_evidence(
        &self,
        generation: &BundleGeneration,
        accepted_result: &ProductBuildAcceptedResult,
        accepted_capture_result: &ProductBuildAcceptedResult,
        materialization: &PublisherMaterializationResult,
        policy_binding: &ReleasePolicyBinding,
    ) -> anyhow::Result<()>;
}

/// A materialization whose complete referenced identity and closed mutation
/// have been checked. Private fields prevent structural wire values from being
/// mistaken for admitted values.
#[derive(Debug)]
pub struct VerifiedPublisherMaterializationResult {
    result: PublisherMaterializationResult,
    input_manifest: ExternalContentManifestObject,
    output_manifest: ExternalContentManifestObject,
}

impl VerifiedPublisherMaterializationResult {
    pub fn result(&self) -> &PublisherMaterializationResult {
        &self.result
    }

    pub fn input_manifest(&self) -> &ExternalContentManifestObject {
        &self.input_manifest
    }

    pub fn output_manifest(&self) -> &ExternalContentManifestObject {
        &self.output_manifest
    }
}

/// A generation bound to its exact accepted product and verified publisher
/// materialization.
#[derive(Debug)]
pub struct VerifiedBundleGeneration {
    generation: BundleGeneration,
    accepted_result: ProductBuildAcceptedResult,
    materialization: VerifiedPublisherMaterializationResult,
}

impl VerifiedBundleGeneration {
    pub fn generation(&self) -> &BundleGeneration {
        &self.generation
    }

    pub fn accepted_result(&self) -> &ProductBuildAcceptedResult {
        &self.accepted_result
    }

    pub fn materialization(&self) -> &VerifiedPublisherMaterializationResult {
        &self.materialization
    }
}

pub fn verify_bundle_generation(
    generation: BundleGeneration,
    objects: &impl PublicationObjectReader,
    materialization_proof: &(impl PublisherMaterializationProof + ?Sized),
    release_evidence_proof: &(impl BundleReleaseEvidenceProof + ?Sized),
    policy_binding: &ReleasePolicyBinding,
) -> anyhow::Result<VerifiedBundleGeneration> {
    generation.validate()?;

    let accepted_value = read_exact(objects, &generation.accepted_product_result_hash)?;
    let accepted_result = ProductBuildAcceptedResult::from_value(&accepted_value)
        .context("bundle generation references an invalid accepted product result")?;
    let selected_product = accepted_result
        .products
        .iter()
        .find(|product| product.product_name == generation.selected_product_identity)
        .context("bundle generation selected product is absent from accepted result")?;
    if selected_product.witness_hash != generation.selected_product_witness {
        bail!("bundle generation selected product witness disagrees with accepted result");
    }

    let materialization_value =
        read_exact(objects, &generation.publisher_materialization_result_hash)?;
    let materialization =
        PublisherMaterializationResult::from_current_value(&materialization_value)
            .context("bundle generation references an invalid publisher materialization")?;
    let accepted_capture_value = read_exact(objects, &generation.accepted_capture_result_hash)?;
    let accepted_capture_result =
        ProductBuildAcceptedResult::from_value(&accepted_capture_value)
            .context("bundle generation references an invalid signed capture result")?;
    let selected_signed_product = accepted_capture_result
        .products
        .iter()
        .find(|product| product.product_name == generation.selected_signed_product_identity)
        .context("bundle generation signed product is absent from capture result")?;
    if selected_signed_product.witness_hash != generation.selected_signed_product_witness {
        bail!("bundle generation signed product witness disagrees with capture result");
    }

    if materialization.accepted_product_result_hash != generation.accepted_product_result_hash
        || materialization.selected_product_identity != generation.selected_product_identity
        || materialization.selected_product_witness != generation.selected_product_witness
    {
        bail!("bundle generation and publisher materialization disagree on selected product");
    }
    if materialization.output_content_manifest_hash != generation.content_manifest_hash
        || materialization.output_manifest_item_hash != generation.manifest_item_hash
    {
        bail!("bundle generation and publisher materialization disagree on output identity");
    }

    let input_manifest = read_manifest(objects, &materialization.input_content_manifest_hash)
        .context("publisher input manifest is invalid")?;
    let output_manifest = read_manifest(objects, &materialization.output_content_manifest_hash)
        .context("publisher output manifest is invalid")?;
    tree::validate_native_bundle_tree(&input_manifest)
        .context("publisher input is not a native bundle tree")?;
    tree::validate_native_bundle_tree(&output_manifest)
        .context("publisher output is not a native bundle tree")?;
    materialization_proof
        .verify_closed_mutation(&materialization, &input_manifest, &output_manifest)
        .context("publisher closed mutation is unproven")?;
    release_evidence_proof
        .verify_release_evidence(
            &generation,
            &accepted_result,
            &accepted_capture_result,
            &materialization,
            policy_binding,
        )
        .context("bundle release evidence is unproven")?;

    Ok(VerifiedBundleGeneration {
        generation,
        accepted_result,
        materialization: VerifiedPublisherMaterializationResult {
            result: materialization,
            input_manifest,
            output_manifest,
        },
    })
}

/// Resolve and verify one exact generation by CAS identity. Channel names and
/// mutable refs deliberately cannot enter this API.
pub fn inspect_bundle_generation(
    generation_hash: &str,
    objects: &impl PublicationObjectReader,
    materialization_proof: &(impl PublisherMaterializationProof + ?Sized),
    release_evidence_proof: &(impl BundleReleaseEvidenceProof + ?Sized),
    policy_binding: &ReleasePolicyBinding,
) -> anyhow::Result<VerifiedBundleGeneration> {
    let value = read_exact(objects, generation_hash)?;
    let generation = BundleGeneration::from_current_value(&value)
        .context("requested object is not a current bundle generation")?;
    verify_bundle_generation(
        generation,
        objects,
        materialization_proof,
        release_evidence_proof,
        policy_binding,
    )
}

/// Finalize a generation only after contextual verification succeeds. This is
/// the local, non-signing boundary: publisher authorization remains a separate
/// attestation operation.
pub fn finalize_bundle_generation(
    candidate: BundleGeneration,
    cas: &lillux::CasStore,
    materialization_proof: &(impl PublisherMaterializationProof + ?Sized),
    release_evidence_proof: &(impl BundleReleaseEvidenceProof + ?Sized),
    policy_binding: &ReleasePolicyBinding,
) -> anyhow::Result<String> {
    let verified = verify_bundle_generation(
        candidate,
        cas,
        materialization_proof,
        release_evidence_proof,
        policy_binding,
    )?;
    let value = verified.generation.to_value()?;
    let expected_hash = verified.generation.content_hash()?;
    let stored = cas.put_object(&value)?;
    if stored.hash != expected_hash {
        bail!("stored bundle generation disagrees with its canonical identity");
    }
    Ok(stored.hash)
}

pub(super) fn read_exact(
    objects: &impl PublicationObjectReader,
    expected_hash: &str,
) -> anyhow::Result<Value> {
    let value = objects
        .get_object(expected_hash)?
        .with_context(|| format!("publication object {expected_hash} is absent"))?;
    let actual_hash = lillux::cas::sha256_hex(lillux::canonical_json(&value)?.as_bytes());
    if actual_hash != expected_hash {
        bail!("publication object content disagrees with requested CAS identity");
    }
    Ok(value)
}

fn read_manifest(
    objects: &impl PublicationObjectReader,
    hash: &str,
) -> anyhow::Result<ExternalContentManifestObject> {
    ExternalContentManifestObject::from_value(&read_exact(objects, hash)?)
}

#[cfg(test)]
mod tests;
