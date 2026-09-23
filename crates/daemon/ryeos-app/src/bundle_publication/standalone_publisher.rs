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
use sha2::{Digest as _, Sha256};

use super::{
    BundleReleaseEvidenceProof, PublisherMaterializationProof, ReleasePolicyBinding,
    publisher::{ConstrainedBundleTreePublisher, SignedBundleTree, VerifiedPublisherCandidate},
};

/// Closed identity of the standalone publisher operation surface. The
/// executable artifact is measured separately so a rebuild cannot retain the
/// authority of another binary merely by repeating this definition.
const PUBLISHER_TOOL_DEFINITION_V1: &[u8] = b"ryeos.standalone-bundle-publisher.v1\nPOST /v1/bundle-recipe/authorize-build\nPOST /v1/bundle-recipe/authorize-capture\nPOST /v1/substrate-core/authorize-recipe\nPOST /v1/substrate-build/authorize-recipe\nPOST /v1/bundle-tree/sign\nPOST /v1/bundle-generation/authorize\nPOST /v1/substrate-release/authorize\nPOST /v1/bundle-catalog/authorize-successor\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedPublisherToolIdentity {
    pub schema: String,
    pub effective_definition_digest: String,
    pub artifact_identity_hash: String,
}

impl ObservedPublisherToolIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == "ryeos.observed_bundle_publisher_tool.v1",
            "unsupported observed publisher tool schema"
        );
        require_hash(
            &self.effective_definition_digest,
            "observed publisher tool definition",
        )?;
        require_hash(
            &self.artifact_identity_hash,
            "observed publisher tool artifact",
        )
    }
}

pub fn observe_publisher_tool(
    executable: &std::path::Path,
) -> anyhow::Result<ObservedPublisherToolIdentity> {
    let pinned = lillux::open_pinned_regular_file_no_follow(executable)
        .with_context(|| format!("pin publisher executable {}", executable.display()))?;
    let observation = pinned.observation()?;
    anyhow::ensure!(
        observation.size() > 0 && observation.size() <= 256 * 1024 * 1024,
        "publisher executable is empty or exceeds the measurement bound"
    );
    let artifact_identity_hash = pinned.digest_stable_exact(&observation)?;
    Ok(ObservedPublisherToolIdentity {
        schema: "ryeos.observed_bundle_publisher_tool.v1".to_owned(),
        effective_definition_digest: format!("{:x}", Sha256::digest(PUBLISHER_TOOL_DEFINITION_V1)),
        artifact_identity_hash,
    })
}

/// Measure the inode that is actually executing, not a pathname that could be
/// replaced after process start.
pub fn observe_current_publisher_tool() -> anyhow::Result<ObservedPublisherToolIdentity> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;
        let executable = std::fs::File::open("/proc/self/exe")
            .context("open current publisher executable descriptor")?;
        lillux::validate_current_executable_descriptor(executable.as_raw_fd() as u32)
            .map_err(anyhow::Error::msg)?;
        let metadata = executable.metadata()?;
        anyhow::ensure!(
            metadata.len() > 0 && metadata.len() <= 256 * 1024 * 1024,
            "running publisher executable is empty or exceeds the measurement bound"
        );
        let (artifact_identity_hash, after) =
            lillux::digest_open_regular_file_stable_exact(&executable, metadata.len())?;
        anyhow::ensure!(
            metadata.len() == after.len(),
            "running publisher executable changed while it was measured"
        );
        return Ok(ObservedPublisherToolIdentity {
            schema: "ryeos.observed_bundle_publisher_tool.v1".to_owned(),
            effective_definition_digest: format!(
                "{:x}",
                Sha256::digest(PUBLISHER_TOOL_DEFINITION_V1)
            ),
            artifact_identity_hash,
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        observe_publisher_tool(&std::env::current_exe()?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandalonePublisherPolicy {
    pub schema: String,
    pub catalog_namespace: String,
    pub catalog_publisher_fingerprint: String,
    pub bundle_publication_policy_section_digest: String,
    pub trust_epoch: u64,
    pub qualification_signer_public_key: [u8; 32],
    pub qualification_signer_fingerprint: String,
    pub qualification_policy: ProductQualificationPolicySource,
    pub qualification_verifier_effective_definition_digest: String,
    pub qualification_verifier_artifact_identity: AdmittedLaunchArtifactIdentity,
    pub required_qualification_claims: Vec<String>,
    pub substrate_qualification_policy: ProductQualificationPolicySource,
    pub substrate_qualification_verifier_effective_definition_digest: String,
    pub substrate_qualification_verifier_artifact_identity: AdmittedLaunchArtifactIdentity,
    pub required_substrate_qualification_claims: Vec<String>,
    pub core_seed_qualification_policy: ProductQualificationPolicySource,
    pub core_seed_qualification_verifier_effective_definition_digest: String,
    pub core_seed_qualification_verifier_artifact_identity: AdmittedLaunchArtifactIdentity,
    pub required_core_seed_qualification_claims: Vec<String>,
    pub substrate_build_signer_public_key: [u8; 32],
    pub substrate_build_signer_fingerprint: String,
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
            core_seed_qualification_policy: catalog.core_seed_qualification_policy.clone(),
            core_seed_qualification_verifier_effective_definition_digest: catalog
                .core_seed_qualification_verifier_effective_definition_digest
                .clone(),
            core_seed_qualification_verifier_artifact_identity: catalog
                .core_seed_qualification_verifier_artifact_identity
                .clone(),
            required_core_seed_qualification_claims: catalog
                .required_core_seed_qualification_claims
                .clone(),
            catalog_namespace: catalog.namespace.clone(),
            catalog_publisher_fingerprint: catalog.publisher_fingerprint.clone(),
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
            substrate_qualification_policy: catalog.substrate_qualification_policy.clone(),
            substrate_qualification_verifier_effective_definition_digest: catalog
                .substrate_qualification_verifier_effective_definition_digest
                .clone(),
            substrate_qualification_verifier_artifact_identity: catalog
                .substrate_qualification_verifier_artifact_identity
                .clone(),
            required_substrate_qualification_claims: catalog
                .required_substrate_qualification_claims
                .clone(),
            substrate_build_signer_public_key: catalog.substrate_build_signer_public_key,
            substrate_build_signer_fingerprint: catalog.substrate_build_signer_fingerprint.clone(),
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
            &self.catalog_publisher_fingerprint,
            "catalog publisher fingerprint",
        )?;
        require_hash(
            &self.qualification_verifier_effective_definition_digest,
            "qualification verifier definition",
        )?;
        require_hash(
            &self.substrate_qualification_verifier_effective_definition_digest,
            "substrate qualification verifier definition",
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
        self.core_seed_qualification_policy.validate()?;
        self.core_seed_qualification_verifier_artifact_identity
            .validate()?;
        require_hash(
            &self.core_seed_qualification_verifier_effective_definition_digest,
            "Core seed verifier definition",
        )?;
        anyhow::ensure!(
            self.core_seed_qualification_policy.canonical_ref
                == super::core_seed::QUALIFICATION_POLICY
                && self.core_seed_qualification_policy.policy.verifier_ref
                    == super::core_seed::QUALIFIER
                && self
                    .core_seed_qualification_policy
                    .policy
                    .verifier_parameters
                    == serde_json::json!({})
                && self.required_core_seed_qualification_claims
                    == vec![super::core_seed::QUALIFICATION_CLAIM.to_owned()],
            "invalid Core seed qualification policy pins"
        );
        self.substrate_qualification_policy.validate()?;
        require_distinct_qualification_verifiers(
            &self.qualification_policy.policy.verifier_ref,
            &self.core_seed_qualification_policy.policy.verifier_ref,
            &self.substrate_qualification_policy.policy.verifier_ref,
        )?;
        let qualification_key =
            lillux::crypto::VerifyingKey::from_bytes(&self.qualification_signer_public_key)?;
        anyhow::ensure!(
            lillux::crypto::fingerprint(&qualification_key)
                == self.qualification_signer_fingerprint,
            "qualification signer key and fingerprint disagree"
        );
        let substrate_build_key =
            lillux::crypto::VerifyingKey::from_bytes(&self.substrate_build_signer_public_key)?;
        anyhow::ensure!(
            lillux::crypto::fingerprint(&substrate_build_key)
                == self.substrate_build_signer_fingerprint,
            "substrate build signer key and fingerprint disagree"
        );
        self.qualification_verifier_artifact_identity.validate()?;
        self.substrate_qualification_verifier_artifact_identity
            .validate()?;
        anyhow::ensure!(
            !self.required_qualification_claims.is_empty(),
            "qualification claims must not be empty"
        );
        anyhow::ensure!(
            !self.required_substrate_qualification_claims.is_empty()
                && self
                    .required_substrate_qualification_claims
                    .windows(2)
                    .all(|pair| pair[0] < pair[1]),
            "substrate qualification claims must be sorted, unique, and non-empty"
        );
        anyhow::ensure!(
            self.required_substrate_qualification_claims == ["substrate_release_checks_v1"],
            "substrate qualification requires the closed substrate release claim"
        );
        Ok(())
    }

    pub fn require_observed_tool(
        &self,
        observed: &ObservedPublisherToolIdentity,
    ) -> anyhow::Result<()> {
        observed.validate()?;
        anyhow::ensure!(
            self.publisher_tool_effective_definition_digest == observed.effective_definition_digest
                && self.publisher_tool_artifact_identity_hash == observed.artifact_identity_hash,
            "observed publisher tool identity violates pinned policy"
        );
        Ok(())
    }
}

fn require_distinct_qualification_verifiers(
    native: &str,
    core_seed: &str,
    substrate: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        native != core_seed && native != substrate && core_seed != substrate,
        "native, Core seed, and substrate qualification policies must use pairwise-distinct verifiers"
    );
    Ok(())
}

#[cfg(test)]
mod policy_invariant_tests {
    use super::require_distinct_qualification_verifiers;

    #[test]
    fn qualification_verifiers_are_pairwise_distinct() {
        assert!(require_distinct_qualification_verifiers("native", "core", "substrate").is_ok());
        assert!(require_distinct_qualification_verifiers("same", "same", "substrate").is_err());
        assert!(require_distinct_qualification_verifiers("same", "core", "same").is_err());
        assert!(require_distinct_qualification_verifiers("native", "same", "same").is_err());
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
    fn verify_substrate_release_evidence(
        &self,
        release: &ryeos_bundle_publication_contract::SubstrateRelease,
        accepted: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        binding: &ReleasePolicyBinding,
    ) -> anyhow::Result<()> {
        release.validate()?;
        accepted.validate()?;
        anyhow::ensure!(
            binding.catalog_namespace == self.policy.catalog_namespace
                && binding.bundle_publication_policy_section_digest
                    == self.policy.bundle_publication_policy_section_digest
                && binding.trust_epoch == self.policy.trust_epoch
                && release.catalog_namespace == binding.catalog_namespace
                && release.bundle_publication_policy_section_digest
                    == binding.bundle_publication_policy_section_digest
                && release.trust_epoch == binding.trust_epoch,
            "substrate release violates pinned publication policy"
        );
        anyhow::ensure!(
            accepted.content_hash()? == release.substrate_build_accepted_result_hash,
            "substrate release accepted build identity changed"
        );
        let selected = accepted
            .products
            .iter()
            .find(|product| product.product_name == release.selected_substrate_product_identity)
            .context("substrate accepted build omits selected product")?;
        anyhow::ensure!(
            selected.witness_hash == release.selected_substrate_product_witness,
            "substrate release witness differs from accepted build"
        );
        let witness = Attestation::from_value(&super::read_exact(
            self.cas.as_ref(),
            &selected.witness_hash,
        )?)?;
        let build_key = lillux::crypto::VerifyingKey::from_bytes(
            &self.policy.substrate_build_signer_public_key,
        )?;
        anyhow::ensure!(
            lillux::crypto::fingerprint(&build_key)
                == self.policy.substrate_build_signer_fingerprint,
            "substrate build signer key and fingerprint disagree"
        );
        let evidence = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
            &witness, &build_key, &accepted.owner_principal,
        )?;
        evidence.recipe_purpose.require_bundle_release()?;
        anyhow::ensure!(
            !witness.is_expired_at(&lillux::time::iso8601_now())?,
            "substrate build witness expired"
        );
        anyhow::ensure!(
            accepted.producer_ref == "graph:ryeos/bundle-release/substrate-build"
                && evidence.recipe_ref == "config:bundle-release/substrate-build-products"
                && evidence.root_producer.canonical_ref == accepted.producer_ref
                && evidence.root_producer.producer_project_snapshot_hash
                    == accepted.producer_project_snapshot_hash
                && evidence.root_producer.effective_definition_digest
                    == accepted.producer_effective_definition_digest
                && evidence.root_producer.admitted_parameters_digest
                    == accepted.producer_parameters_digest
                && evidence.producer_partition_identity.as_deref()
                    == Some(accepted.producer_partition_identity.as_str())
                && evidence.declaration.name == release.selected_substrate_product_identity,
            "substrate accepted build does not match authenticated substrate producer testimony"
        );
        let manifest = ExternalContentManifestObject::from_value(&super::read_exact(
            self.cas.as_ref(),
            &evidence.manifest_hash,
        )?)?;
        validate_substrate_receipt_tree(&manifest)?;
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == ".ai/substrate-release.json")
            .context("substrate product omits measured image receipt")?;
        anyhow::ensure!(
            entry.blob_hash.as_deref() == Some(release.substrate_build_receipt_hash.as_str()),
            "substrate receipt is not the authenticated product member"
        );
        anyhow::ensure!(
            entry.size.is_some_and(|size| size > 0 && size <= 64 * 1024),
            "substrate receipt exceeds its bounded product size"
        );
        let bytes = self
            .cas
            .get_blob(&release.substrate_build_receipt_hash)?
            .context("substrate receipt blob absent")?;
        anyhow::ensure!(
            bytes.len() <= 64 * 1024
                && entry.size == Some(bytes.len() as u64)
                && lillux::sha256_hex(&bytes) == release.substrate_build_receipt_hash,
            "substrate receipt bytes or size differ from captured member"
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let receipt =
            ryeos_bundle_publication_contract::SubstrateBuildReceipt::from_current_value(&value)?;
        let retained_receipt =
            super::read_exact(self.cas.as_ref(), &release.substrate_build_receipt_hash)?;
        anyhow::ensure!(
            retained_receipt == value,
            "substrate receipt object closure disagrees with captured bytes"
        );
        anyhow::ensure!(
            receipt.content_hash()? == release.substrate_build_receipt_hash,
            "substrate receipt must use its exact canonical object bytes"
        );
        anyhow::ensure!(
            receipt.substrate_image_digest == release.substrate_image_digest
                && receipt.substrate_protocol == release.substrate_protocol
                && receipt.target == release.target
                && receipt.core_generation_hash == release.core_generation_hash,
            "substrate release contradicts authenticated measured build receipt"
        );
        anyhow::ensure!(
            release.qualification_evidence_hashes.len() == 1,
            "substrate release requires exactly one independent qualification"
        );
        let qualification = Attestation::from_value(&super::read_exact(
            self.cas.as_ref(),
            &release.qualification_evidence_hashes[0],
        )?)?;
        qualification.verify_with_key(&self.qualification_key)?;
        anyhow::ensure!(
            !qualification.is_expired_at(&lillux::time::iso8601_now())?,
            "substrate qualification expired"
        );
        let qualified = ProductQualificationEvidence::from_attestation(&qualification)?;
        anyhow::ensure!(
            qualification.subject_hash == evidence.manifest_hash
                && qualified.product_witness_hash == release.selected_substrate_product_witness
                && qualified.product_coordinate.owner_principal == accepted.owner_principal,
            "substrate qualification attests another build product"
        );
        qualified.validate_current_policy(
            &self.policy.substrate_qualification_policy,
            &self
                .policy
                .substrate_qualification_verifier_effective_definition_digest,
            &self.policy.required_substrate_qualification_claims,
        )?;
        qualified.validate_current_artifact(
            &self
                .policy
                .substrate_qualification_verifier_artifact_identity,
        )?;
        Ok(())
    }

    fn verify_release_evidence(
        &self,
        generation: &ryeos_bundle_publication_contract::BundleGeneration,
        accepted: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
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
        let build_key = lillux::crypto::VerifyingKey::from_bytes(
            &self.policy.substrate_build_signer_public_key,
        )?;
        for (result, witness_hash) in [
            (accepted, &generation.selected_product_witness),
            (
                accepted_capture,
                &generation.selected_signed_product_witness,
            ),
        ] {
            let witness =
                Attestation::from_value(&super::read_exact(self.cas.as_ref(), witness_hash)?)?;
            let product = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
                &witness,
                &build_key,
                &result.owner_principal,
            )?;
            product.recipe_purpose.require_bundle_release()?;
        }
        if generation.bundle_name == "core" {
            anyhow::ensure!(
                accepted.producer_ref == super::core_seed::BUILD_GRAPH
                    && accepted_capture.producer_ref == super::core_seed::CAPTURE_GRAPH
                    && accepted.owner_principal == accepted_capture.owner_principal
                    && generation.selected_product_identity == "core_seed"
                    && generation.selected_signed_product_identity == "signed_core_seed",
                "Core generation requires dedicated substrate Core seed producer and capture"
            );
            for (result, witness_hash, recipe_ref, manifest_hash, product_name) in [
                (
                    accepted,
                    &generation.selected_product_witness,
                    super::core_seed::BUILD_RECIPE,
                    &materialization.input_content_manifest_hash,
                    "core_seed",
                ),
                (
                    accepted_capture,
                    &generation.selected_signed_product_witness,
                    super::core_seed::CAPTURE_RECIPE,
                    &materialization.output_content_manifest_hash,
                    "signed_core_seed",
                ),
            ] {
                let witness =
                    Attestation::from_value(&super::read_exact(self.cas.as_ref(), witness_hash)?)?;
                let product = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
                    &witness, &build_key, &result.owner_principal,
                )?;
                anyhow::ensure!(
                    !witness.is_expired_at(&lillux::time::iso8601_now())?,
                    "Core seed product witness expired"
                );
                anyhow::ensure!(
                    product.recipe_ref == recipe_ref
                        && product.manifest_hash == *manifest_hash
                        && product.declaration.name == product_name
                        && product.root_producer.canonical_ref == result.producer_ref
                        && product.root_producer.producer_project_snapshot_hash
                            == result.producer_project_snapshot_hash
                        && product.root_producer.effective_definition_digest
                            == result.producer_effective_definition_digest
                        && product.root_producer.admitted_parameters_digest
                            == result.producer_parameters_digest
                        && product.producer_partition_identity.as_deref()
                            == Some(result.producer_partition_identity.as_str()),
                    "Core seed accepted product contradicts authenticated capture evidence"
                );
                let relationship = product
                    .relationships
                    .relationships
                    .iter()
                    .find(|relationship| {
                        relationship.producer.canonical_ref == result.producer_ref
                            && relationship.producer.product_name == product_name
                    })
                    .context("Core seed witness omits its exact producer relationship")?;
                relationship.validate_product_evidence(&product)?;
                let input = super::admitted_build::AdmittedReleaseInput::from_core_seed_value(
                    relationship
                        .producer
                        .parameters
                        .get("release_input")
                        .context("Core seed producer omitted release input")?,
                )?;
                anyhow::ensure!(
                    generation.source_snapshot_hash.as_deref()
                        == Some(input.source_snapshot_hash.as_str())
                        && generation.authored_version == input.authored_manifest.version
                        && generation.target == input.target,
                    "Core generation metadata differs from authenticated Core seed input"
                );
                if recipe_ref == super::core_seed::CAPTURE_RECIPE {
                    anyhow::ensure!(
                        relationship.producer.parameters["materialization_result_hash"]
                            == generation.publisher_materialization_result_hash
                            && relationship.producer.parameters["signed_tree_manifest_hash"]
                                == generation.content_manifest_hash
                            && relationship.producer.parameters["manifest_item_hash"]
                                == generation.manifest_item_hash,
                        "Core seed capture is not bound to the released publisher transformation"
                    );
                }
            }
            evidence.validate_current_policy(
                &self.policy.core_seed_qualification_policy,
                &self
                    .policy
                    .core_seed_qualification_verifier_effective_definition_digest,
                &self.policy.required_core_seed_qualification_claims,
            )?;
            evidence.validate_current_artifact(
                &self
                    .policy
                    .core_seed_qualification_verifier_artifact_identity,
            )
        } else {
            evidence.validate_current_policy(
                &self.policy.qualification_policy,
                &self
                    .policy
                    .qualification_verifier_effective_definition_digest,
                &self.policy.required_qualification_claims,
            )?;
            evidence
                .validate_current_artifact(&self.policy.qualification_verifier_artifact_identity)
        }
    }
}

fn validate_substrate_receipt_tree(manifest: &ExternalContentManifestObject) -> anyhow::Result<()> {
    use ryeos_state::objects::ExternalContentManifestEntryKind;
    manifest.validate()?;
    let mut receipts = 0;
    for entry in &manifest.entries {
        match (entry.path.as_str(), &entry.kind) {
            (".ai", ExternalContentManifestEntryKind::Dir) => {}
            (".ai/substrate-release.json", ExternalContentManifestEntryKind::File) => {
                anyhow::ensure!(
                    entry.mode == Some(0o644),
                    "substrate receipt must be a non-executable regular file"
                );
                receipts += 1;
            }
            _ => anyhow::bail!("substrate receipt product contains an undeclared entry"),
        }
    }
    anyhow::ensure!(
        receipts == 1,
        "substrate product requires exactly one canonical receipt"
    );
    Ok(())
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
    fn authorize_substrate_build_recipe(
        &self,
        request: &super::recipe::AuthorizeSubstrateBuildRecipeRequest,
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
            "schema":"ryeos.substrate_build_recipe_authorization.v1",
            "canonical_ref":super::recipe::SUBSTRATE_BUILD_RECIPE_REF,
            "publisher_fingerprint":self.identity.fingerprint(),
            "body_hash":lillux::signature::content_hash(&body),
            "signed_blob_hash":blob.hash,
            "signed_config":signed
        }))
    }
    fn authorize_core_seed_recipe(
        &self,
        request: &super::core_seed::CoreSeedRecipeRequest,
    ) -> anyhow::Result<serde_json::Value> {
        request.require_policy(
            &self.policy.catalog_namespace,
            &self.policy.bundle_publication_policy_section_digest,
            self.policy.trust_epoch,
        )?;
        let body = request.config_body()?;
        let signed = lillux::signature::sign_content(&body, self.identity.signing_key(), "#", None);
        let blob = self.cas.put_blob(signed.as_bytes())?;
        Ok(
            serde_json::json!({"schema":"ryeos.core_seed_recipe_authorization.v1",
            "canonical_ref":request.canonical_ref(),"publisher_fingerprint":self.identity.fingerprint(),
            "body_hash":lillux::signature::content_hash(&body),"signed_blob_hash":blob.hash,"signed_config":signed}),
        )
    }
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
            "canonical_ref": request.canonical_ref()?,
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
            "canonical_ref": request.canonical_ref()?,
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

#[cfg(test)]
mod observed_tool_tests {
    use super::*;

    #[test]
    fn substrate_receipt_tree_is_closed_and_non_executable() {
        let value = serde_json::json!({
            "schema":ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA,
            "kind":ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            "entries":[
                {"path":".ai", "kind":"dir"},
                {"path":".ai/substrate-release.json", "kind":"file", "mode":420,
                    "blob_hash":"a".repeat(64), "size":1}
            ],
            "entry_count":2,"total_bytes":1
        });
        let manifest = ExternalContentManifestObject::from_value(&value).unwrap();
        validate_substrate_receipt_tree(&manifest).unwrap();
        let mut executable = manifest.clone();
        executable.entries[1].mode = Some(0o755);
        assert!(validate_substrate_receipt_tree(&executable).is_err());
        let mut foreign = manifest;
        foreign.entries[1].path = ".ai/manifest.yaml".into();
        assert!(validate_substrate_receipt_tree(&foreign).is_err());
    }

    #[test]
    fn executable_bytes_are_part_of_the_observed_identity() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("publisher");
        std::fs::write(&executable, b"publisher-one").unwrap();
        let first = observe_publisher_tool(&executable).unwrap();
        std::fs::write(&executable, b"publisher-two").unwrap();
        let second = observe_publisher_tool(&executable).unwrap();
        assert_eq!(
            first.effective_definition_digest,
            second.effective_definition_digest
        );
        assert_ne!(first.artifact_identity_hash, second.artifact_identity_hash);
    }

    #[cfg(unix)]
    #[test]
    fn tool_measurement_refuses_symlinks() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("publisher");
        let alias = root.path().join("alias");
        std::fs::write(&executable, b"publisher").unwrap();
        symlink(&executable, &alias).unwrap();
        assert!(observe_publisher_tool(&alias).is_err());
    }
}
