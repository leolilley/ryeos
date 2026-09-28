//! Provider-neutral challenge and measurement for a restored guest owner tree.
//!
//! The guest returns measured bytes, not a provider identity. The controller
//! must independently bind its authenticated run/occurrence to the retained
//! snapshot locator and admitted verifier artifact before accepting this result.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::canonical_json;
use crate::runtime_snapshot::{RuntimeSnapshotIntent, RuntimeSnapshotLocator};

pub const RESTORED_OWNER_MEASUREMENT_PROTOCOL: &str = "ryeos.restored-owner-measurement.v1";
pub const MAX_RESTORED_OWNER_CHALLENGE_BYTES: usize = 4096;
pub const MAX_RESTORED_OWNER_RESULT_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredOwnerChallenge {
    pub schema: u32,
    pub protocol: String,
    pub operation_id: String,
    pub snapshot_id: String,
    pub restored_occurrence_id: String,
    pub nonce_hex: String,
}

impl RestoredOwnerChallenge {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.protocol == RESTORED_OWNER_MEASUREMENT_PROTOCOL,
            "unsupported restored-owner challenge"
        );
        require_hash(&self.operation_id, "operation")?;
        require_hash(&self.nonce_hex, "nonce")?;
        for (label, value) in [
            ("snapshot", &self.snapshot_id),
            ("restored occurrence", &self.restored_occurrence_id),
        ] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 256
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
                "restored-owner {label} identity is invalid"
            );
        }
        ensure!(
            canonical_json(self)?.len() <= MAX_RESTORED_OWNER_CHALLENGE_BYTES,
            "restored-owner challenge exceeds its bound"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }

    pub fn validate_for(
        &self,
        intent: &RuntimeSnapshotIntent,
        locator: &RuntimeSnapshotLocator,
    ) -> Result<()> {
        self.validate()?;
        locator.validate_for(intent)?;
        ensure!(
            self.operation_id == intent.operation_id
                && self.snapshot_id == locator.snapshot_id
                && self.restored_occurrence_id != locator.source_occurrence_id,
            "restored-owner challenge differs from bound snapshot or source"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredOwnerMeasurement {
    pub schema: u32,
    pub protocol: String,
    pub challenge_digest: String,
    pub manifest_hash: String,
    pub owner_executable_sha256: String,
    pub controller_public_root: String,
}

impl RestoredOwnerMeasurement {
    pub fn validate_for(
        &self,
        challenge: &RestoredOwnerChallenge,
        intent: &RuntimeSnapshotIntent,
    ) -> Result<()> {
        challenge.validate()?;
        intent.validate()?;
        ensure!(
            self.schema == 1
                && self.protocol == RESTORED_OWNER_MEASUREMENT_PROTOCOL
                && self.challenge_digest == challenge.digest()?
                && challenge.operation_id == intent.operation_id
                && self.manifest_hash == intent.guest_runtime_manifest_hash
                && self.owner_executable_sha256 == intent.owner_executable_sha256
                && self.controller_public_root == intent.controller_public_root,
            "restored owner measurement differs from exact retained product"
        );
        require_hash(&self.challenge_digest, "challenge")?;
        ensure!(
            canonical_json(self)?.len() <= MAX_RESTORED_OWNER_RESULT_BYTES,
            "restored owner measurement exceeds its bound"
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
        "restored-owner {label} digest is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn challenge_cannot_be_reused_for_another_snapshot_or_occurrence() {
        let challenge = RestoredOwnerChallenge {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: "1".repeat(64),
            snapshot_id: "snp-exact".into(),
            restored_occurrence_id: "sbx-exact".into(),
            nonce_hex: "2".repeat(64),
        };
        let digest = challenge.digest().unwrap();
        let mut changed = challenge.clone();
        changed.snapshot_id = "snp-other".into();
        assert_ne!(changed.digest().unwrap(), digest);
        changed = challenge.clone();
        changed.restored_occurrence_id = "sbx-other".into();
        assert_ne!(changed.digest().unwrap(), digest);
        changed = challenge;
        changed.nonce_hex = "3".repeat(64);
        assert_ne!(changed.digest().unwrap(), digest);
    }

    #[test]
    fn measurement_must_match_retained_product_and_fresh_challenge() {
        let mut intent = RuntimeSnapshotIntent {
            schema: 1,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "1".repeat(64)),
            provider_id: "provider".into(),
            source_occurrence_id: "source".into(),
            provider_group_id: "group".into(),
            production_profile_digest: "2".repeat(64),
            adapter_artifact_hash: "3".repeat(64),
            provider_spec_digest: "4".repeat(64),
            settings_digest: "5".repeat(64),
            product_witness_hash: "6".repeat(64),
            guest_runtime_manifest_hash: "7".repeat(64),
            owner_executable_sha256: "8".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode([3u8; 32])
            ),
            upload_sha256: "9".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: 42,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let challenge = RestoredOwnerChallenge {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: intent.operation_id.clone(),
            snapshot_id: "snapshot".into(),
            restored_occurrence_id: "restored".into(),
            nonce_hex: "a".repeat(64),
        };
        let measured = RestoredOwnerMeasurement {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            challenge_digest: challenge.digest().unwrap(),
            manifest_hash: intent.guest_runtime_manifest_hash.clone(),
            owner_executable_sha256: intent.owner_executable_sha256.clone(),
            controller_public_root: intent.controller_public_root.clone(),
        };
        measured.validate_for(&challenge, &intent).unwrap();
        let mut wrong = measured.clone();
        wrong.owner_executable_sha256 = "b".repeat(64);
        assert!(wrong.validate_for(&challenge, &intent).is_err());
        wrong = measured;
        wrong.challenge_digest = "b".repeat(64);
        assert!(wrong.validate_for(&challenge, &intent).is_err());
    }
}
