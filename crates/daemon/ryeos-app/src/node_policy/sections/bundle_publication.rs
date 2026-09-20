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
    pub qualification_signer_public_key: [u8; 32],
    pub qualification_signer_fingerprint: String,
    pub qualification_policy: ProductQualificationPolicySource,
    pub qualification_verifier_effective_definition_digest: String,
    pub qualification_verifier_artifact_identity: AdmittedLaunchArtifactIdentity,
    pub required_qualification_claims: Vec<String>,
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
        if self.schema != 1 {
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
            catalog.qualification_policy.validate()?;
            catalog
                .qualification_verifier_artifact_identity
                .validate()?;
            validate_hash(
                &catalog.qualification_verifier_effective_definition_digest,
                "qualification verifier definition",
            )?;
            validate_hash(
                &catalog.publisher_tool_effective_definition_digest,
                "publisher tool definition",
            )?;
            validate_hash(
                &catalog.publisher_tool_artifact_identity_hash,
                "publisher tool artifact",
            )?;
            if catalog.required_qualification_claims.is_empty()
                || catalog.required_qualification_claims.len() > 64
                || catalog
                    .required_qualification_claims
                    .windows(2)
                    .any(|pair| pair[0] >= pair[1])
            {
                bail!("qualification claims must be bounded, sorted, and unique");
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn value() -> Value {
        serde_json::json!({
            "schema": 1,
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
            schema: 1,
            catalogs: vec![],
        };
        policy.validate().unwrap();
        assert!(policy.require_catalog("official").is_err());
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
}
