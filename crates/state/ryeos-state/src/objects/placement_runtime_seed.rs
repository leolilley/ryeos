//! Immutable operational recovery seed for one admitted execution placement.
//!
//! The object is reachable from the authoritative continuation event.  It is
//! not a mutable runtime row or another session/head authority: it binds one
//! exact successor placement to the canonical, secret-free launch metadata
//! blob needed to reconstruct target-local runtime state after a crash.

use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PLACEMENT_RUNTIME_SEED_KIND: &str = "placement_runtime_seed";
pub const PLACEMENT_RUNTIME_SEED_SCHEMA: u32 = 1;
pub const MAX_PLACEMENT_RUNTIME_METADATA_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementRuntimeSeed {
    pub schema: u32,
    pub kind: String,
    pub operation_id: String,
    pub chain_root_id: String,
    pub source_placement_thread_id: String,
    pub successor_placement_thread_id: String,
    pub target_site_id: String,
    pub owner_principal: String,
    pub target_launch_capsule_hash: String,
    pub launch_metadata_blob_hash: String,
    pub launch_metadata_size_bytes: u64,
}

impl PlacementRuntimeSeed {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation_id: String,
        chain_root_id: String,
        source_placement_thread_id: String,
        successor_placement_thread_id: String,
        target_site_id: String,
        owner_principal: String,
        target_launch_capsule_hash: String,
        launch_metadata_blob_hash: String,
        launch_metadata_size_bytes: u64,
    ) -> anyhow::Result<Self> {
        let seed = Self {
            schema: PLACEMENT_RUNTIME_SEED_SCHEMA,
            kind: PLACEMENT_RUNTIME_SEED_KIND.to_owned(),
            operation_id,
            chain_root_id,
            source_placement_thread_id,
            successor_placement_thread_id,
            target_site_id,
            owner_principal,
            target_launch_capsule_hash,
            launch_metadata_blob_hash,
            launch_metadata_size_bytes,
        };
        seed.validate()?;
        Ok(seed)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        super::validate_object_kind(&self.kind, PLACEMENT_RUNTIME_SEED_KIND)?;
        if self.schema != PLACEMENT_RUNTIME_SEED_SCHEMA {
            bail!(
                "placement_runtime_seed is not the exact current contract: stored schema={}, current schema={PLACEMENT_RUNTIME_SEED_SCHEMA}",
                self.schema
            );
        }
        for (label, value) in [
            ("chain root", self.chain_root_id.as_str()),
            (
                "source placement thread",
                self.source_placement_thread_id.as_str(),
            ),
            (
                "successor placement thread",
                self.successor_placement_thread_id.as_str(),
            ),
            ("target site", self.target_site_id.as_str()),
            ("owner principal", self.owner_principal.as_str()),
        ] {
            super::validate_trimmed_control_free(
                &format!("placement runtime seed {label}"),
                value,
                false,
            )?;
            if value.len() > 4096 {
                bail!("placement runtime seed {label} exceeds its byte ceiling");
            }
        }
        if self.source_placement_thread_id == self.successor_placement_thread_id {
            bail!("placement runtime seed source and successor must differ");
        }
        for (label, hash) in [
            ("operation", self.operation_id.as_str()),
            (
                "target launch capsule",
                self.target_launch_capsule_hash.as_str(),
            ),
            (
                "launch metadata blob",
                self.launch_metadata_blob_hash.as_str(),
            ),
        ] {
            super::thread_snapshot::validate_canonical_hash(
                &format!("placement runtime seed {label}"),
                hash,
            )?;
        }
        if self.launch_metadata_size_bytes == 0
            || self.launch_metadata_size_bytes > MAX_PLACEMENT_RUNTIME_METADATA_BYTES
        {
            bail!(
                "placement runtime metadata size {} is outside 1..={MAX_PLACEMENT_RUNTIME_METADATA_BYTES}",
                self.launch_metadata_size_bytes
            );
        }
        Ok(())
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn from_current_value(value: Value) -> anyhow::Result<Self> {
        let seed: Self = serde_json::from_value(value)?;
        seed.validate()?;
        Ok(seed)
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

    fn seed() -> PlacementRuntimeSeed {
        PlacementRuntimeSeed::new(
            "1".repeat(64),
            "T-root".into(),
            "T-source".into(),
            "T-target".into(),
            "site:b".into(),
            "owner".into(),
            "2".repeat(64),
            "3".repeat(64),
            1024,
        )
        .unwrap()
    }

    #[test]
    fn exact_current_seed_roundtrips_and_is_bounded() {
        let seed = seed();
        assert_eq!(
            PlacementRuntimeSeed::from_current_value(seed.to_value().unwrap()).unwrap(),
            seed
        );
        let mut oversized = seed;
        oversized.launch_metadata_size_bytes = MAX_PLACEMENT_RUNTIME_METADATA_BYTES + 1;
        assert!(oversized.validate().is_err());
    }
}
