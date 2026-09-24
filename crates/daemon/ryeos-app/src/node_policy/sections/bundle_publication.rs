//! Operator-owned authorization for native bundle catalogs.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context as _, bail};
use ryeos_state::{
    external_content::products::qualification::ProductQualificationPolicySource,
    objects::AdmittedLaunchArtifactIdentity,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    bundle_publication::attestation::{
        BUNDLE_CATALOG_RELEASE_CLAIM, BUNDLE_GENERATION_RELEASE_CLAIM, BUNDLE_PUBLICATION_POLICY,
        BUNDLE_SET_RELEASE_CLAIM,
    },
    node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy},
};

pub const SECTION_NAME: &str = "bundle_publication";
pub const MAX_CATALOGS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleCatalogPolicy {
    pub namespace: String,
    pub publisher_fingerprint: String,
    /// Node principals allowed to transport already publisher-signed content.
    /// This grants neither signing authority nor service invocation capability.
    pub authorized_uploaders: Vec<String>,
    pub generation_claim: String,
    pub set_claim: String,
    pub catalog_publication_claim: String,
    pub policy: String,
    pub trust_epoch: u64,
    pub frozen: bool,
    /// Exact node-authored calibration retained when this catalog authority was
    /// measured. Release callers cannot replace its execution products.
    pub calibration_run_attestation_hash: String,
    pub calibration_execution_environment:
        crate::bundle_publication::calibration::CalibrationEnvironmentEvidence,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundlePublicationPolicy {
    pub schema: u32,
    pub catalogs: Vec<BundleCatalogPolicy>,
}

impl BundleCatalogPolicy {
    pub fn require_uploader(&self, fingerprint: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!self.frozen, "catalog namespace is frozen");
        require_authorized_uploader(&self.authorized_uploaders, fingerprint)
    }
}

fn require_authorized_uploader(uploaders: &[String], fingerprint: &str) -> anyhow::Result<()> {
    validate_hash(fingerprint, "catalog uploader fingerprint")?;
    anyhow::ensure!(
        uploaders.iter().any(|allowed| allowed == fingerprint),
        "authenticated principal is not an authorized catalog uploader"
    );
    Ok(())
}

impl TypedNodePolicy for BundlePublicationPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

impl BundlePublicationPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 2 {
            bail!("bundle-publication policy schema is not current");
        }
        // No catalog is authorized until an operator provisions measured release
        // identities. Empty bootstrap policy is a deny-all policy, not a wildcard.
        if self.catalogs.len() > MAX_CATALOGS {
            bail!("bundle-publication policy exceeds the catalog bound");
        }
        let mut previous: Option<&str> = None;
        for catalog in &self.catalogs {
            validate_name(&catalog.namespace, "catalog namespace")?;
            validate_hash(
                &catalog.publisher_fingerprint,
                "catalog publisher fingerprint",
            )?;
            anyhow::ensure!(
                catalog.authorized_uploaders.len() <= 64
                    && catalog
                        .authorized_uploaders
                        .windows(2)
                        .all(|pair| pair[0] < pair[1]),
                "catalog uploaders must be bounded, sorted and unique"
            );
            for uploader in &catalog.authorized_uploaders {
                validate_hash(uploader, "catalog uploader fingerprint")?;
            }
            if catalog.generation_claim != BUNDLE_GENERATION_RELEASE_CLAIM
                || catalog.set_claim != BUNDLE_SET_RELEASE_CLAIM
                || catalog.catalog_publication_claim != BUNDLE_CATALOG_RELEASE_CLAIM
                || catalog.policy != BUNDLE_PUBLICATION_POLICY
            {
                bail!("bundle-publication catalog uses a non-current closed claim or policy");
            }
            if catalog.trust_epoch == 0 {
                bail!("bundle-publication trust epoch must be nonzero");
            }
            validate_hash(
                &catalog.calibration_run_attestation_hash,
                "catalog authority calibration attestation",
            )?;
            catalog.calibration_execution_environment.validate()?;
            validate_hash(
                &catalog.qualification_signer_fingerprint,
                "qualification signer fingerprint",
            )?;
            let qualification_key =
                lillux::crypto::VerifyingKey::from_bytes(&catalog.qualification_signer_public_key)?;
            if lillux::crypto::fingerprint(&qualification_key)
                != catalog.qualification_signer_fingerprint
            {
                bail!("qualification signer key and fingerprint disagree");
            }
            validate_hash(
                &catalog.substrate_build_signer_fingerprint,
                "substrate build signer fingerprint",
            )?;
            let substrate_build_key = lillux::crypto::VerifyingKey::from_bytes(
                &catalog.substrate_build_signer_public_key,
            )?;
            anyhow::ensure!(
                lillux::crypto::fingerprint(&substrate_build_key)
                    == catalog.substrate_build_signer_fingerprint,
                "substrate build signer key and fingerprint disagree"
            );
            require_closed_bundle_qualification(
                &catalog.portable_qualification_policy,
                &catalog.required_portable_qualification_claims,
                "config:bundle-release/portable-qualification",
                crate::bundle_publication::recipe::PORTABLE_QUALIFIER,
                "portable_bundle_release_checks_v1",
            )?;
            require_closed_bundle_qualification(
                &catalog.native_qualification_policy,
                &catalog.required_native_qualification_claims,
                "config:bundle-release/native-qualification",
                "tool:ryeos/bundle-release/native-qualify",
                "native_bundle_release_checks_v1",
            )?;
            catalog.core_seed_qualification_policy.validate()?;
            catalog
                .core_seed_qualification_verifier_artifact_identity
                .validate()?;
            validate_hash(
                &catalog.core_seed_qualification_verifier_effective_definition_digest,
                "Core seed qualification verifier definition",
            )?;
            anyhow::ensure!(
                catalog.core_seed_qualification_policy.canonical_ref
                    == crate::bundle_publication::core_seed::QUALIFICATION_POLICY
                    && catalog.core_seed_qualification_policy.policy.verifier_ref
                        == crate::bundle_publication::core_seed::QUALIFIER
                    && catalog
                        .core_seed_qualification_policy
                        .policy
                        .verifier_parameters
                        == serde_json::json!({})
                    && catalog
                        .core_seed_qualification_policy
                        .policy
                        .subject_declaration_id
                        == "subject"
                    && catalog.core_seed_qualification_policy.policy.allowed_claims
                        == [crate::bundle_publication::core_seed::QUALIFICATION_CLAIM.to_owned()]
                    && catalog.required_core_seed_qualification_claims
                        == vec![
                            crate::bundle_publication::core_seed::QUALIFICATION_CLAIM.to_owned()
                        ],
                "Core seed qualification must use its distinct fixed policy, verifier and claim"
            );
            catalog
                .portable_qualification_verifier_artifact_identity
                .validate()?;
            catalog
                .native_qualification_verifier_artifact_identity
                .validate()?;
            require_closed_bundle_qualification(
                &catalog.substrate_qualification_policy,
                &catalog.required_substrate_qualification_claims,
                "config:bundle-release/substrate-qualification",
                crate::bundle_publication::recipe::SUBSTRATE_QUALIFIER,
                "substrate_release_checks_v1",
            )?;
            require_distinct_qualification_verifiers(
                &catalog.portable_qualification_policy.policy.verifier_ref,
                &catalog.native_qualification_policy.policy.verifier_ref,
                &catalog.core_seed_qualification_policy.policy.verifier_ref,
                &catalog.substrate_qualification_policy.policy.verifier_ref,
            )?;
            catalog
                .substrate_qualification_verifier_artifact_identity
                .validate()?;
            validate_hash(
                &catalog.portable_qualification_verifier_effective_definition_digest,
                "portable qualification verifier definition",
            )?;
            validate_hash(
                &catalog.native_qualification_verifier_effective_definition_digest,
                "native qualification verifier definition",
            )?;
            validate_hash(
                &catalog.substrate_qualification_verifier_effective_definition_digest,
                "substrate qualification verifier definition",
            )?;
            validate_hash(
                &catalog.publisher_tool_effective_definition_digest,
                "publisher tool definition",
            )?;
            validate_hash(
                &catalog.publisher_tool_artifact_identity_hash,
                "publisher tool artifact",
            )?;
            if catalog.required_substrate_qualification_claims.is_empty()
                || catalog.required_substrate_qualification_claims.len() > 64
                || catalog
                    .required_substrate_qualification_claims
                    .windows(2)
                    .any(|pair| pair[0] >= pair[1])
            {
                bail!("substrate qualification claims must be bounded, sorted, and unique");
            }
            anyhow::ensure!(
                catalog.required_substrate_qualification_claims == ["substrate_release_checks_v1"],
                "substrate qualification requires the closed substrate release claim"
            );
            if previous.is_some_and(|value| value >= catalog.namespace.as_str()) {
                bail!("bundle-publication catalogs must be strictly sorted and unique");
            }
            previous = Some(&catalog.namespace);
        }
        Ok(())
    }

    pub fn require_catalog(&self, namespace: &str) -> anyhow::Result<&BundleCatalogPolicy> {
        self.catalogs
            .binary_search_by(|catalog| catalog.namespace.as_str().cmp(namespace))
            .ok()
            .map(|index| &self.catalogs[index])
            .with_context(|| format!("catalog namespace `{namespace}` is not authorized"))
    }

    pub fn section_digest(&self) -> anyhow::Result<String> {
        let value = serde_json::to_value(self)?;
        Ok(lillux::cas::sha256_hex(
            lillux::canonical_json(&value)?.as_bytes(),
        ))
    }

    pub fn catalogs_by_namespace(&self) -> BTreeMap<&str, &BundleCatalogPolicy> {
        self.catalogs
            .iter()
            .map(|catalog| (catalog.namespace.as_str(), catalog))
            .collect()
    }
}

pub struct BundlePublicationPolicySection;

impl NodePolicySection for BundlePublicationPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let policy: BundlePublicationPolicy = serde_json::from_value(body.clone())
            .context("failed to parse bundle-publication policy")?;
        policy.validate()?;
        Ok(Arc::new(policy))
    }
}

fn validate_hash(value: &str, label: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be a lowercase 64-hex digest");
    }
    Ok(())
}

fn validate_name(value: &str, label: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        bail!("{label} must be a bounded lowercase identifier");
    }
    Ok(())
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
mod tests {
    use super::*;
    use ryeos_state::external_content::products::qualification::{
        PRODUCT_QUALIFICATION_POLICY_SCHEMA, ProductQualificationPolicy,
    };

    fn qualification_source(
        policy_ref: &str,
        verifier_ref: &str,
        claim: &str,
    ) -> ProductQualificationPolicySource {
        ProductQualificationPolicySource {
            canonical_ref: policy_ref.to_owned(),
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            policy: ProductQualificationPolicy {
                schema: PRODUCT_QUALIFICATION_POLICY_SCHEMA.to_owned(),
                verifier_ref: verifier_ref.to_owned(),
                subject_declaration_id: "subject".to_owned(),
                allowed_claims: vec![claim.to_owned()],
                verifier_parameters: serde_json::json!({}),
            },
        }
    }

    fn value() -> Value {
        serde_json::json!({
            "schema": 2,
            "catalogs": [{
                "namespace": "official",
                "publisher_fingerprint": "a".repeat(64),
                "generation_claim": BUNDLE_GENERATION_RELEASE_CLAIM,
                "set_claim": BUNDLE_SET_RELEASE_CLAIM,
                "catalog_publication_claim": BUNDLE_CATALOG_RELEASE_CLAIM,
                "policy": BUNDLE_PUBLICATION_POLICY,
                "trust_epoch": 1,
                "frozen": false
            }]
        })
    }

    #[test]
    fn rejects_catalog_without_complete_consumer_proof_authority() {
        assert!(serde_json::from_value::<BundlePublicationPolicy>(value()).is_err());
    }

    #[test]
    fn empty_bootstrap_policy_authorizes_no_catalog() {
        let policy = BundlePublicationPolicy {
            schema: 2,
            catalogs: vec![],
        };
        policy.validate().unwrap();
        assert!(policy.require_catalog("official").is_err());
        assert!(
            BundlePublicationPolicy {
                schema: 1,
                catalogs: vec![],
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn upload_permission_is_explicit_and_distinct_from_publisher_trust() {
        let publisher = "a".repeat(64);
        let uploader = "b".repeat(64);
        let allowed = vec![uploader.clone()];
        assert!(require_authorized_uploader(&allowed, &uploader).is_ok());
        assert!(require_authorized_uploader(&allowed, &publisher).is_err());
        assert!(require_authorized_uploader(&[], &uploader).is_err());
        assert!(require_authorized_uploader(&allowed, "not-a-fingerprint").is_err());
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
    fn portable_and_native_qualification_require_exact_distinct_lanes() {
        let portable = qualification_source(
            "config:bundle-release/portable-qualification",
            "tool:ryeos/bundle-release/portable-qualify",
            "portable_bundle_release_checks_v1",
        );
        let native = qualification_source(
            "config:bundle-release/native-qualification",
            "tool:ryeos/bundle-release/native-qualify",
            "native_bundle_release_checks_v1",
        );
        let portable_claim = vec!["portable_bundle_release_checks_v1".to_owned()];
        let native_claim = vec!["native_bundle_release_checks_v1".to_owned()];
        assert!(
            require_closed_bundle_qualification(
                &portable,
                &portable_claim,
                "config:bundle-release/portable-qualification",
                "tool:ryeos/bundle-release/portable-qualify",
                "portable_bundle_release_checks_v1",
            )
            .is_ok()
        );
        assert!(
            require_closed_bundle_qualification(
                &native,
                &native_claim,
                "config:bundle-release/native-qualification",
                "tool:ryeos/bundle-release/native-qualify",
                "native_bundle_release_checks_v1",
            )
            .is_ok()
        );
        assert!(
            require_closed_bundle_qualification(
                &native,
                &portable_claim,
                "config:bundle-release/portable-qualification",
                "tool:ryeos/bundle-release/portable-qualify",
                "portable_bundle_release_checks_v1",
            )
            .is_err()
        );
        assert!(
            require_closed_bundle_qualification(
                &portable,
                &native_claim,
                "config:bundle-release/native-qualification",
                "tool:ryeos/bundle-release/native-qualify",
                "native_bundle_release_checks_v1",
            )
            .is_err()
        );
        let mut widened = portable.clone();
        widened.policy.allowed_claims.push("extra_claim".to_owned());
        assert!(
            require_closed_bundle_qualification(
                &widened,
                &portable_claim,
                "config:bundle-release/portable-qualification",
                "tool:ryeos/bundle-release/portable-qualify",
                "portable_bundle_release_checks_v1",
            )
            .is_err()
        );
    }
}
