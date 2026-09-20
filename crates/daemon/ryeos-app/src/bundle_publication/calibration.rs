//! Domain-separated evidence for bootstrapping bundle-publication authority.
//!
//! Calibration evidence measures the fixed producer and verifier realizations
//! needed to author the first catalog policy. It is deliberately not a bundle
//! release object and cannot be consumed by release finalization.

use anyhow::{Context as _, bail};
use ryeos_state::{objects::Attestation, signer::Signer};
use serde::{Deserialize, Serialize};

pub const AUTHORITY_CALIBRATION_SCHEMA: &str = "ryeos.bundle_publication_authority_calibration.v1";
pub const AUTHORITY_CALIBRATION_CLAIM: &str = "bundle_publication_authority_calibration_v1";
pub const AUTHORITY_CALIBRATION_POLICY: &str = "ryeos.bundle_publication_authority_calibration.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityCalibrationRequest {
    pub project_path: String,
    pub source_snapshot_hash: String,
}

impl AuthorityCalibrationRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.project_path.trim().is_empty() || self.project_path.chars().any(char::is_control) {
            bail!("authority calibration project path is invalid");
        }
        canonical_hash("calibration source snapshot", &self.source_snapshot_hash)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityCalibrationResult {
    pub calibration_run_attestation_hash: String,
    pub evidence: AuthorityCalibrationEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationRecipeIdentity {
    pub signed_config_hash: String,
    pub raw_content_digest: String,
}

impl CalibrationRecipeIdentity {
    fn validate(&self, label: &str) -> anyhow::Result<()> {
        canonical_hash(&format!("{label} signed config"), &self.signed_config_hash)?;
        canonical_hash(&format!("{label} raw content"), &self.raw_content_digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityCalibrationRecipes {
    pub native_build: CalibrationRecipeIdentity,
    pub native_capture: CalibrationRecipeIdentity,
    pub native_qualification: CalibrationRecipeIdentity,
    pub core_seed_build: CalibrationRecipeIdentity,
    pub core_seed_capture: CalibrationRecipeIdentity,
    pub core_seed_qualification: CalibrationRecipeIdentity,
    pub substrate_build: CalibrationRecipeIdentity,
    pub substrate_qualification: CalibrationRecipeIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityCalibrationLane {
    pub owner_principal: String,
    pub qualification_attestation_hash: String,
}

impl AuthorityCalibrationLane {
    fn validate(&self, label: &str) -> anyhow::Result<()> {
        let fingerprint = self
            .owner_principal
            .strip_prefix("fp:")
            .with_context(|| format!("{label} calibration owner is not a fingerprint principal"))?;
        canonical_hash(&format!("{label} calibration owner"), fingerprint)?;
        canonical_hash(
            &format!("{label} qualification attestation"),
            &self.qualification_attestation_hash,
        )
    }
}

/// Node-authored, immutable summary of one complete calibration execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityCalibrationEvidence {
    pub schema: String,
    pub source_snapshot_hash: String,
    pub node_signer_fingerprint: String,
    pub substrate_image_digest: String,
    pub substrate_protocol: u32,
    pub native: AuthorityCalibrationLane,
    pub core_seed: AuthorityCalibrationLane,
    pub substrate: AuthorityCalibrationLane,
    pub substrate_product_witness_hash: String,
    pub substrate_product_witness_signer_fingerprint: String,
    pub recipes: AuthorityCalibrationRecipes,
}

impl AuthorityCalibrationEvidence {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != AUTHORITY_CALIBRATION_SCHEMA {
            bail!("authority calibration evidence schema is not current");
        }
        canonical_hash("calibration source snapshot", &self.source_snapshot_hash)?;
        canonical_hash("calibration node signer", &self.node_signer_fingerprint)?;
        let image = self
            .substrate_image_digest
            .strip_prefix("sha256:")
            .context("calibration substrate image digest must use sha256:<lowercase-hex>")?;
        canonical_hash("calibration substrate image", image)?;
        if self.substrate_protocol == 0 {
            bail!("calibration substrate protocol must be nonzero");
        }
        self.native.validate("native")?;
        self.core_seed.validate("Core seed")?;
        self.substrate.validate("substrate")?;
        canonical_hash(
            "calibration substrate product witness",
            &self.substrate_product_witness_hash,
        )?;
        canonical_hash(
            "calibration substrate product witness signer",
            &self.substrate_product_witness_signer_fingerprint,
        )?;
        for (label, identity) in [
            ("native build recipe", &self.recipes.native_build),
            ("native capture recipe", &self.recipes.native_capture),
            (
                "native qualification policy",
                &self.recipes.native_qualification,
            ),
            ("Core seed build recipe", &self.recipes.core_seed_build),
            ("Core seed capture recipe", &self.recipes.core_seed_capture),
            (
                "Core seed qualification policy",
                &self.recipes.core_seed_qualification,
            ),
            ("substrate build recipe", &self.recipes.substrate_build),
            (
                "substrate qualification policy",
                &self.recipes.substrate_qualification,
            ),
        ] {
            identity.validate(label)?;
        }
        Ok(())
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        self.validate()?;
        ryeos_state::objects::canonical_value_digest(&serde_json::to_value(self)?)
    }

    pub fn sign_attestation(&self, signer: &dyn Signer) -> anyhow::Result<Attestation> {
        let subject_hash = self.content_hash()?;
        Attestation::unsigned(
            subject_hash,
            AUTHORITY_CALIBRATION_CLAIM.to_owned(),
            AUTHORITY_CALIBRATION_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::to_value(self)?,
        )
        .sign(signer)
    }

    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        if attestation.claim != AUTHORITY_CALIBRATION_CLAIM
            || attestation.policy != AUTHORITY_CALIBRATION_POLICY
        {
            bail!("attestation is not an authority calibration run");
        }
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())?;
        evidence.validate()?;
        if attestation.subject_hash != evidence.content_hash()? {
            bail!("authority calibration attestation subject changed");
        }
        if attestation.issuer_fingerprint()? != evidence.node_signer_fingerprint {
            bail!("authority calibration attestation signer changed");
        }
        Ok(evidence)
    }
}

fn canonical_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be a lowercase sha256 digest");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::signer::Signer;

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    #[test]
    fn calibration_attestation_is_domain_separated_and_node_bound() {
        let key = lillux::crypto::SigningKey::from_bytes(&[31; 32]);
        let fingerprint = lillux::crypto::fingerprint(&key.verifying_key());
        struct BoundSigner {
            key: lillux::crypto::SigningKey,
            fingerprint: String,
        }
        impl Signer for BoundSigner {
            fn fingerprint(&self) -> &str {
                &self.fingerprint
            }
            fn sign(&self, data: &[u8]) -> Vec<u8> {
                use lillux::crypto::Signer as _;
                self.key.sign(data).to_bytes().to_vec()
            }
            fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
                self.key.verifying_key()
            }
        }
        let recipe = CalibrationRecipeIdentity {
            signed_config_hash: hash('a'),
            raw_content_digest: hash('b'),
        };
        let lane = AuthorityCalibrationLane {
            owner_principal: format!("fp:{fingerprint}"),
            qualification_attestation_hash: hash('c'),
        };
        let evidence = AuthorityCalibrationEvidence {
            schema: AUTHORITY_CALIBRATION_SCHEMA.to_owned(),
            source_snapshot_hash: hash('d'),
            node_signer_fingerprint: fingerprint.clone(),
            substrate_image_digest: format!("sha256:{}", hash('e')),
            substrate_protocol: 1,
            native: lane.clone(),
            core_seed: lane.clone(),
            substrate: lane,
            substrate_product_witness_hash: hash('f'),
            substrate_product_witness_signer_fingerprint: fingerprint.clone(),
            recipes: AuthorityCalibrationRecipes {
                native_build: recipe.clone(),
                native_capture: recipe.clone(),
                native_qualification: recipe.clone(),
                core_seed_build: recipe.clone(),
                core_seed_capture: recipe.clone(),
                core_seed_qualification: recipe.clone(),
                substrate_build: recipe.clone(),
                substrate_qualification: recipe,
            },
        };
        let signer = BoundSigner { key, fingerprint };
        let attestation = evidence.sign_attestation(&signer).unwrap();
        attestation
            .verify_with_key(&signer.verifying_key())
            .unwrap();
        assert_eq!(
            AuthorityCalibrationEvidence::from_attestation(&attestation).unwrap(),
            evidence
        );
        assert_ne!(
            attestation.claim,
            super::super::attestation::BUNDLE_GENERATION_RELEASE_CLAIM
        );
    }
}
