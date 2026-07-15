//! SourceManifest — mapping of project-relative paths to source objects.

use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

/// A manifest listing all items in a project source snapshot.
///
/// Maps canonical project-relative path → ItemSource object hash.
#[derive(Debug, Clone)]
pub struct SourceManifest {
    /// Map of canonical project-relative path to ItemSource object hash.
    pub item_source_hashes: HashMap<String, String>,
}

impl SourceManifest {
    /// Serialize to a CAS JSON object.
    pub fn to_value(&self) -> Value {
        json!({
            "kind": "source_manifest",
            "item_source_hashes": self.item_source_hashes,
        })
    }

    /// Deserialize from a CAS JSON value.
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: String,
            item_source_hashes: HashMap<String, String>,
        }

        let wire: Wire = serde_json::from_value(value.clone())
            .context("failed to deserialize source_manifest")?;
        if wire.kind != "source_manifest" {
            anyhow::bail!(
                "source_manifest kind mismatch: expected source_manifest, got {}",
                wire.kind
            );
        }
        for (item_ref, hash) in &wire.item_source_hashes {
            super::validate_canonical_project_relative_path(item_ref).with_context(|| {
                format!("source_manifest contains invalid project-relative path {item_ref:?}")
            })?;
            super::thread_snapshot::validate_canonical_hash("item source hash", hash)?;
        }
        Ok(Self {
            item_source_hashes: wire.item_source_hashes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut map = HashMap::new();
        map.insert(".ai/directives/test/simple.md".to_string(), "ab".repeat(32));
        let original = SourceManifest {
            item_source_hashes: map,
        };
        let value = original.to_value();
        let restored = SourceManifest::from_value(&value).unwrap();
        assert_eq!(restored.item_source_hashes.len(), 1);
        assert!(restored
            .item_source_hashes
            .contains_key(".ai/directives/test/simple.md"));
    }
}
