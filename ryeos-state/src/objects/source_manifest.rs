//! SourceManifest — mapping of item references to their content blob hashes.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A manifest listing all items in a project source snapshot.
///
/// Maps item reference → content blob hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    /// Map of item_ref to content blob hash.
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
        let item_source_hashes = value
            .get("item_source_hashes")
            .and_then(|v| v.as_object())
            .ok_or_else(|| anyhow::anyhow!("missing item_source_hashes"))?
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect();
        Ok(Self { item_source_hashes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut map = HashMap::new();
        map.insert("directive:test/simple".to_string(), "ab".repeat(32));
        let original = SourceManifest { item_source_hashes: map };
        let value = original.to_value();
        let restored = SourceManifest::from_value(&value).unwrap();
        assert_eq!(restored.item_source_hashes.len(), 1);
        assert!(restored.item_source_hashes.contains_key("directive:test/simple"));
    }
}
