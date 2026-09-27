//! Domain-separated evidence for bootstrapping bundle-publication authority.
//!
//! Calibration evidence measures the fixed producer and verifier realizations
//! needed to author the first catalog policy. It is deliberately not a bundle
//! release object and cannot be consumed by release finalization.

use anyhow::{Context as _, bail};
use ryeos_state::{objects::Attestation, signer::Signer};
use serde::{Deserialize, Serialize};

pub const AUTHORITY_CALIBRATION_SCHEMA: &str = "ryeos.bundle_publication_authority_calibration.v2";
pub const AUTHORITY_CALIBRATION_CLAIM: &str = "bundle_publication_authority_calibration_v2";
pub const AUTHORITY_CALIBRATION_POLICY: &str = "ryeos.bundle_publication_authority_calibration.v2";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityCalibrationRequest {
    pub project_path: String,
    pub source_snapshot_hash: String,
    pub execution_environment: CalibrationEnvironmentSelection,
}

impl AuthorityCalibrationRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.project_path.trim().is_empty() || self.project_path.chars().any(char::is_control) {
            bail!("authority calibration project path is invalid");
        }
        if self.project_path.len() > 4096 {
            bail!("authority calibration project path exceeds limit");
        }
        canonical_hash("calibration source snapshot", &self.source_snapshot_hash)?;
        self.execution_environment.validate()
    }
}

/// Local retained products only. No caller-controlled source, owner, relationship,
/// declaration, redemption, or mount authority is accepted by this contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationProductSelection {
    pub product_witness_hash: String,
    pub qualification_attestation_hash: String,
}

impl CalibrationProductSelection {
    pub fn validate(&self) -> anyhow::Result<()> {
        canonical_hash("environment product witness", &self.product_witness_hash)?;
        canonical_hash(
            "environment qualification",
            &self.qualification_attestation_hash,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationEnvironmentSelection {
    pub python_runtime: CalibrationProductSelection,
    pub platform: CalibrationProductSelection,
    pub cargo_vendor: CalibrationProductSelection,
    pub static_link_inputs: CalibrationProductSelection,
}

impl CalibrationEnvironmentSelection {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.python_runtime.validate()?;
        self.platform.validate()?;
        self.cargo_vendor.validate()?;
        self.static_link_inputs.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationEnvironmentProduct {
    pub selection: CalibrationProductSelection,
    pub owner_principal: String,
    pub producer: ryeos_state::external_content::products::ProductProducerAdmission,
    pub recipe_ref: String,
    pub recipe_raw_content_digest: String,
    pub qualification_policy_item_hash: String,
    pub verifier_artifact_hash: String,
    /// Bounded verifier-authored semantic evidence. Admission parses the
    /// platform member into a closed target/ABI contract before any build.
    pub qualification_probe_evidence: serde_json::Value,
}

impl CalibrationEnvironmentProduct {
    fn validate(&self) -> anyhow::Result<()> {
        self.selection.validate()?;
        canonical_hash(
            "environment owner",
            self.owner_principal
                .strip_prefix("fp:")
                .context("environment owner must be a fingerprint principal")?,
        )?;
        self.producer.validate()?;
        if !self.recipe_ref.starts_with("config:") {
            bail!("environment producer recipe must be an exact Config ref");
        }
        canonical_hash(
            "environment producer recipe content",
            &self.recipe_raw_content_digest,
        )?;
        canonical_hash(
            "environment qualification policy",
            &self.qualification_policy_item_hash,
        )?;
        canonical_hash(
            "environment verifier artifact",
            &self.verifier_artifact_hash,
        )?;
        if !self.qualification_probe_evidence.is_object()
            || serde_json::to_vec(&self.qualification_probe_evidence)?.len() > 64 * 1024
        {
            bail!("environment qualification probe evidence is invalid or oversized");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationEnvironmentKind {
    PythonRuntime,
    Platform,
    CargoVendor,
    StaticLinkInputs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationTerminal {
    PortableBuild,
    PortableCapture,
    PortableQualification,
    NativeBuild,
    NativeCapture,
    NativeQualification,
    CoreSeedBuild,
    CoreSeedCapture,
    CoreSeedQualification,
    SubstrateBuild,
    SubstrateQualification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationEnvironmentUse {
    pub terminal: CalibrationTerminal,
    pub environment: CalibrationEnvironmentKind,
    pub relationship_name: String,
    pub relationship_config_item_hash: String,
}

/// Semantic retained identities only; live grants, expiry, revocation and CAS
/// closure must be re-admitted by the measuring authority, not inferred here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationEnvironmentEvidence {
    pub node_policy_generation_hash: String,
    pub python_runtime: CalibrationEnvironmentProduct,
    pub platform: CalibrationEnvironmentProduct,
    pub cargo_vendor: CalibrationEnvironmentProduct,
    pub static_link_inputs: CalibrationEnvironmentProduct,
    pub terminal_uses: Vec<CalibrationEnvironmentUse>,
}

impl CalibrationEnvironmentEvidence {
    /// Checks request-to-evidence identity only. It does not replace admission
    /// of each retained product under current signed policy.
    pub fn require_selection(
        &self,
        selected: &CalibrationEnvironmentSelection,
    ) -> anyhow::Result<()> {
        self.validate()?;
        selected.validate()?;
        if self.python_runtime.selection != selected.python_runtime
            || self.platform.selection != selected.platform
            || self.cargo_vendor.selection != selected.cargo_vendor
            || self.static_link_inputs.selection != selected.static_link_inputs
        {
            bail!("calibration environment differs from requested retained products");
        }
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        canonical_hash(
            "environment node policy generation",
            &self.node_policy_generation_hash,
        )?;
        self.python_runtime.validate()?;
        self.platform.validate()?;
        self.cargo_vendor.validate()?;
        self.static_link_inputs.validate()?;
        use CalibrationEnvironmentKind::*;
        use CalibrationTerminal::*;
        let expected: std::collections::BTreeSet<_> = [
            PortableBuild,
            PortableCapture,
            PortableQualification,
            NativeBuild,
            NativeCapture,
            NativeQualification,
            CoreSeedBuild,
            CoreSeedCapture,
            CoreSeedQualification,
            SubstrateBuild,
            SubstrateQualification,
        ]
        .into_iter()
        .map(|terminal| (terminal, PythonRuntime))
        .chain(
            [NativeBuild, CoreSeedBuild]
                .into_iter()
                .flat_map(|terminal| {
                    [Platform, CargoVendor, StaticLinkInputs]
                        .into_iter()
                        .map(move |environment| (terminal, environment))
                }),
        )
        .collect();
        if self.terminal_uses.len() != expected.len() {
            bail!("calibration environment use map is incomplete or oversized");
        }
        let mut actual = std::collections::BTreeSet::new();
        for usage in &self.terminal_uses {
            canonical_hash(
                "environment relationship config",
                &usage.relationship_config_item_hash,
            )?;
            if !actual.insert((usage.terminal, usage.environment)) {
                bail!("duplicate calibration environment use");
            }
            let expected_relationship = match (usage.terminal, usage.environment) {
                (PortableBuild, PythonRuntime) => "python_to_portable_build",
                (PortableCapture, PythonRuntime) => "python_to_portable_signed_capture",
                (PortableQualification, PythonRuntime) => "python_to_portable_qualify",
                (NativeBuild, PythonRuntime) => "python_to_native_build",
                (NativeBuild, Platform) => "platform_to_native_build",
                (NativeBuild, CargoVendor) => "cargo_vendor_to_native_build",
                (NativeBuild, StaticLinkInputs) => "static_link_inputs_to_native_build",
                (NativeCapture, PythonRuntime) => "python_to_signed_capture",
                (NativeQualification, PythonRuntime) => "python_to_native_qualify",
                (CoreSeedBuild, PythonRuntime) => "python_to_core_seed_build",
                (CoreSeedBuild, Platform) => "platform_to_core_seed_build",
                (CoreSeedBuild, CargoVendor) => "cargo_vendor_to_core_seed_build",
                (CoreSeedBuild, StaticLinkInputs) => "static_link_inputs_to_core_seed_build",
                (CoreSeedCapture, PythonRuntime) => "python_to_core_seed_capture",
                (CoreSeedQualification, PythonRuntime) => "python_to_core_seed_qualify",
                (SubstrateBuild, PythonRuntime) => "python_to_substrate_build",
                (SubstrateQualification, PythonRuntime) => "python_to_substrate_qualify",
                _ => bail!("calibration environment use has no admitted relationship"),
            };
            if usage.relationship_name != expected_relationship {
                bail!("calibration environment use names another relationship");
            }
        }
        if actual != expected {
            bail!("calibration environment use map grants incorrect terminal inputs");
        }
        Ok(())
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
    pub portable_build: CalibrationRecipeIdentity,
    pub portable_capture: CalibrationRecipeIdentity,
    pub portable_qualification: CalibrationRecipeIdentity,
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
    pub portable: AuthorityCalibrationLane,
    pub native: AuthorityCalibrationLane,
    pub core_seed: AuthorityCalibrationLane,
    pub substrate: AuthorityCalibrationLane,
    pub substrate_product_witness_hash: String,
    pub substrate_product_witness_signer_fingerprint: String,
    /// Immutable source recipes whose shape authorized this calibration.
    pub source_recipes: AuthorityCalibrationRecipes,
    /// Exact, signed recipes admitted for this run's producer parameters.
    pub recipes: AuthorityCalibrationRecipes,
    pub execution_environment: CalibrationEnvironmentEvidence,
}

impl AuthorityCalibrationEvidence {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.execution_environment.validate()?;
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
        self.portable.validate("portable")?;
        self.native.validate("native")?;
        if self.portable.qualification_attestation_hash
            == self.native.qualification_attestation_hash
        {
            bail!("portable and native calibration must retain distinct qualifications");
        }
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
            (
                "portable source build recipe",
                &self.source_recipes.portable_build,
            ),
            (
                "portable source capture recipe",
                &self.source_recipes.portable_capture,
            ),
            (
                "portable source qualification policy",
                &self.source_recipes.portable_qualification,
            ),
            (
                "native source build recipe",
                &self.source_recipes.native_build,
            ),
            (
                "native source capture recipe",
                &self.source_recipes.native_capture,
            ),
            (
                "native source qualification policy",
                &self.source_recipes.native_qualification,
            ),
            (
                "Core seed source build recipe",
                &self.source_recipes.core_seed_build,
            ),
            (
                "Core seed source capture recipe",
                &self.source_recipes.core_seed_capture,
            ),
            (
                "Core seed source qualification policy",
                &self.source_recipes.core_seed_qualification,
            ),
            (
                "substrate source build recipe",
                &self.source_recipes.substrate_build,
            ),
            (
                "substrate source qualification policy",
                &self.source_recipes.substrate_qualification,
            ),
            ("portable build recipe", &self.recipes.portable_build),
            ("portable capture recipe", &self.recipes.portable_capture),
            (
                "portable qualification policy",
                &self.recipes.portable_qualification,
            ),
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

    fn environment() -> CalibrationEnvironmentEvidence {
        let product = CalibrationEnvironmentProduct {
            selection: CalibrationProductSelection {
                product_witness_hash: hash('a'),
                qualification_attestation_hash: hash('b'),
            },
            owner_principal: format!("fp:{}", hash('c')),
            producer: ryeos_state::external_content::products::ProductProducerAdmission {
                canonical_ref: "graph:test/producer".into(),
                effective_definition_digest: hash('d'),
                exact_program_hash: hash('e'),
                producer_project_snapshot_hash: hash('4'),
                launch_authority_digest: hash('5'),
                admitted_parameters_digest: hash('6'),
            },
            recipe_ref: "config:test/products".into(),
            recipe_raw_content_digest: hash('7'),
            qualification_policy_item_hash: hash('f'),
            verifier_artifact_hash: hash('1'),
            qualification_probe_evidence: serde_json::json!({"schema":"test.v1"}),
        };
        use CalibrationEnvironmentKind::*;
        use CalibrationTerminal::*;
        let terminal_uses = [
            PortableBuild,
            PortableCapture,
            PortableQualification,
            NativeBuild,
            NativeCapture,
            NativeQualification,
            CoreSeedBuild,
            CoreSeedCapture,
            CoreSeedQualification,
            SubstrateBuild,
            SubstrateQualification,
        ]
        .into_iter()
        .map(|terminal| (terminal, PythonRuntime))
        .chain(
            [NativeBuild, CoreSeedBuild]
                .into_iter()
                .flat_map(|terminal| {
                    [Platform, CargoVendor, StaticLinkInputs]
                        .into_iter()
                        .map(move |kind| (terminal, kind))
                }),
        )
        .map(|(terminal, environment)| CalibrationEnvironmentUse {
            relationship_name: match (terminal, environment) {
                (PortableBuild, PythonRuntime) => "python_to_portable_build",
                (PortableCapture, PythonRuntime) => "python_to_portable_signed_capture",
                (PortableQualification, PythonRuntime) => "python_to_portable_qualify",
                (NativeBuild, PythonRuntime) => "python_to_native_build",
                (NativeBuild, Platform) => "platform_to_native_build",
                (NativeBuild, CargoVendor) => "cargo_vendor_to_native_build",
                (NativeBuild, StaticLinkInputs) => "static_link_inputs_to_native_build",
                (NativeCapture, PythonRuntime) => "python_to_signed_capture",
                (NativeQualification, PythonRuntime) => "python_to_native_qualify",
                (CoreSeedBuild, PythonRuntime) => "python_to_core_seed_build",
                (CoreSeedBuild, Platform) => "platform_to_core_seed_build",
                (CoreSeedBuild, CargoVendor) => "cargo_vendor_to_core_seed_build",
                (CoreSeedBuild, StaticLinkInputs) => "static_link_inputs_to_core_seed_build",
                (CoreSeedCapture, PythonRuntime) => "python_to_core_seed_capture",
                (CoreSeedQualification, PythonRuntime) => "python_to_core_seed_qualify",
                (SubstrateBuild, PythonRuntime) => "python_to_substrate_build",
                (SubstrateQualification, PythonRuntime) => "python_to_substrate_qualify",
                _ => unreachable!(),
            }
            .into(),
            terminal,
            environment,
            relationship_config_item_hash: hash('2'),
        })
        .collect();
        CalibrationEnvironmentEvidence {
            node_policy_generation_hash: hash('3'),
            python_runtime: product.clone(),
            platform: product.clone(),
            cargo_vendor: product.clone(),
            static_link_inputs: product,
            terminal_uses,
        }
    }

    #[test]
    fn environment_use_map_is_exact_and_bounded() {
        let valid = environment();
        valid.validate().unwrap();
        let mut selected = CalibrationEnvironmentSelection {
            python_runtime: valid.python_runtime.selection.clone(),
            platform: valid.platform.selection.clone(),
            cargo_vendor: valid.cargo_vendor.selection.clone(),
            static_link_inputs: valid.static_link_inputs.selection.clone(),
        };
        valid.require_selection(&selected).unwrap();
        selected.static_link_inputs.product_witness_hash = hash('8');
        assert!(valid.require_selection(&selected).is_err());
        selected.static_link_inputs = valid.static_link_inputs.selection.clone();
        let mut serialized = serde_json::to_value(&selected).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("static_link_inputs");
        assert!(serde_json::from_value::<CalibrationEnvironmentSelection>(serialized).is_err());
        selected.platform.product_witness_hash = hash('9');
        assert!(valid.require_selection(&selected).is_err());
        let mut missing = valid.clone();
        missing.terminal_uses.pop();
        assert!(missing.validate().is_err());
        let mut extra = valid.clone();
        extra.terminal_uses.push(extra.terminal_uses[0].clone());
        assert!(extra.validate().is_err());
        let mut duplicate = valid.clone();
        duplicate.terminal_uses[1] = duplicate.terminal_uses[0].clone();
        assert!(duplicate.validate().is_err());
        let mut authority = valid.clone();
        authority.terminal_uses[0].environment = CalibrationEnvironmentKind::Platform;
        assert!(authority.validate().is_err());
        let mut portable_static = valid.clone();
        portable_static.terminal_uses[0].environment = CalibrationEnvironmentKind::StaticLinkInputs;
        assert!(portable_static.validate().is_err());
        let mut owner = valid.clone();
        owner.python_runtime.owner_principal = "node:ambient".into();
        assert!(owner.validate().is_err());
        let mut policy = valid;
        policy.platform.qualification_policy_item_hash = hash('A');
        assert!(policy.validate().is_err());
    }

    #[test]
    fn environment_request_rejects_extra_authority_and_duplicate_fields() {
        let selection = environment().python_runtime.selection;
        let encoded = serde_json::to_string(&selection).unwrap();
        let extra = encoded.replacen('{', "{\"owner_principal\":\"fp:other\",", 1);
        assert!(serde_json::from_str::<CalibrationProductSelection>(&extra).is_err());
        let duplicate = encoded.replacen(
            '{',
            &format!("{{\"product_witness_hash\":\"{}\",", hash('c')),
            1,
        );
        assert!(serde_json::from_str::<CalibrationProductSelection>(&duplicate).is_err());
        assert!(
            serde_json::from_value::<AuthorityCalibrationRequest>(serde_json::json!({
                "project_path":"/retained/source", "source_snapshot_hash":hash('a')
            }))
            .is_err()
        );
        let mut unqualified = selection;
        unqualified.qualification_attestation_hash.clear();
        assert!(unqualified.validate().is_err());
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
        let source_recipe = CalibrationRecipeIdentity {
            signed_config_hash: hash('1'),
            raw_content_digest: hash('2'),
        };
        let lane = AuthorityCalibrationLane {
            owner_principal: format!("fp:{fingerprint}"),
            qualification_attestation_hash: hash('c'),
        };
        let evidence = AuthorityCalibrationEvidence {
            execution_environment: environment(),
            schema: AUTHORITY_CALIBRATION_SCHEMA.to_owned(),
            source_snapshot_hash: hash('d'),
            node_signer_fingerprint: fingerprint.clone(),
            substrate_image_digest: format!("sha256:{}", hash('e')),
            substrate_protocol: 1,
            portable: lane.clone(),
            native: AuthorityCalibrationLane {
                qualification_attestation_hash: hash('8'),
                ..lane.clone()
            },
            core_seed: lane.clone(),
            substrate: lane,
            substrate_product_witness_hash: hash('f'),
            substrate_product_witness_signer_fingerprint: fingerprint.clone(),
            source_recipes: AuthorityCalibrationRecipes {
                portable_build: source_recipe.clone(),
                portable_capture: source_recipe.clone(),
                portable_qualification: source_recipe.clone(),
                native_build: source_recipe.clone(),
                native_capture: source_recipe.clone(),
                native_qualification: source_recipe.clone(),
                core_seed_build: source_recipe.clone(),
                core_seed_capture: source_recipe.clone(),
                core_seed_qualification: source_recipe.clone(),
                substrate_build: source_recipe.clone(),
                substrate_qualification: source_recipe,
            },
            recipes: AuthorityCalibrationRecipes {
                portable_build: recipe.clone(),
                portable_capture: recipe.clone(),
                portable_qualification: recipe.clone(),
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
        assert_ne!(
            evidence.source_recipes.native_build,
            evidence.recipes.native_build
        );
        let attestation = evidence.sign_attestation(&signer).unwrap();
        attestation
            .verify_with_key(&signer.verifying_key())
            .unwrap();
        assert_eq!(
            AuthorityCalibrationEvidence::from_attestation(&attestation).unwrap(),
            evidence
        );
        let mut collapsed = evidence.clone();
        collapsed.native = collapsed.portable.clone();
        assert!(collapsed.validate().is_err());
        let mut legacy = serde_json::to_value(&evidence).unwrap();
        legacy["schema"] = serde_json::json!("ryeos.bundle_publication_authority_calibration.v1");
        assert!(
            serde_json::from_value::<AuthorityCalibrationEvidence>(legacy)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut incomplete = serde_json::to_value(&evidence).unwrap();
        incomplete.as_object_mut().unwrap().remove("portable");
        assert!(serde_json::from_value::<AuthorityCalibrationEvidence>(incomplete).is_err());
        let mut incomplete_recipes = serde_json::to_value(&evidence).unwrap();
        incomplete_recipes["source_recipes"]
            .as_object_mut()
            .unwrap()
            .remove("native_build");
        assert!(
            serde_json::from_value::<AuthorityCalibrationEvidence>(incomplete_recipes).is_err()
        );
        let mut changed = attestation.clone();
        changed.evidence["execution_environment"]["node_policy_generation_hash"] =
            serde_json::json!(hash('9'));
        assert!(AuthorityCalibrationEvidence::from_attestation(&changed).is_err());
        let mut changed = attestation.clone();
        changed.evidence["execution_environment"]["static_link_inputs"]["selection"]["product_witness_hash"] =
            serde_json::json!(hash('8'));
        assert!(AuthorityCalibrationEvidence::from_attestation(&changed).is_err());
        assert_ne!(
            attestation.claim,
            super::super::attestation::BUNDLE_GENERATION_RELEASE_CLAIM
        );
    }
}
