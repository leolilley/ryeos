//! Generic, content-addressed restore manifest.
//!
//! The state layer deliberately treats the restore contract and every attached
//! input as opaque bytes. Its authority is structural: the publisher, exact
//! restore bytes, and complete input closure are committed by the manifest
//! object hash and resolve without consulting a project runtime.

use std::collections::BTreeSet;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STATE_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const STATE_MANIFEST_KIND: &str = "state_manifest";
pub const MAX_STATE_MANIFEST_OBJECTS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateManifestBlob {
    pub name: String,
    pub media_type: String,
    pub blob_hash: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateManifest {
    pub schema: u32,
    pub kind: String,
    pub contract: String,
    pub publisher_chain_root_id: String,
    pub publisher_thread_id: String,
    pub restore: StateManifestBlob,
    pub objects: Vec<StateManifestBlob>,
}

impl StateManifest {
    pub fn new(
        contract: String,
        publisher_chain_root_id: String,
        publisher_thread_id: String,
        restore: StateManifestBlob,
        objects: Vec<StateManifestBlob>,
    ) -> anyhow::Result<Self> {
        let manifest = Self {
            schema: STATE_MANIFEST_SCHEMA_VERSION,
            kind: STATE_MANIFEST_KIND.to_string(),
            contract,
            publisher_chain_root_id,
            publisher_thread_id,
            restore,
            objects,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("StateManifest serialization cannot fail")
    }

    pub fn from_current_value(value: Value) -> anyhow::Result<Self> {
        let manifest: Self =
            serde_json::from_value(value).context("deserialize current state_manifest")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        super::validate_object_kind(&self.kind, STATE_MANIFEST_KIND)?;
        if self.schema != STATE_MANIFEST_SCHEMA_VERSION {
            anyhow::bail!(
                "state_manifest is not the exact current contract: stored schema={}, current schema={STATE_MANIFEST_SCHEMA_VERSION}",
                self.schema
            );
        }
        super::validate_trimmed_control_free("state_manifest contract", &self.contract, false)?;
        super::validate_trimmed_control_free(
            "state_manifest publisher_chain_root_id",
            &self.publisher_chain_root_id,
            false,
        )?;
        super::validate_trimmed_control_free(
            "state_manifest publisher_thread_id",
            &self.publisher_thread_id,
            false,
        )?;
        validate_blob(&self.restore, "restore")?;
        if self.restore.name != "restore" {
            anyhow::bail!("state_manifest restore entry must be named `restore`");
        }
        if self.restore.media_type != "application/json" {
            anyhow::bail!("state_manifest restore entry must use application/json");
        }
        if self.objects.len() > MAX_STATE_MANIFEST_OBJECTS {
            anyhow::bail!(
                "state_manifest has {} objects; maximum is {MAX_STATE_MANIFEST_OBJECTS}",
                self.objects.len()
            );
        }
        let mut names = BTreeSet::new();
        names.insert(self.restore.name.as_str());
        for object in &self.objects {
            validate_blob(object, "object")?;
            if !names.insert(object.name.as_str()) {
                anyhow::bail!(
                    "state_manifest contains duplicate blob name {:?}",
                    object.name
                );
            }
        }
        Ok(())
    }
}

fn validate_blob(blob: &StateManifestBlob, role: &str) -> anyhow::Result<()> {
    super::validate_trimmed_control_free(
        &format!("state_manifest {role} name"),
        &blob.name,
        false,
    )?;
    super::validate_trimmed_control_free(
        &format!("state_manifest {role} media_type"),
        &blob.media_type,
        false,
    )?;
    super::thread_snapshot::validate_canonical_hash(
        &format!("state_manifest {role} blob_hash"),
        &blob.blob_hash,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(name: &str, byte: u8) -> StateManifestBlob {
        StateManifestBlob {
            name: name.to_string(),
            media_type: "application/octet-stream".to_string(),
            blob_hash: format!("{byte:02x}").repeat(32),
            size_bytes: 1,
        }
    }

    #[test]
    fn exact_current_manifest_roundtrips_and_rejects_duplicate_names() {
        let mut restore = blob("restore", 0xab);
        restore.media_type = "application/json".to_string();
        let manifest = StateManifest::new(
            "example.restore.v1".to_string(),
            "T-root".to_string(),
            "T-leaf".to_string(),
            restore.clone(),
            vec![blob("engine", 0xcd)],
        )
        .unwrap();
        assert_eq!(
            StateManifest::from_current_value(manifest.to_value()).unwrap(),
            manifest
        );
        assert!(
            StateManifest::new(
                "example.restore.v1".to_string(),
                "T-root".to_string(),
                "T-leaf".to_string(),
                restore,
                vec![blob("restore", 0xef)],
            )
            .is_err()
        );
    }
}
