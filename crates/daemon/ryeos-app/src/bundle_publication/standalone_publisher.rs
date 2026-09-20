//! Standalone, policy-pinned authorities for the constrained publisher.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_state::{
    external_content::products::qualification::{
        ProductQualificationEvidence, ProductQualificationPolicySource,
    },
    objects::{AdmittedLaunchArtifactIdentity, Attestation, ExternalContentManifestObject},
};
use serde::{Deserialize, Serialize};

use super::{
    BundleReleaseEvidenceProof, PublisherMaterializationProof, ReleasePolicyBinding,
    publisher::{ConstrainedBundleTreePublisher, SignedBundleTree, VerifiedPublisherCandidate},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandalonePublisherPolicy {
    pub schema: String,
    pub catalog_namespace: String,
    pub bundle_publication_policy_section_digest: String,
    pub trust_epoch: u64,
    pub qualification_signer_public_key: [u8; 32],
    pub qualification_signer_fingerprint: String,
    pub qualification_policy: ProductQualificationPolicySource,
    pub qualification_verifier_effective_definition_digest: String,
    pub qualification_verifier_artifact_identity: AdmittedLaunchArtifactIdentity,
    pub required_qualification_claims: Vec<String>,
    pub publisher_tool_effective_definition_digest: String,
    pub publisher_tool_artifact_identity_hash: String,
}

impl StandalonePublisherPolicy {
    /// Derive publisher pins from the exact same typed section installed on
    /// release, source and consumer nodes. This does not discover or authorize
    /// identities: the operator supplies the measured section explicitly.
    pub fn from_node_policy(
        section: &crate::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        namespace: &str,
    ) -> anyhow::Result<Self> {
        section.validate()?;
        let catalog = section.require_catalog(namespace)?;
        anyhow::ensure!(!catalog.frozen, "catalog is frozen");
        let policy = Self {
            schema: "ryeos.standalone_bundle_publisher_policy.v1".into(),
            catalog_namespace: catalog.namespace.clone(),
            bundle_publication_policy_section_digest: section.section_digest()?,
            trust_epoch: catalog.trust_epoch,
            qualification_signer_public_key: catalog.qualification_signer_public_key,
            qualification_signer_fingerprint: catalog.qualification_signer_fingerprint.clone(),
            qualification_policy: catalog.qualification_policy.clone(),
            qualification_verifier_effective_definition_digest: catalog
                .qualification_verifier_effective_definition_digest
                .clone(),
            qualification_verifier_artifact_identity: catalog
                .qualification_verifier_artifact_identity
                .clone(),
            required_qualification_claims: catalog.required_qualification_claims.clone(),
            publisher_tool_effective_definition_digest: catalog
                .publisher_tool_effective_definition_digest
                .clone(),
            publisher_tool_artifact_identity_hash: catalog
                .publisher_tool_artifact_identity_hash
                .clone(),
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == "ryeos.standalone_bundle_publisher_policy.v1",
            "unsupported standalone publisher policy schema"
        );
        require_hash(
            &self.bundle_publication_policy_section_digest,
            "bundle publication policy",
        )?;
        require_hash(
            &self.qualification_verifier_effective_definition_digest,
            "qualification verifier definition",
        )?;
        require_hash(
            &self.publisher_tool_effective_definition_digest,
            "publisher tool definition",
        )?;
        require_hash(
            &self.publisher_tool_artifact_identity_hash,
            "publisher tool artifact",
        )?;
        anyhow::ensure!(
            !self.catalog_namespace.is_empty() && self.catalog_namespace.len() <= 256,
            "invalid catalog namespace"
        );
        anyhow::ensure!(self.trust_epoch > 0, "trust epoch must be nonzero");
        self.qualification_policy.validate()?;
        let qualification_key =
            lillux::crypto::VerifyingKey::from_bytes(&self.qualification_signer_public_key)?;
        anyhow::ensure!(
            lillux::crypto::fingerprint(&qualification_key)
                == self.qualification_signer_fingerprint,
            "qualification signer key and fingerprint disagree"
        );
        self.qualification_verifier_artifact_identity.validate()?;
        anyhow::ensure!(
            !self.required_qualification_claims.is_empty(),
            "qualification claims must not be empty"
        );
        Ok(())
    }
}

pub struct StandalonePublisherProof {
    cas: Arc<lillux::CasStore>,
    policy: StandalonePublisherPolicy,
    qualification_key: lillux::crypto::VerifyingKey,
}

impl StandalonePublisherProof {
    pub fn new(
        cas: Arc<lillux::CasStore>,
        policy: StandalonePublisherPolicy,
    ) -> anyhow::Result<Self> {
        policy.validate()?;
        let qualification_key =
            lillux::crypto::VerifyingKey::from_bytes(&policy.qualification_signer_public_key)?;
        Ok(Self {
            cas,
            policy,
            qualification_key,
        })
    }
}

impl PublisherMaterializationProof for StandalonePublisherProof {
    fn verify_closed_mutation(
        &self,
        result: &ryeos_bundle_publication_contract::PublisherMaterializationResult,
        input: &ExternalContentManifestObject,
        output: &ExternalContentManifestObject,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            result.publisher_tool_effective_definition_digest
                == self.policy.publisher_tool_effective_definition_digest
                && result.publisher_tool_artifact_identity_hash
                    == self.policy.publisher_tool_artifact_identity_hash,
            "publisher tool identity violates pinned policy"
        );
        let mut before = input.entries.clone();
        let mut after = output.entries.clone();
        let signed = after
            .iter()
            .find(|entry| entry.path == ".ai/manifest.yaml")
            .context("signed tree omits bundle manifest")?;
        anyhow::ensure!(
            signed.blob_hash.as_deref() == Some(&result.output_manifest_item_hash),
            "signed manifest identity disagrees with materialization"
        );
        before.retain(|entry| entry.path != ".ai/manifest.yaml");
        after.retain(|entry| entry.path != ".ai/manifest.yaml");
        anyhow::ensure!(
            before == after,
            "publisher changed content outside bundle manifest"
        );
        Ok(())
    }
}

impl BundleReleaseEvidenceProof for StandalonePublisherProof {
    fn verify_release_evidence(
        &self,
        generation: &ryeos_bundle_publication_contract::BundleGeneration,
        _accepted: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        accepted_capture: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        materialization: &ryeos_bundle_publication_contract::PublisherMaterializationResult,
        binding: &ReleasePolicyBinding,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            binding.catalog_namespace == self.policy.catalog_namespace
                && binding.bundle_publication_policy_section_digest
                    == self.policy.bundle_publication_policy_section_digest
                && binding.trust_epoch == self.policy.trust_epoch,
            "release policy binding is not the pinned standalone policy"
        );
        anyhow::ensure!(
            generation.provenance_hash.is_none() && generation.sbom_hash.is_none(),
            "unsupported release evidence is present"
        );
        anyhow::ensure!(
            generation.qualification_evidence_hashes.len() == 1,
            "exactly one qualification attestation is required"
        );
        anyhow::ensure!(
            materialization.publisher_tool_effective_definition_digest
                == self.policy.publisher_tool_effective_definition_digest
                && materialization.publisher_tool_artifact_identity_hash
                    == self.policy.publisher_tool_artifact_identity_hash,
            "publisher tool identity violates pinned policy"
        );
        let hash = &generation.qualification_evidence_hashes[0];
        let selected = accepted_capture
            .products
            .iter()
            .find(|product| product.product_name == generation.selected_signed_product_identity)
            .context("capture result omits selected signed generation product")?;
        anyhow::ensure!(
            selected.witness_hash == generation.selected_signed_product_witness,
            "generation qualification subject is not the captured signed product"
        );
        let value = self
            .cas
            .get_object(hash)?
            .context("qualification attestation is absent")?;
        anyhow::ensure!(
            lillux::cas::sha256_hex(lillux::canonical_json(&value)?.as_bytes()) == *hash,
            "qualification CAS identity mismatch"
        );
        let attestation = Attestation::from_value(&value)?;
        attestation.verify_with_key(&self.qualification_key)?;
        anyhow::ensure!(
            !attestation.is_expired_at(&lillux::time::iso8601_now())?,
            "qualification attestation is expired"
        );
        let evidence = ProductQualificationEvidence::from_attestation(&attestation)?;
        anyhow::ensure!(
            attestation.subject_hash == generation.content_manifest_hash,
            "qualification subject is not the released tree"
        );
        anyhow::ensure!(
            evidence.product_witness_hash == generation.selected_signed_product_witness,
            "qualification product witness differs from generation"
        );
        anyhow::ensure!(
            evidence.product_coordinate.owner_principal == accepted_capture.owner_principal,
            "qualification belongs to another product owner"
        );
        evidence.validate_current_policy(
            &self.policy.qualification_policy,
            &self
                .policy
                .qualification_verifier_effective_definition_digest,
            &self.policy.required_qualification_claims,
        )?;
        evidence.validate_current_artifact(&self.policy.qualification_verifier_artifact_identity)
    }
}

pub struct ManifestOnlyTreePublisher {
    cas: Arc<lillux::CasStore>,
    identity: crate::identity::NodeIdentity,
    policy: StandalonePublisherPolicy,
}

impl ManifestOnlyTreePublisher {
    pub fn new(
        cas: Arc<lillux::CasStore>,
        identity: crate::identity::NodeIdentity,
        policy: StandalonePublisherPolicy,
    ) -> anyhow::Result<Self> {
        policy.validate()?;
        Ok(Self {
            cas,
            identity,
            policy,
        })
    }
}

impl ConstrainedBundleTreePublisher for ManifestOnlyTreePublisher {
    fn authorize_build_recipe(
        &self,
        request: &super::recipe::AuthorizeBuildRecipeRequest,
    ) -> anyhow::Result<serde_json::Value> {
        request.require_policy(
            &self.policy.catalog_namespace,
            &self.policy.bundle_publication_policy_section_digest,
            self.policy.trust_epoch,
        )?;
        let body = request.config_body()?;
        let signed = lillux::signature::sign_content(&body, self.identity.signing_key(), "#", None);
        let blob = self.cas.put_blob(signed.as_bytes())?;
        Ok(serde_json::json!({
            "schema": "ryeos.bundle_build_recipe_authorization.v1",
            "canonical_ref": "config:bundle-release/native-build-products",
            "publisher_fingerprint": self.identity.fingerprint(),
            "body_hash": lillux::signature::content_hash(&body),
            "signed_blob_hash": blob.hash,
            "signed_config": signed
        }))
    }

    fn authorize_capture_recipe(
        &self,
        request: &super::recipe::AuthorizeCaptureRecipeRequest,
    ) -> anyhow::Result<serde_json::Value> {
        request.require_policy(
            &self.policy.catalog_namespace,
            &self.policy.bundle_publication_policy_section_digest,
            self.policy.trust_epoch,
        )?;
        let body = request.config_body()?;
        let signed = lillux::signature::sign_content(&body, self.identity.signing_key(), "#", None);
        let blob = self.cas.put_blob(signed.as_bytes())?;
        Ok(serde_json::json!({
            "schema": "ryeos.bundle_capture_recipe_authorization.v1",
            "canonical_ref": super::recipe::CAPTURE_RECIPE_REF,
            "publisher_fingerprint": self.identity.fingerprint(),
            "body_hash": lillux::signature::content_hash(&body),
            "signed_blob_hash": blob.hash,
            "signed_config": signed
        }))
    }

    fn materialize_and_sign(
        &self,
        candidate: &VerifiedPublisherCandidate,
    ) -> anyhow::Result<SignedBundleTree> {
        let mut output = candidate.input_manifest().clone();
        let entry = output
            .entries
            .iter_mut()
            .find(|entry| entry.path == ".ai/manifest.yaml")
            .context("bundle candidate omits .ai/manifest.yaml")?;
        let input_hash = entry
            .blob_hash
            .as_deref()
            .context("bundle manifest has no blob")?;
        let bytes = self
            .cas
            .get_blob(input_hash)?
            .context("bundle manifest blob is absent")?;
        let body = std::str::from_utf8(&bytes).context("bundle manifest is not UTF-8")?;
        let signed = lillux::signature::sign_content(body, self.identity.signing_key(), "#", None);
        let stored = self.cas.put_blob(signed.as_bytes())?;
        let old_size = entry.size.context("bundle manifest has no size")?;
        entry.blob_hash = Some(stored.hash.clone());
        entry.size = Some(signed.len() as u64);
        output.total_bytes = output
            .total_bytes
            .checked_sub(old_size)
            .and_then(|v| v.checked_add(signed.len() as u64))
            .context("signed tree size overflow")?;
        super::tree::validate_native_bundle_tree(&output)?;
        let manifest_hash = self.cas.put_object(&serde_json::to_value(&output)?)?.hash;
        Ok(SignedBundleTree {
            output_content_manifest_hash: manifest_hash,
            output_manifest_item_hash: stored.hash,
            publisher_fingerprint: self.identity.fingerprint().to_owned(),
            publisher_tool_effective_definition_digest: self
                .policy
                .publisher_tool_effective_definition_digest
                .clone(),
            publisher_tool_artifact_identity_hash: self
                .policy
                .publisher_tool_artifact_identity_hash
                .clone(),
        })
    }
}

fn require_hash(value: &str, label: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be lowercase 64-hex");
    }
    Ok(())
}
