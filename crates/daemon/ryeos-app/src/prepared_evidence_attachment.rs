//! Retained evidence attachment metadata and mechanical validation.
//!
//! This serializable value does not authorize an event, acquire bytes or admit
//! execution. Its original acquisition and publication owners remain separate.

use serde::{Deserialize, Serialize};

const MAX_REF_BINDING_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedEvidenceAttachment {
    pub binding_id: String,
    pub bundle_id: String,
    pub event_kind: String,
    pub chain_id: String,
    pub event_hash: String,
    pub attachment_name: String,
    pub blob_hash: String,
    pub size_bytes: u64,
    pub media_type: Option<String>,
    pub target: String,
    pub destination_path: String,
    pub access: ryeos_handler_protocol::EvidenceAttachmentAccessWire,
    pub manifest_hash: String,
    pub binding_digest: String,
}

impl PreparedEvidenceAttachment {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !valid_ref_binding_name(&self.binding_id) {
            anyhow::bail!("prepared evidence attachment binding_id is not canonical");
        }
        for (label, value) in [
            ("bundle_id", self.bundle_id.as_str()),
            ("event_kind", self.event_kind.as_str()),
            ("chain_id", self.chain_id.as_str()),
            ("attachment_name", self.attachment_name.as_str()),
        ] {
            ryeos_state::objects::validate_bundle_identifier(label, value)?;
        }
        for (label, value) in [
            ("event_hash", self.event_hash.as_str()),
            ("blob_hash", self.blob_hash.as_str()),
            ("manifest_hash", self.manifest_hash.as_str()),
            ("binding_digest", self.binding_digest.as_str()),
        ] {
            if !lillux::valid_hash(value) {
                anyhow::bail!("prepared evidence attachment {label} is not a canonical hash");
            }
        }
        if self.size_bytes > ryeos_state::objects::MAX_BUNDLE_EVENT_ATTACHMENT_BYTES {
            anyhow::bail!("prepared evidence attachment exceeds the bundle-event file bound");
        }
        ryeos_state::objects::validate_canonical_project_relative_path(&self.destination_path)?;
        if self.target.is_empty()
            || !self.target.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
        {
            anyhow::bail!("prepared evidence attachment target is not canonical");
        }
        if self.access != ryeos_handler_protocol::EvidenceAttachmentAccessWire::ReadOnly {
            anyhow::bail!("prepared evidence attachment is not read-only");
        }
        if self.binding_digest != self.reproduce_binding_digest()? {
            anyhow::bail!("prepared evidence attachment binding digest changed");
        }
        Ok(())
    }

    pub fn reproduce_binding_digest(&self) -> anyhow::Result<String> {
        let value = serde_json::json!({
            "schema": "ryeos.evidence_attachment_binding.v1",
            "binding_id": self.binding_id,
            "bundle_id": self.bundle_id,
            "event_kind": self.event_kind,
            "chain_id": self.chain_id,
            "event_hash": self.event_hash,
            "attachment_name": self.attachment_name,
            "blob_hash": self.blob_hash,
            "size_bytes": self.size_bytes,
            "media_type": self.media_type,
            "target": self.target,
            "destination_path": self.destination_path,
            "access": self.access,
            "manifest_hash": self.manifest_hash,
        });
        let canonical = lillux::canonical_json(&value)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }
}

pub fn valid_ref_binding_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_REF_BINDING_NAME_BYTES {
        return false;
    }
    let mut segments = name.split('_');
    let Some(first) = segments.next() else {
        return false;
    };
    first.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && first
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && segments.all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}
