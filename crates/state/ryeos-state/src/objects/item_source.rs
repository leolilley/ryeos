//! ItemSource — an item loaded into a project with integrity metadata.

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

/// A single file captured from a project snapshot.
#[derive(Debug, Clone)]
pub struct ItemSource {
    /// Canonical project-relative path (e.g. ".ai/directives/test/simple.md").
    pub item_ref: String,
    /// SHA-256 hash of the content blob in CAS.
    pub content_blob_hash: String,
    /// Integrity check identifier (e.g. "ed25519:<fingerprint>").
    pub integrity: String,
    /// Optional signature metadata.
    pub signature_info: Option<Value>,
    /// Unix permission bits for executable binaries (e.g. `0o755`).
    /// `None` for non-executable items (text files, configs, etc.).
    /// Ingested from file mode when the exec bit is set.
    pub mode: Option<u32>,
}

impl ItemSource {
    /// Serialize to a CAS JSON object.
    pub fn to_value(&self) -> Value {
        json!({
            "kind": "item_source",
            "item_ref": self.item_ref,
            "content_blob_hash": self.content_blob_hash,
            "integrity": self.integrity,
            "signature_info": self.signature_info,
            "mode": self.mode,
        })
    }

    /// Deserialize from a CAS JSON value.
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: String,
            item_ref: String,
            content_blob_hash: String,
            integrity: String,
            signature_info: Value,
            mode: Value,
        }

        let wire: Wire =
            serde_json::from_value(value.clone()).context("failed to deserialize item_source")?;
        if wire.kind != "item_source" {
            anyhow::bail!(
                "item_source kind mismatch: expected item_source, got {}",
                wire.kind
            );
        }
        super::validate_canonical_project_relative_path(&wire.item_ref)
            .context("item_source item_ref is not a canonical project-relative path")?;
        super::validate_trimmed_control_free("item_source integrity", &wire.integrity, false)?;
        super::thread_snapshot::validate_canonical_hash(
            "content_blob_hash",
            &wire.content_blob_hash,
        )?;
        let signature_info = match wire.signature_info {
            Value::Null => None,
            value @ Value::Object(_) => Some(value),
            _ => anyhow::bail!("item_source signature_info must be an object or null"),
        };
        let mode = match wire.mode {
            Value::Null => None,
            Value::Number(number) => Some(
                number
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| anyhow::anyhow!("item_source mode must be a u32 or null"))?,
            ),
            _ => anyhow::bail!("item_source mode must be a u32 or null"),
        };
        if let Some(mode) = mode {
            if mode > 0o7777 || mode & 0o111 == 0 {
                anyhow::bail!(
                    "item_source mode must be executable Unix permission bits in 0o0000..=0o7777"
                );
            }
        }
        Ok(Self {
            item_ref: wire.item_ref,
            content_blob_hash: wire.content_blob_hash,
            integrity: wire.integrity,
            signature_info,
            mode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let original = ItemSource {
            item_ref: ".ai/directives/test/simple.md".to_string(),
            content_blob_hash: "ab".repeat(32),
            integrity: "ed25519:fp123".to_string(),
            signature_info: None,
            mode: None,
        };
        let value = original.to_value();
        let restored = ItemSource::from_value(&value).unwrap();
        assert_eq!(restored.item_ref, original.item_ref);
        assert_eq!(restored.content_blob_hash, original.content_blob_hash);

        let mut missing_signature_info = value.clone();
        missing_signature_info
            .as_object_mut()
            .unwrap()
            .remove("signature_info");
        assert!(ItemSource::from_value(&missing_signature_info).is_err());
        let mut missing_mode = value;
        missing_mode.as_object_mut().unwrap().remove("mode");
        assert!(ItemSource::from_value(&missing_mode).is_err());
    }
}
