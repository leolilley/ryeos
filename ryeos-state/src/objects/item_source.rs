//! ItemSource — an item loaded into a project with integrity metadata.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A single item loaded into a project (directive, tool, knowledge, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemSource {
    /// The item reference (e.g. "directive:test/simple").
    pub item_ref: String,
    /// SHA-256 hash of the content blob in CAS.
    pub content_blob_hash: String,
    /// Integrity check identifier (e.g. "ed25519:<fingerprint>").
    pub integrity: String,
    /// Optional signature metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_info: Option<Value>,
}

impl ItemSource {
    /// Serialize to a CAS JSON object.
    pub fn to_value(&self) -> Value {
        let mut v = json!({
            "kind": "item_source",
            "item_ref": self.item_ref,
            "content_blob_hash": self.content_blob_hash,
            "integrity": self.integrity,
        });
        if let Some(ref sig) = self.signature_info {
            v.as_object_mut()
                .unwrap()
                .insert("signature_info".into(), sig.clone());
        }
        v
    }

    /// Deserialize from a CAS JSON value.
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        Ok(Self {
            item_ref: value
                .get("item_ref")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing item_ref"))?
                .to_string(),
            content_blob_hash: value
                .get("content_blob_hash")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing content_blob_hash"))?
                .to_string(),
            integrity: value
                .get("integrity")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing integrity"))?
                .to_string(),
            signature_info: value.get("signature_info").cloned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let original = ItemSource {
            item_ref: "directive:test/simple".to_string(),
            content_blob_hash: "ab".repeat(32),
            integrity: "ed25519:fp123".to_string(),
            signature_info: None,
        };
        let value = original.to_value();
        let restored = ItemSource::from_value(&value).unwrap();
        assert_eq!(restored.item_ref, original.item_ref);
        assert_eq!(restored.content_blob_hash, original.content_blob_hash);
    }
}
