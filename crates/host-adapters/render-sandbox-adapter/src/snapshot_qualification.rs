//! Render-specific interpretation of an independently produced snapshot probe.
//!
//! Parsing and matching this shape grants no lifecycle capability. The daemon
//! must first authenticate a current, published product qualification and its
//! admitted verifier execution, then join these fields to the signed binding.

use anyhow::{Result, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use ryeos_external_execution_contract::{
    LIFECYCLE_ADAPTER_PROTOCOL, LifecycleRuntimeProbeRequest, LifecycleRuntimeProbeResponse,
};

use crate::{ADAPTER_ID, RenderPlan, Settings};

pub(crate) fn interpret_authenticated_request(
    request: &LifecycleRuntimeProbeRequest,
    settings: &Settings,
) -> Result<LifecycleRuntimeProbeResponse> {
    request.validate()?;
    ensure!(
        request.adapter_id == ADAPTER_ID
            && request.source.manifest_hash
                == request
                    .probe_evidence
                    .get("guest_runtime_manifest_hash")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
        "runtime probe request differs from exact Render adapter or source"
    );
    let root_b64 = request
        .source
        .controller_public_root
        .strip_prefix("ed25519:")
        .ok_or_else(|| anyhow::anyhow!("runtime probe source has no controller public root"))?;
    let root_bytes = base64::engine::general_purpose::STANDARD.decode(root_b64)?;
    let root_bytes: [u8; 32] = root_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("runtime probe controller root has wrong length"))?;
    ensure!(
        base64::engine::general_purpose::STANDARD.encode(root_bytes) == root_b64
            && lillux::sha256_hex(hex::encode(root_bytes).as_bytes())
                == request.source.controller_root_blob_sha256,
        "runtime probe controller root differs from authenticated product"
    );
    let probe = RenderSnapshotProbe::from_probe_evidence(&request.probe_evidence)?;
    probe.validate_for(
        settings,
        &SnapshotExpectation {
            product_witness_hash: &request.product_witness_hash,
            guest_runtime_manifest_hash: &request.source.manifest_hash,
            controller_public_root: &request.source.controller_public_root,
            account: &request.account,
            binding_hash: &request.binding_hash,
            installed_owner_hash: &request.source.owner_executable_sha256,
        },
    )?;
    Ok(LifecycleRuntimeProbeResponse {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        adapter_id: ADAPTER_ID.into(),
        request_digest: request.digest()?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderSnapshotProbe {
    pub schema: u32,
    pub product_witness_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub controller_public_root: String,
    pub owner_id: String,
    pub account: String,
    pub snapshot_id: String,
    pub snapshot_kind: String,
    pub plan: RenderPlan,
    pub region: String,
    pub binding_hash: String,
    pub restored_tree_manifest_hash: String,
    pub installed_owner_hash: String,
    pub installed_controller_public_root: String,
    pub signed_import_mode: u32,
    pub guest_package_mode: u32,
    pub lost_stream_survival_evidence_hash: String,
    pub authenticated_ready_evidence_hash: String,
    pub whole_guest_termination_evidence_hash: String,
    pub writer_exclusion_evidence_hash: String,
}

pub(crate) struct SnapshotExpectation<'a> {
    pub product_witness_hash: &'a str,
    pub guest_runtime_manifest_hash: &'a str,
    pub controller_public_root: &'a str,
    pub account: &'a str,
    pub binding_hash: &'a str,
    pub installed_owner_hash: &'a str,
}

impl RenderSnapshotProbe {
    /// Decode bounded probe data only after the caller has authenticated the
    /// published qualification and its independent verifier execution.
    pub(crate) fn from_probe_evidence(value: &serde_json::Value) -> Result<Self> {
        ensure!(
            lillux::canonical_json(value)?.len() <= 32 * 1024,
            "Render snapshot probe exceeds its byte bound"
        );
        Ok(serde_json::from_value(value.clone())?)
    }

    pub(crate) fn validate_for(
        &self,
        settings: &Settings,
        expected: &SnapshotExpectation<'_>,
    ) -> Result<()> {
        ensure!(self.schema == 1, "unsupported Render snapshot probe schema");
        for hash in [
            &self.product_witness_hash,
            &self.guest_runtime_manifest_hash,
            &self.binding_hash,
            &self.restored_tree_manifest_hash,
            &self.installed_owner_hash,
            &self.lost_stream_survival_evidence_hash,
            &self.authenticated_ready_evidence_hash,
            &self.whole_guest_termination_evidence_hash,
            &self.writer_exclusion_evidence_hash,
        ] {
            ensure!(
                lillux::valid_hash(hash),
                "Render snapshot probe has an invalid content identity"
            );
        }
        let root = self
            .controller_public_root
            .strip_prefix("ed25519:")
            .ok_or_else(|| {
                anyhow::anyhow!("Render snapshot probe has no controller public root")
            })?;
        let decoded = base64::engine::general_purpose::STANDARD.decode(root)?;
        let root_bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("Render snapshot controller root has the wrong length"))?;
        let root_key = lillux::crypto::VerifyingKey::from_bytes(&root_bytes)?;
        ensure!(
            !root_key.is_weak()
                && base64::engine::general_purpose::STANDARD.encode(root_key.to_bytes()) == root,
            "Render snapshot controller root is weak or noncanonical"
        );
        ensure!(
            self.product_witness_hash == expected.product_witness_hash
                && self.guest_runtime_manifest_hash == expected.guest_runtime_manifest_hash
                && self.controller_public_root == expected.controller_public_root
                && self.owner_id == settings.owner_id
                && self.account == expected.account
                && self.snapshot_id == settings.snapshot_id
                && self.snapshot_kind == "filesystem"
                && self.plan == settings.plan
                && self.region == settings.region
                && self.binding_hash == expected.binding_hash
                && self.installed_owner_hash == expected.installed_owner_hash
                && self.installed_controller_public_root == expected.controller_public_root
                && self.restored_tree_manifest_hash == expected.guest_runtime_manifest_hash
                // Render's observed upload is owner-writable 0600. The
                // one-shot owner accepts that exact mode beneath its private
                // activation root; 0400 would refuse a real upload.
                && self.signed_import_mode == 0o600
                && self.guest_package_mode == 0o600,
            "Render snapshot probe differs from the exact admitted placement or restored runtime"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_root() -> String {
        let signing = lillux::crypto::SigningKey::from_bytes(&[3; 32]);
        format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().to_bytes())
        )
    }

    fn probe() -> RenderSnapshotProbe {
        RenderSnapshotProbe {
            schema: 1,
            product_witness_hash: "1".repeat(64),
            guest_runtime_manifest_hash: "2".repeat(64),
            controller_public_root: public_root(),
            owner_id: "owner".into(),
            account: "account".into(),
            snapshot_id: "snp-exact".into(),
            snapshot_kind: "filesystem".into(),
            plan: RenderPlan::Starter,
            region: "oregon".into(),
            binding_hash: "4".repeat(64),
            restored_tree_manifest_hash: "2".repeat(64),
            installed_owner_hash: "5".repeat(64),
            installed_controller_public_root: public_root(),
            signed_import_mode: 0o600,
            guest_package_mode: 0o600,
            lost_stream_survival_evidence_hash: "6".repeat(64),
            authenticated_ready_evidence_hash: "7".repeat(64),
            whole_guest_termination_evidence_hash: "8".repeat(64),
            writer_exclusion_evidence_hash: "9".repeat(64),
        }
    }

    #[test]
    fn probe_requires_all_exact_placement_and_execution_coordinates() {
        let settings = Settings {
            schema: 2,
            owner_id: "owner".into(),
            plan: RenderPlan::Starter,
            region: "oregon".into(),
            snapshot_id: "snp-exact".into(),
            tls_roots_der_base64: Vec::new(),
        };
        let expected = SnapshotExpectation {
            product_witness_hash: &"1".repeat(64),
            guest_runtime_manifest_hash: &"2".repeat(64),
            controller_public_root: &public_root(),
            account: "account",
            binding_hash: &"4".repeat(64),
            installed_owner_hash: &"5".repeat(64),
        };
        let mut observed = probe();
        observed.validate_for(&settings, &expected).unwrap();
        observed.snapshot_id = "snp-other".into();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.snapshot_kind = "runtime".into();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.installed_controller_public_root = "ed25519:other".into();
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.signed_import_mode = 0o400;
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.signed_import_mode = 0o644;
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.guest_package_mode = 0o644;
        assert!(observed.validate_for(&settings, &expected).is_err());
        observed = probe();
        observed.whole_guest_termination_evidence_hash.clear();
        assert!(observed.validate_for(&settings, &expected).is_err());

        let mut untrusted = serde_json::to_value(probe()).unwrap();
        untrusted["qualified"] = serde_json::Value::Bool(true);
        assert!(RenderSnapshotProbe::from_probe_evidence(&untrusted).is_err());
        untrusted = serde_json::to_value(probe()).unwrap();
        untrusted["region"] = serde_json::Value::String("x".repeat(32 * 1024));
        assert!(RenderSnapshotProbe::from_probe_evidence(&untrusted).is_err());
    }

    #[test]
    fn offline_probe_interpretation_joins_protected_source_and_settings() {
        let public_root = public_root();
        let root_bytes = base64::engine::general_purpose::STANDARD
            .decode(public_root.strip_prefix("ed25519:").unwrap())
            .unwrap();
        let settings = Settings {
            schema: 2,
            owner_id: "owner".into(),
            plan: RenderPlan::Starter,
            region: "oregon".into(),
            snapshot_id: "snp-exact".into(),
            tls_roots_der_base64: Vec::new(),
        };
        let request = LifecycleRuntimeProbeRequest {
            schema: 1,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            adapter_id: ADAPTER_ID.into(),
            adapter_artifact_hash: "a".repeat(64),
            settings_digest: "b".repeat(64),
            binding_hash: "4".repeat(64),
            qualification_attestation_hash: "c".repeat(64),
            product_witness_hash: "1".repeat(64),
            account: "account".into(),
            source: ryeos_external_execution_contract::LifecycleRuntimeProbeSource {
                manifest_hash: "2".repeat(64),
                owner_executable_sha256: "5".repeat(64),
                controller_root_blob_sha256: lillux::sha256_hex(hex::encode(root_bytes).as_bytes()),
                controller_public_root: public_root,
            },
            probe_evidence: serde_json::to_value(probe()).unwrap(),
        };
        let response = interpret_authenticated_request(&request, &settings).unwrap();
        response.validate_for(&request).unwrap();
        let mut changed = request.clone();
        changed.source.owner_executable_sha256 = "d".repeat(64);
        assert!(interpret_authenticated_request(&changed, &settings).is_err());
        changed = request.clone();
        changed.source.controller_root_blob_sha256 = "e".repeat(64);
        assert!(interpret_authenticated_request(&changed, &settings).is_err());
        let mut changed_settings = settings;
        changed_settings.snapshot_id = "snp-other".into();
        assert!(interpret_authenticated_request(&request, &changed_settings).is_err());
    }
}
