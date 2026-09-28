//! Durable, provider-neutral identity of one external runtime snapshot effect.
//!
//! The intent owns one provider-sequence attempt. A provider locator is
//! only an attempt result; restored bytes and runtime behavior require an
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
    pub source_occurrence_id: String,
    pub provider_group_id: String,
    /// Signed producer authority, distinct from the later placement binding
    /// that will name the resulting snapshot ID.
    pub production_profile_digest: String,
    pub adapter_artifact_hash: String,
    pub settings_digest: String,
    pub product_witness_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub owner_executable_sha256: String,
    pub controller_public_root: String,
    pub upload_sha256: String,
    pub upload_bytes: u64,
    pub attempt_deadline_ms: i64,
}

impl RuntimeSnapshotIntent {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            "unsupported runtime snapshot intent"
        );
        for (label, value) in [
            ("operation", &self.operation_id),
            ("production profile", &self.production_profile_digest),
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
        require_bounded_text(&self.source_occurrence_id, 256, "source occurrence")?;
        require_bounded_text(&self.provider_group_id, 256, "provider group")?;
        require_bounded_text(&self.controller_public_root, 128, "controller root")?;
        ensure!(
            self.controller_public_root.starts_with("ed25519:")
                && self.controller_public_root.len() == "ed25519:".len() + 44,
            "runtime snapshot controller root has no canonical envelope"
        );
        ensure!(
            (1..=MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES).contains(&self.upload_bytes)
                && self.attempt_deadline_ms > 0,
            "runtime snapshot upload or deadline exceeds its bound"
        );
        ensure!(
            self.operation_id == self.derived_operation_id()?,
            "runtime snapshot operation does not match exact transfer coordinates"
        );
        Ok(())
    }

    /// Stable across a retry with a later deadline, but unique to the exact
    /// admitted source and delivery bytes. A caller cannot mint a second
    /// attempt opportunity by choosing another operation ID.
    pub fn derived_operation_id(&self) -> Result<String> {
        let coordinates = (
            "ryeos.runtime-snapshot-operation.v1",
            &self.owner_principal,
            &self.provider_id,
            &self.source_occurrence_id,
            &self.provider_group_id,
            &self.production_profile_digest,
            &self.adapter_artifact_hash,
            &self.settings_digest,
            &self.product_witness_hash,
            &self.guest_runtime_manifest_hash,
            &self.owner_executable_sha256,
            &self.controller_public_root,
            &self.upload_sha256,
            self.upload_bytes,
        );
        Ok(hex::encode(Sha256::digest(canonical_json(&coordinates)?)))
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
        ensure!(
            self.source_occurrence_id == intent.source_occurrence_id
                && self.provider_group_id == intent.provider_group_id,
            "runtime snapshot locator changed its source occurrence"
        );
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
        let mut intent = RuntimeSnapshotIntent {
            schema: 1,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "2".repeat(64)),
            provider_id: "render-sandbox-early-access".into(),
            source_occurrence_id: "sbox-fixture-1".into(),
            provider_group_id: "sbg-fixture-1".into(),
            production_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            settings_digest: "5".repeat(64),
            product_witness_hash: "6".repeat(64),
            guest_runtime_manifest_hash: "7".repeat(64),
            owner_executable_sha256: "8".repeat(64),
            controller_public_root: format!("ed25519:{}", "A".repeat(44)),
            upload_sha256: "9".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: 42,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
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
            "production_profile_digest",
            "source_occurrence_id",
            "provider_group_id",
        ] {
            let mut changed = serde_json::to_value(&baseline).unwrap();
            changed[field] = serde_json::json!(if field == "controller_public_root" {
                format!("ed25519:{}", "B".repeat(44))
            } else if field == "source_occurrence_id" {
                "sbox-fixture-2".to_owned()
            } else if field == "provider_group_id" {
                "sbg-fixture-2".to_owned()
            } else {
                "a".repeat(64)
            });
            let mut changed: RuntimeSnapshotIntent = serde_json::from_value(changed).unwrap();
            assert!(changed.validate().is_err(), "{field} reused operation ID");
            changed.operation_id = changed.derived_operation_id().unwrap();
            assert_ne!(changed.digest().unwrap(), digest, "{field}");
        }
        let mut unknown = serde_json::to_value(&baseline).unwrap();
        unknown["credential"] = serde_json::json!("ambient");
        assert!(serde_json::from_value::<RuntimeSnapshotIntent>(unknown).is_err());
    }

    #[test]
    fn deadline_change_cannot_mint_another_attempt_opportunity() {
        let baseline = intent();
        let mut retried = baseline.clone();
        retried.attempt_deadline_ms += 100;
        assert_eq!(
            retried.derived_operation_id().unwrap(),
            baseline.operation_id
        );
        assert_ne!(retried.digest().unwrap(), baseline.digest().unwrap());
        retried.operation_id = "f".repeat(64);
        assert!(retried.validate().is_err());
    }

    #[test]
    fn locator_cannot_switch_source_after_attempt() {
        let intent = intent();
        let mut locator = RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: "snp-fixture-1".into(),
            provider_response_sha256: "a".repeat(64),
            adapter_observation_sha256: "b".repeat(64),
        };
        locator.validate_for(&intent).unwrap();
        locator.source_occurrence_id = "sbox-other".into();
        assert!(locator.validate_for(&intent).is_err());
    }
}
