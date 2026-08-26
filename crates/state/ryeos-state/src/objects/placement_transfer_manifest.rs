//! Bounded CAS manifest for the source operands of one placement transfer.
//!
//! This object is a durable sync-job operand, not chain or placement
//! authority.  It lets a target fetch a complete source chain plus the
//! source's secret-free operational launch ledger without embedding large
//! JSON in an HTTP request or `sync_jobs.operation_json`.

use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PLACEMENT_TRANSFER_MANIFEST_KIND: &str = "placement_transfer_manifest";
pub const PLACEMENT_TRANSFER_MANIFEST_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementTransferManifest {
    pub schema: u32,
    pub kind: String,
    pub operation_id: String,
    pub owner_principal: String,
    pub chain_root_id: String,
    pub origin_site_id: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub source_placement_thread_id: String,
    pub successor_placement_thread_id: String,
    pub source_chain_head_hash: String,
    pub source_last_event_hash: String,
    pub checkpoint_manifest_hash: String,
    pub source_launch_capsule_hash: String,
    pub source_launch_metadata_blob_hash: String,
    pub source_launch_metadata_size_bytes: u64,
}

impl PlacementTransferManifest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation_id: String,
        owner_principal: String,
        chain_root_id: String,
        origin_site_id: String,
        source_site_id: String,
        target_site_id: String,
        source_placement_thread_id: String,
        successor_placement_thread_id: String,
        source_chain_head_hash: String,
        source_last_event_hash: String,
        checkpoint_manifest_hash: String,
        source_launch_capsule_hash: String,
        source_launch_metadata_blob_hash: String,
        source_launch_metadata_size_bytes: u64,
    ) -> anyhow::Result<Self> {
        let manifest = Self {
            schema: PLACEMENT_TRANSFER_MANIFEST_SCHEMA,
            kind: PLACEMENT_TRANSFER_MANIFEST_KIND.to_owned(),
            operation_id,
            owner_principal,
            chain_root_id,
            origin_site_id,
            source_site_id,
            target_site_id,
            source_placement_thread_id,
            successor_placement_thread_id,
            source_chain_head_hash,
            source_last_event_hash,
            checkpoint_manifest_hash,
            source_launch_capsule_hash,
            source_launch_metadata_blob_hash,
            source_launch_metadata_size_bytes,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        super::validate_object_kind(&self.kind, PLACEMENT_TRANSFER_MANIFEST_KIND)?;
        if self.schema != PLACEMENT_TRANSFER_MANIFEST_SCHEMA {
            bail!(
                "placement_transfer_manifest is not the exact current contract: stored schema={}, current schema={PLACEMENT_TRANSFER_MANIFEST_SCHEMA}",
                self.schema
            );
        }
        for (label, value) in [
            ("owner principal", self.owner_principal.as_str()),
            ("chain root", self.chain_root_id.as_str()),
            ("origin site", self.origin_site_id.as_str()),
            ("source site", self.source_site_id.as_str()),
            ("target site", self.target_site_id.as_str()),
            (
                "source placement thread",
                self.source_placement_thread_id.as_str(),
            ),
            (
                "successor placement thread",
                self.successor_placement_thread_id.as_str(),
            ),
        ] {
            super::validate_trimmed_control_free(
                &format!("placement transfer {label}"),
                value,
                false,
            )?;
            if value.len() > 4096 {
                bail!("placement transfer {label} exceeds its byte ceiling");
            }
        }
        if self.origin_site_id.is_empty()
            || self.source_site_id == self.target_site_id
            || self.source_placement_thread_id == self.successor_placement_thread_id
        {
            bail!("placement transfer does not describe a cross-site successor");
        }
        for (label, hash) in [
            ("operation", self.operation_id.as_str()),
            ("source chain head", self.source_chain_head_hash.as_str()),
            ("source last event", self.source_last_event_hash.as_str()),
            (
                "checkpoint manifest",
                self.checkpoint_manifest_hash.as_str(),
            ),
            (
                "source launch capsule",
                self.source_launch_capsule_hash.as_str(),
            ),
            (
                "source launch metadata blob",
                self.source_launch_metadata_blob_hash.as_str(),
            ),
        ] {
            super::thread_snapshot::validate_canonical_hash(
                &format!("placement transfer {label}"),
                hash,
            )?;
        }
        if self.source_launch_metadata_size_bytes == 0
            || self.source_launch_metadata_size_bytes > super::MAX_PLACEMENT_RUNTIME_METADATA_BYTES
        {
            bail!("placement transfer source launch metadata exceeds its byte ceiling");
        }
        Ok(())
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn from_current_value(value: Value) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_value(value)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&self.to_value()?)?.as_bytes(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrips_and_refuses_same_site_transfer() {
        let manifest = PlacementTransferManifest::new(
            "1".repeat(64),
            "owner".into(),
            "T-root".into(),
            "site:a".into(),
            "site:a".into(),
            "site:b".into(),
            "T-source".into(),
            "T-target".into(),
            "2".repeat(64),
            "3".repeat(64),
            "4".repeat(64),
            "5".repeat(64),
            "6".repeat(64),
            4096,
        )
        .unwrap();
        assert_eq!(
            PlacementTransferManifest::from_current_value(manifest.to_value().unwrap()).unwrap(),
            manifest
        );
        let mut same_site = manifest;
        same_site.target_site_id = same_site.source_site_id.clone();
        assert!(same_site.validate().is_err());
    }
}
