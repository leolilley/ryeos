//! Shared records for retained signed Bundle artifact bytes.
//!
//! These are CAS provenance data, not admission, qualification or provider
//! authority. Each consuming owner validates its own exact selection and
//! retains the complete closure before releasing its publication stage.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedSignedBundleItem {
    pub resolved_ref: String,
    pub bundle_name: String,
    pub signer_fingerprint: String,
    pub signed_blob_hash: String,
    pub raw_content_digest: String,
    pub signature_envelope: RetainedSignatureEnvelope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedSignatureEnvelope {
    pub prefix: String,
    pub suffix: Option<String>,
    pub after_shebang: bool,
}

impl RetainedSignatureEnvelope {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        let valid = |value: &str| {
            !value.is_empty()
                && value.len() <= 16
                && value.bytes().all(|byte| byte.is_ascii_graphic())
                && !value.contains("ryeos:signed")
        };
        if !valid(&self.prefix) || self.suffix.as_deref().is_some_and(|suffix| !valid(suffix)) {
            anyhow::bail!("retained Bundle signature envelope is invalid");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedSignedBundleManifest {
    pub bundle_name: String,
    pub signer_fingerprint: String,
    pub signed_blob_hash: String,
    pub body_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedBundleExecutorSource {
    pub bundle_name: String,
    pub item_ref: String,
    pub target_triple: String,
    pub signer_fingerprint: String,
    pub signed_manifest_ref_blob_hash: String,
    /// Canonical manifest bytes are retained as a blob to avoid retaining
    /// every unrelated payload named by the complete executor inventory.
    pub manifest_object_blob_hash: String,
    pub item_source_object_hash: String,
    pub signed_sidecar_blob_hash: String,
    pub payload_blob_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedBundleSignerKey {
    pub signer_fingerprint: String,
    /// Historical public Ed25519 verifier, never a fresh admission grant.
    pub verifying_key: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_signed_item_preserves_existing_provenance_wire_shape() {
        let value = json!({
            "resolved_ref": "config:fixtures/recipe",
            "bundle_name": "fixtures",
            "signer_fingerprint": "a".repeat(64),
            "signed_blob_hash": "b".repeat(64),
            "raw_content_digest": "c".repeat(64),
            "signature_envelope": {"prefix": "#", "suffix": null, "after_shebang": false},
        });
        let item: RetainedSignedBundleItem = serde_json::from_value(value.clone()).unwrap();
        item.signature_envelope.validate().unwrap();
        assert_eq!(serde_json::to_value(item).unwrap(), value);
        let mut unexpected = value;
        unexpected["authority"] = json!("qualified");
        assert!(serde_json::from_value::<RetainedSignedBundleItem>(unexpected).is_err());
    }

    #[test]
    fn signature_envelope_remains_bounded_without_granting_authority() {
        let mut envelope = RetainedSignatureEnvelope {
            prefix: "#".into(),
            suffix: None,
            after_shebang: false,
        };
        envelope.validate().unwrap();
        for prefix in ["", "ryeos:signed", "has space", "abcdefghijklmnopq"] {
            envelope.prefix = prefix.into();
            assert!(envelope.validate().is_err());
        }
    }
}
