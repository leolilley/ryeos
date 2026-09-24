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
    pub calibration_execution_environment: super::calibration::CalibrationEnvironmentEvidence,
    pub qualification_signer_public_key: [u8; 32],
    pub qualification_signer_fingerprint: String,
    pub portable_qualification_policy: ProductQualificationPolicySource,
    pub portable_qualification_verifier_effective_definition_digest: String,
    pub portable_qualification_verifier_artifact_identity: AdmittedLaunchArtifactIdentity,
    pub required_portable_qualification_claims: Vec<String>,
    pub native_qualification_policy: ProductQualificationPolicySource,
    pub native_qualification_verifier_effective_definition_digest: String,
    pub native_qualification_verifier_artifact_identity: AdmittedLaunchArtifactIdentity,
    pub required_native_qualification_claims: Vec<String>,
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
            schema: "ryeos.standalone_bundle_publisher_policy.v2".into(),
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
            calibration_execution_environment: catalog.calibration_execution_environment.clone(),
            qualification_signer_public_key: catalog.qualification_signer_public_key,
            qualification_signer_fingerprint: catalog.qualification_signer_fingerprint.clone(),
            portable_qualification_policy: catalog.portable_qualification_policy.clone(),
            portable_qualification_verifier_effective_definition_digest: catalog
                .portable_qualification_verifier_effective_definition_digest
                .clone(),
            portable_qualification_verifier_artifact_identity: catalog
                .portable_qualification_verifier_artifact_identity
                .clone(),
            required_portable_qualification_claims: catalog
                .required_portable_qualification_claims
                .clone(),
            native_qualification_policy: catalog.native_qualification_policy.clone(),
            native_qualification_verifier_effective_definition_digest: catalog
                .native_qualification_verifier_effective_definition_digest
                .clone(),
            native_qualification_verifier_artifact_identity: catalog
                .native_qualification_verifier_artifact_identity
                .clone(),
            required_native_qualification_claims: catalog
                .required_native_qualification_claims
                .clone(),
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
            self.schema == "ryeos.standalone_bundle_publisher_policy.v2",
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
            &self.portable_qualification_verifier_effective_definition_digest,
            "portable qualification verifier definition",
        )?;
        require_hash(
            &self.native_qualification_verifier_effective_definition_digest,
            "native qualification verifier definition",
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
        self.calibration_execution_environment.validate()?;
        require_closed_bundle_qualification(
            &self.portable_qualification_policy,
            &self.required_portable_qualification_claims,
            "config:bundle-release/portable-qualification",
            super::recipe::PORTABLE_QUALIFIER,
            "portable_bundle_release_checks_v1",
        )?;
        require_closed_bundle_qualification(
            &self.native_qualification_policy,
            &self.required_native_qualification_claims,
            "config:bundle-release/native-qualification",
            "tool:ryeos/bundle-release/native-qualify",
            "native_bundle_release_checks_v1",
        )?;
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
                && self
                    .core_seed_qualification_policy
                    .policy
                    .subject_declaration_id
                    == "subject"
                && self.core_seed_qualification_policy.policy.allowed_claims
                    == [super::core_seed::QUALIFICATION_CLAIM.to_owned()]
                && self.required_core_seed_qualification_claims
                    == vec![super::core_seed::QUALIFICATION_CLAIM.to_owned()],
            "invalid Core seed qualification policy pins"
        );
        require_closed_bundle_qualification(
            &self.substrate_qualification_policy,
            &self.required_substrate_qualification_claims,
            "config:bundle-release/substrate-qualification",
            super::recipe::SUBSTRATE_QUALIFIER,
            "substrate_release_checks_v1",
        )?;
        require_distinct_qualification_verifiers(
            &self.portable_qualification_policy.policy.verifier_ref,
            &self.native_qualification_policy.policy.verifier_ref,
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
        self.portable_qualification_verifier_artifact_identity
            .validate()?;
        self.native_qualification_verifier_artifact_identity
            .validate()?;
        self.substrate_qualification_verifier_artifact_identity
            .validate()?;
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
    portable: &str,
    native: &str,
    core_seed: &str,
    substrate: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        portable != native
            && portable != core_seed
            && portable != substrate
            && native != core_seed
            && native != substrate
            && core_seed != substrate,
        "portable, native, Core seed, and substrate qualification policies must use pairwise-distinct verifiers"
    );
    Ok(())
}

fn require_closed_bundle_qualification(
    policy: &ProductQualificationPolicySource,
    required_claims: &[String],
    policy_ref: &str,
    verifier_ref: &str,
    claim: &str,
) -> anyhow::Result<()> {
    policy.validate()?;
    anyhow::ensure!(
        policy.canonical_ref == policy_ref
            && policy.policy.verifier_ref == verifier_ref
            && policy.policy.subject_declaration_id == "subject"
            && policy.policy.verifier_parameters == serde_json::json!({})
            && policy.policy.allowed_claims == [claim.to_owned()]
            && required_claims == [claim.to_owned()],
        "bundle qualification must use its exact lane policy, verifier, and claim"
    );
    Ok(())
}

#[cfg(test)]
mod policy_invariant_tests {
    use super::{
        expected_non_core_selections, require_distinct_qualification_verifiers,
        require_non_core_release_input, require_non_core_selections,
    };
    use crate::bundle_publication::calibration::{
        CalibrationEnvironmentSelection, CalibrationProductSelection,
    };
    use ryeos_bundle_publication_contract::{
        BUNDLE_GENERATION_KIND, BUNDLE_GENERATION_SCHEMA, BundleGeneration, BundleTarget,
    };
    use serde_json::{Value, json};

    fn release_input(native: bool) -> Value {
        let target = if native {
            json!({"kind":"triple","triple":"x86_64-unknown-linux-gnu"})
        } else {
            json!({"kind":"portable"})
        };
        json!({
            "schema": "ryeos.bundle_release_input_plan.v1",
            "project_path": "/source",
            "bundle_name": "example",
            "authored_manifest": {
                "name": "example", "version": "1.0.0",
                "provides_kinds": [], "requires_kinds": []
            },
            "source_snapshot_hash": "a".repeat(64),
            "predecessor_generation_hash": null,
            "target": target,
            "build_profile": "release",
            "payload_ownership_item_ref": "config:bundle-release/payload-ownership",
            "payload_ownership_content_hash": "b".repeat(64),
            "payloads": if native { json!([{
                "bundle":"example", "binary":"example-bin", "cargo_package":"example-bin",
                "build_class":"release", "bundle_sets":["example"]
            }]) } else { json!([]) },
            "cargo_packages": if native { json!(["example-bin"]) } else { json!([]) },
            "build_classes": if native { json!(["release"]) } else { json!([]) },
            "requires_binary_build": native,
            "clean_output_required": true,
            "ambient_target_reuse_allowed": false
        })
    }

    fn generation(target: Value) -> BundleGeneration {
        serde_json::from_value(json!({
            "schema": BUNDLE_GENERATION_SCHEMA,
            "kind": BUNDLE_GENERATION_KIND,
            "bundle_name": "example",
            "authored_version": "1.0.0",
            "content_manifest_hash": "c".repeat(64),
            "manifest_item_hash": "d".repeat(64),
            "target": target,
            "build_profile": "release",
            "substrate_protocol": 1,
            "bundle_manifest_format": "ryeos.bundle-manifest/v1",
            "accepted_product_result_hash": "e".repeat(64),
            "selected_product_identity": "example",
            "selected_product_witness": "f".repeat(64),
            "publisher_materialization_result_hash": "1".repeat(64),
            "accepted_capture_result_hash": "2".repeat(64),
            "selected_signed_product_identity": "signed_example",
            "selected_signed_product_witness": "3".repeat(64),
            "source_snapshot_hash": "a".repeat(64),
            "qualification_evidence_hashes": ["4".repeat(64)],
            "provenance_hash": null,
            "sbom_hash": null
        }))
        .unwrap()
    }

    fn environment() -> CalibrationEnvironmentSelection {
        let selection = |product: char, qualification: char| CalibrationProductSelection {
            product_witness_hash: product.to_string().repeat(64),
            qualification_attestation_hash: qualification.to_string().repeat(64),
        };
        CalibrationEnvironmentSelection {
            python_runtime: selection('a', 'b'),
            platform: selection('c', 'd'),
            cargo_vendor: selection('e', 'f'),
            static_link_inputs: selection('1', '2'),
        }
    }

    #[test]
    fn qualification_verifiers_are_pairwise_distinct() {
        assert!(
            require_distinct_qualification_verifiers("portable", "native", "core", "substrate")
                .is_ok()
        );
        assert!(
            require_distinct_qualification_verifiers("same", "same", "core", "substrate").is_err()
        );
        assert!(
            require_distinct_qualification_verifiers("portable", "same", "same", "substrate")
                .is_err()
        );
        assert!(
            require_distinct_qualification_verifiers("portable", "native", "same", "same").is_err()
        );
        assert!(
            require_distinct_qualification_verifiers("same", "native", "core", "same").is_err()
        );
    }

    #[test]
    fn non_core_release_metadata_must_match_authenticated_input_in_both_lanes() {
        let portable_input = release_input(false);
        let native_input = release_input(true);
        let portable_generation = generation(portable_input["target"].clone());
        let native_generation = generation(native_input["target"].clone());
        assert!(require_non_core_release_input(&portable_generation, &portable_input).is_ok());
        assert!(require_non_core_release_input(&native_generation, &native_input).is_ok());
        assert!(require_non_core_release_input(&portable_generation, &native_input).is_err());
        assert!(require_non_core_release_input(&native_generation, &portable_input).is_err());
        for field in ["bundle_name", "source_snapshot_hash"] {
            let mut altered = portable_input.clone();
            altered[field] = json!("different");
            assert!(require_non_core_release_input(&portable_generation, &altered).is_err());
        }
        let mut altered = portable_input.clone();
        altered["authored_manifest"]["version"] = json!("2.0.0");
        assert!(require_non_core_release_input(&portable_generation, &altered).is_err());
    }

    #[test]
    fn build_and_capture_select_exact_phase_products_and_signed_build_witness() {
        let environment = environment();
        let portable = BundleTarget::Portable;
        let native = BundleTarget::Triple {
            triple: "x86_64-unknown-linux-gnu".to_owned(),
        };
        let build_witness = "3".repeat(64);
        for (target, build_count) in [(&portable, 1), (&native, 4)] {
            let build =
                expected_non_core_selections(&environment, target, false, &build_witness).unwrap();
            let capture =
                expected_non_core_selections(&environment, target, true, &build_witness).unwrap();
            assert_eq!(build.len(), build_count);
            assert_eq!(capture.len(), 2);
            assert!(
                require_non_core_selections(
                    &json!(build),
                    &environment,
                    target,
                    false,
                    &build_witness
                )
                .is_ok()
            );
            assert!(
                require_non_core_selections(
                    &json!(capture),
                    &environment,
                    target,
                    true,
                    &build_witness
                )
                .is_ok()
            );
            assert!(
                require_non_core_selections(
                    &json!(build),
                    &environment,
                    target,
                    true,
                    &build_witness
                )
                .is_err()
            );
            assert!(
                require_non_core_selections(
                    &json!(capture),
                    &environment,
                    target,
                    false,
                    &build_witness
                )
                .is_err()
            );
        }
        let capture =
            expected_non_core_selections(&environment, &native, true, &build_witness).unwrap();
        let mut wrong_witness = json!(capture);
        wrong_witness[1]["selection"]["witness_hash"] = json!("4".repeat(64));
        assert!(
            require_non_core_selections(
                &wrong_witness,
                &environment,
                &native,
                true,
                &build_witness,
            )
            .is_err()
        );
        let mut wrong_qualification = json!(capture);
        wrong_qualification[0]["selection"]["qualification_hash"] = json!("5".repeat(64));
        assert!(
            require_non_core_selections(
                &wrong_qualification,
                &environment,
                &native,
                true,
                &build_witness,
            )
            .is_err()
        );
        let mut wrong_root = json!(capture);
        wrong_root[1]["target"] = json!({"kind":"content_dependency","binding":"other"});
        assert!(
            require_non_core_selections(&wrong_root, &environment, &native, true, &build_witness,)
                .is_err()
        );
        let mut extra = json!(capture);
        extra.as_array_mut().unwrap().push(json!({
            "target":{"kind":"root"},
            "selection":{"declaration_id":"unexpected", "witness_hash":"6".repeat(64),
                "witness_source":{"kind":"local_capture"}, "qualification_hash":null}
        }));
        assert!(
            require_non_core_selections(&extra, &environment, &native, true, &build_witness,)
                .is_err()
        );
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
            let (
                build_graph,
                build_recipe,
                build_product,
                build_relationship,
                capture_graph,
                capture_recipe,
                signed_product,
                capture_relationship,
                qualifier,
                qualification_policy,
                verifier_digest,
                verifier_artifact,
                required_claims,
            ) = match &generation.target {
                ryeos_bundle_publication_contract::BundleTarget::Portable => (
                    super::recipe::PORTABLE_BUILD_GRAPH,
                    super::recipe::PORTABLE_BUILD_RECIPE_REF,
                    "portable_bundle",
                    "portable_bundle_to_signed_capture",
                    super::recipe::PORTABLE_CAPTURE_GRAPH,
                    super::recipe::PORTABLE_CAPTURE_RECIPE_REF,
                    "signed_portable_bundle",
                    "signed_portable_bundle_to_release_qualification",
                    super::recipe::PORTABLE_QUALIFIER,
                    &self.policy.portable_qualification_policy,
                    &self
                        .policy
                        .portable_qualification_verifier_effective_definition_digest,
                    &self
                        .policy
                        .portable_qualification_verifier_artifact_identity,
                    &self.policy.required_portable_qualification_claims,
                ),
                ryeos_bundle_publication_contract::BundleTarget::Triple { .. } => (
                    super::recipe::BUILD_GRAPH,
                    super::recipe::BUILD_RECIPE_REF,
                    "native_bundle",
                    "native_bundle_to_signed_capture",
                    super::recipe::CAPTURE_GRAPH,
                    super::recipe::CAPTURE_RECIPE_REF,
                    "signed_native_bundle",
                    "signed_native_bundle_to_release_qualification",
                    "tool:ryeos/bundle-release/native-qualify",
                    &self.policy.native_qualification_policy,
                    &self
                        .policy
                        .native_qualification_verifier_effective_definition_digest,
                    &self.policy.native_qualification_verifier_artifact_identity,
                    &self.policy.required_native_qualification_claims,
                ),
            };
            anyhow::ensure!(
                accepted.producer_ref == build_graph
                    && accepted_capture.producer_ref == capture_graph
                    && accepted.owner_principal == accepted_capture.owner_principal
                    && generation.selected_product_identity == build_product
                    && generation.selected_signed_product_identity == signed_product,
                "bundle generation selected a product outside its authenticated target lane"
            );
            let mut release_input_value = None;
            for (
                result,
                witness_hash,
                recipe_ref,
                manifest_hash,
                product_name,
                relationship_name,
                consumer_ref,
                consumer_declaration,
            ) in [
                (
                    accepted,
                    &generation.selected_product_witness,
                    build_recipe,
                    &materialization.input_content_manifest_hash,
                    build_product,
                    build_relationship,
                    capture_graph,
                    "unsigned_bundle",
                ),
                (
                    accepted_capture,
                    &generation.selected_signed_product_witness,
                    capture_recipe,
                    &materialization.output_content_manifest_hash,
                    signed_product,
                    capture_relationship,
                    qualifier,
                    "subject",
                ),
            ] {
                let witness =
                    Attestation::from_value(&super::read_exact(self.cas.as_ref(), witness_hash)?)?;
                anyhow::ensure!(
                    !witness.is_expired_at(&lillux::time::iso8601_now())?,
                    "bundle product witness expired"
                );
                let product = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
                    &witness, &build_key, &result.owner_principal,
                )?;
                product.recipe_purpose.require_bundle_release()?;
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
                    "accepted bundle product contradicts authenticated capture evidence"
                );
                let relationship = product.relationships.select(relationship_name)?;
                relationship.validate_product_evidence(&product)?;
                anyhow::ensure!(
                    relationship.producer.canonical_ref == result.producer_ref
                        && relationship.producer.product_name == product_name
                        && relationship.consumer.canonical_ref == consumer_ref
                        && relationship.consumer.declaration_id == consumer_declaration,
                    "bundle product relationship differs from its target lane"
                );
                let parameters = relationship
                    .producer
                    .parameters
                    .as_object()
                    .context("bundle producer parameters are not an object")?;
                let release_value = parameters
                    .get("release_input")
                    .context("bundle producer omitted release input")?;
                require_non_core_release_input(generation, release_value)?;
                if let Some(previous) = &release_input_value {
                    anyhow::ensure!(
                        previous == release_value,
                        "build and capture witnesses name different release inputs"
                    );
                } else {
                    release_input_value = Some(release_value.clone());
                }
                require_non_core_selections(
                    parameters
                        .get("child_product_selections")
                        .context("bundle producer omitted child product selections")?,
                    &calibration_environment_selection(
                        &self.policy.calibration_execution_environment,
                    ),
                    &generation.target,
                    recipe_ref == capture_recipe,
                    &generation.selected_product_witness,
                )?;
                if recipe_ref == capture_recipe {
                    anyhow::ensure!(
                        parameters.len() == 6
                            && parameters["materialization_result_hash"]
                                == generation.publisher_materialization_result_hash
                            && parameters["signed_tree_manifest_hash"]
                                == generation.content_manifest_hash
                            && parameters["manifest_item_hash"] == generation.manifest_item_hash
                            && parameters["signed_manifest"]
                                .as_str()
                                .is_some_and(|manifest| lillux::sha256_hex(manifest.as_bytes())
                                    == generation.manifest_item_hash),
                        "bundle capture is not bound to the released publisher transformation"
                    );
                    anyhow::ensure!(
                        relationship.qualification.policy_ref.as_deref()
                            == Some(qualification_policy.canonical_ref.as_str())
                            && relationship.qualification.required_claims == *required_claims,
                        "bundle capture qualification relationship differs from its target lane"
                    );
                } else {
                    anyhow::ensure!(
                        parameters.len() == 2
                            && relationship.qualification.policy_ref.is_none()
                            && relationship.qualification.required_claims.is_empty(),
                        "bundle build relationship differs from its target lane"
                    );
                }
            }
            evidence.validate_current_policy(
                qualification_policy,
                verifier_digest,
                required_claims,
            )?;
            evidence.validate_current_artifact(verifier_artifact)
        }
    }
}

fn calibration_environment_selection(
    environment: &super::calibration::CalibrationEnvironmentEvidence,
) -> super::calibration::CalibrationEnvironmentSelection {
    super::calibration::CalibrationEnvironmentSelection {
        python_runtime: environment.python_runtime.selection.clone(),
        platform: environment.platform.selection.clone(),
        cargo_vendor: environment.cargo_vendor.selection.clone(),
        static_link_inputs: environment.static_link_inputs.selection.clone(),
    }
}

fn expected_non_core_selections(
    environment: &super::calibration::CalibrationEnvironmentSelection,
    target: &ryeos_bundle_publication_contract::BundleTarget,
    capture: bool,
    build_witness: &str,
) -> anyhow::Result<super::recipe::ReleaseChildProductSelections> {
    use ryeos_state::external_content::products::{
        composition::{
            ProductSelection, ProductSelectionInput, ProductSelectionTarget,
            canonicalize_product_selection_inputs,
        },
        transfer::ProductWitnessSource,
    };
    environment.validate()?;
    let environment_selection =
        |declaration_id: &str, selected: &super::calibration::CalibrationProductSelection| {
            ProductSelectionInput {
                target: ProductSelectionTarget::Root {},
                selection: ProductSelection {
                    declaration_id: declaration_id.to_owned(),
                    witness_hash: selected.product_witness_hash.clone(),
                    witness_source: ProductWitnessSource::LocalCapture {},
                    qualification_hash: Some(selected.qualification_attestation_hash.clone()),
                },
            }
        };
    let mut selections = vec![environment_selection("python", &environment.python_runtime)];
    if capture {
        selections.push(ProductSelectionInput {
            target: ProductSelectionTarget::Root {},
            selection: ProductSelection {
                declaration_id: "unsigned_bundle".to_owned(),
                witness_hash: build_witness.to_owned(),
                witness_source: ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        });
    } else if matches!(
        target,
        ryeos_bundle_publication_contract::BundleTarget::Triple { .. }
    ) {
        selections.extend([
            environment_selection("cargo-vendor", &environment.cargo_vendor),
            environment_selection("platform", &environment.platform),
            environment_selection("static-link-inputs", &environment.static_link_inputs),
        ]);
    }
    canonicalize_product_selection_inputs(selections)
}

fn require_non_core_selections(
    observed: &serde_json::Value,
    environment: &super::calibration::CalibrationEnvironmentSelection,
    target: &ryeos_bundle_publication_contract::BundleTarget,
    capture: bool,
    build_witness: &str,
) -> anyhow::Result<()> {
    let selected: super::recipe::ReleaseChildProductSelections =
        serde_json::from_value(observed.clone())?;
    super::recipe::validate_child_product_selections(&selected)?;
    anyhow::ensure!(
        selected == expected_non_core_selections(environment, target, capture, build_witness)?,
        "bundle phase selects products outside its pinned calibration and signed build witness"
    );
    Ok(())
}

fn require_non_core_release_input(
    generation: &ryeos_bundle_publication_contract::BundleGeneration,
    release_value: &serde_json::Value,
) -> anyhow::Result<()> {
    let input = super::admitted_build::AdmittedReleaseInput::from_value(release_value)?;
    anyhow::ensure!(
        generation.bundle_name == input.bundle_name
            && generation.authored_version == input.authored_manifest.version
            && generation.source_snapshot_hash.as_deref()
                == Some(input.source_snapshot_hash.as_str())
            && generation.target == input.target,
        "bundle generation metadata differs from authenticated release input"
    );
    Ok(())
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
