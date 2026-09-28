//! Durable, provider-neutral identity of one external runtime snapshot effect.
//!
//! The intent owns one provider-contact opportunity. A provider locator is
//! only a contact result; restored bytes and runtime behavior require an
//! independent qualification rooted in the retained product witness.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::canonical_json;

pub const RUNTIME_SNAPSHOT_INTENT_SCHEMA: u32 = 1;
pub const RUNTIME_SNAPSHOT_RESULT_SCHEMA: u32 = 1;
pub const MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES: u64 = 64 * 1024 * 1024 + 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotIntent {
    pub schema: u32,
    pub operation_id: String,
    pub owner_principal: String,
    pub provider_id: String,
    pub binding_hash: String,
    pub adapter_artifact_hash: String,
    pub settings_digest: String,
    pub product_witness_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub owner_executable_sha256: String,
    pub controller_public_root: String,
    pub upload_sha256: String,
    pub upload_bytes: u64,
    pub contact_deadline_ms: i64,
}

impl RuntimeSnapshotIntent {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            "unsupported runtime snapshot intent"
        );
        for (label, value) in [
            ("operation", &self.operation_id),
            ("binding", &self.binding_hash),
            ("adapter", &self.adapter_artifact_hash),
            ("settings", &self.settings_digest),
            ("product witness", &self.product_witness_hash),
            ("guest runtime manifest", &self.guest_runtime_manifest_hash),
            ("owner executable", &self.owner_executable_sha256),
            ("upload", &self.upload_sha256),
        ] {
            require_hash(value, label)?;
        }
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .ok_or_else(|| anyhow::anyhow!("runtime snapshot owner principal is invalid"))?;
        require_hash(owner, "owner")?;
        require_bounded_text(&self.provider_id, 128, "provider")?;
        require_bounded_text(&self.controller_public_root, 128, "controller root")?;
        ensure!(
            self.controller_public_root.starts_with("ed25519:")
                && self.controller_public_root.len() == "ed25519:".len() + 44,
            "runtime snapshot controller root has no canonical envelope"
        );
        ensure!(
            (1..=MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES).contains(&self.upload_bytes)
                && self.contact_deadline_ms > 0,
            "runtime snapshot upload or deadline exceeds its bound"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotLocator {
    pub schema: u32,
    pub operation_id: String,
    pub intent_digest: String,
    pub source_occurrence_id: String,
    pub provider_group_id: String,
    pub snapshot_id: String,
    pub provider_response_sha256: String,
    pub adapter_observation_sha256: String,
}

impl RuntimeSnapshotLocator {
    pub fn validate_for(&self, intent: &RuntimeSnapshotIntent) -> Result<()> {
        intent.validate()?;
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_RESULT_SCHEMA
                && self.operation_id == intent.operation_id
                && self.intent_digest == intent.digest()?,
            "runtime snapshot locator contradicts its durable intent"
        );
        for (label, value) in [
            ("provider response", &self.provider_response_sha256),
            ("adapter observation", &self.adapter_observation_sha256),
        ] {
            require_hash(value, label)?;
        }
        for (label, value) in [
            ("source occurrence", &self.source_occurrence_id),
            ("provider group", &self.provider_group_id),
            ("snapshot", &self.snapshot_id),
        ] {
            require_bounded_text(value, 256, label)?;
        }
        Ok(())
    }
}

fn require_hash(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "runtime snapshot {label} digest is invalid"
    );
    Ok(())
}

fn require_bounded_text(value: &str, maximum: usize, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= maximum
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b':')),
        "runtime snapshot {label} is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> RuntimeSnapshotIntent {
        RuntimeSnapshotIntent {
            schema: 1,
            operation_id: "1".repeat(64),
            owner_principal: format!("fp:{}", "2".repeat(64)),
            provider_id: "render-sandbox-early-access".into(),
            binding_hash: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            settings_digest: "5".repeat(64),
            product_witness_hash: "6".repeat(64),
            guest_runtime_manifest_hash: "7".repeat(64),
            owner_executable_sha256: "8".repeat(64),
            controller_public_root: format!("ed25519:{}", "A".repeat(44)),
            upload_sha256: "9".repeat(64),
            upload_bytes: 1024,
            contact_deadline_ms: 42,
        }
    }

    #[test]
    fn source_and_delivery_changes_move_intent_identity() {
        let baseline = intent();
        let digest = baseline.digest().unwrap();
        for field in [
            "product_witness_hash",
            "guest_runtime_manifest_hash",
            "controller_public_root",
            "upload_sha256",
            "adapter_artifact_hash",
            "settings_digest",
        ] {
            let mut changed = serde_json::to_value(&baseline).unwrap();
            changed[field] = serde_json::json!(if field == "controller_public_root" {
                format!("ed25519:{}", "B".repeat(44))
            } else {
                "a".repeat(64)
            });
            let changed: RuntimeSnapshotIntent = serde_json::from_value(changed).unwrap();
            assert_ne!(changed.digest().unwrap(), digest, "{field}");
        }
        let mut unknown = serde_json::to_value(&baseline).unwrap();
        unknown["credential"] = serde_json::json!("ambient");
        assert!(serde_json::from_value::<RuntimeSnapshotIntent>(unknown).is_err());
    }
}
