//! Pure retained calibration identities and closed node-policy validation.
//!
//! Live product admission, grants, CAS custody and calibration execution remain
//! with the caller's actual operation owners; these records convey no authority.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

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
        validate_calibration_hash("environment product witness", &self.product_witness_hash)?;
        validate_calibration_hash(
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
        validate_calibration_hash(
            "environment owner",
            self.owner_principal
                .strip_prefix("fp:")
                .context("environment owner must be a fingerprint principal")?,
        )?;
        self.producer.validate()?;
        if !self.recipe_ref.starts_with("config:") {
            bail!("environment producer recipe must be an exact Config ref");
        }
        validate_calibration_hash(
            "environment producer recipe content",
            &self.recipe_raw_content_digest,
        )?;
        validate_calibration_hash(
            "environment qualification policy",
            &self.qualification_policy_item_hash,
        )?;
        validate_calibration_hash(
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
        validate_calibration_hash(
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
            validate_calibration_hash(
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

pub fn validate_calibration_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be a lowercase sha256 digest");
    }
    Ok(())
}
