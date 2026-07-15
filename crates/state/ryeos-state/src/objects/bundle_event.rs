//! BundleEventObject — immutable bundle fact stored in CAS.

use anyhow::Context;
use serde::{Deserialize, Serialize};

use super::thread_snapshot::parse_canonical_timestamp;
use super::{validate_object_kind, SCHEMA_VERSION};

pub const BUNDLE_EVENT_KIND: &str = "bundle_event";
/// Maximum canonical JSON size of one bundle event CAS object.
pub const MAX_BUNDLE_EVENT_SERIALIZED_BYTES: usize = 2 * 1024 * 1024;

/// Attribution captured for a bundle event append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BundleEventAttribution {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

/// Immutable bundle event object.
///
/// The CAS hash of the canonical JSON representation is the event hash. The
/// hash itself is intentionally not embedded in the object body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventObject {
    pub schema: u32,
    pub kind: String,
    pub bundle_id: String,
    pub event_kind: String,
    pub event_type: String,
    pub schema_version: u32,
    pub chain_id: String,
    pub chain_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_chain_event_hash: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "BundleEventAttribution::is_empty")]
    pub attribution: BundleEventAttribution,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    pub payload: serde_json::Value,
}

impl BundleEventAttribution {
    fn is_empty(&self) -> bool {
        self.actor.is_none()
            && self.tool.is_none()
            && self.executor.is_none()
            && self.site.is_none()
    }
}

impl BundleEventObject {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_object_kind(&self.kind, BUNDLE_EVENT_KIND)?;
        if self.schema != SCHEMA_VERSION {
            anyhow::bail!("unexpected schema version: {}", self.schema);
        }
        validate_bundle_identifier("bundle_id", &self.bundle_id)?;
        validate_bundle_identifier("event_kind", &self.event_kind)?;
        validate_bundle_identifier("event_type", &self.event_type)?;
        validate_bundle_identifier("chain_id", &self.chain_id)?;
        if self.schema_version == 0 {
            anyhow::bail!("schema_version must be greater than zero");
        }
        if self.chain_seq == 0 {
            anyhow::bail!("chain_seq must be greater than zero");
        }
        if let Some(hash) = &self.prev_chain_event_hash {
            validate_canonical_hash("prev_chain_event_hash", hash)?;
        }
        parse_canonical_timestamp(&self.created_at)
            .map_err(|error| anyhow::anyhow!("invalid bundle event created_at: {error}"))?;
        if let Some(key) = &self.idempotency_key {
            validate_idempotency_key(key)?;
        }
        if let Some(hash) = &self.request_fingerprint {
            validate_canonical_hash("request_fingerprint", hash)?;
        }
        let serialized_bytes = lillux::canonical_json(&self.to_value())
            .context("failed to canonicalize bundle event")?
            .len();
        if serialized_bytes > MAX_BUNDLE_EVENT_SERIALIZED_BYTES {
            anyhow::bail!(
                "bundle event is {} serialized bytes (max {})",
                serialized_bytes,
                MAX_BUNDLE_EVENT_SERIALIZED_BYTES
            );
        }
        Ok(())
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("BundleEventObject serialization cannot fail")
    }
}

pub fn hash_bundle_event(event: &BundleEventObject) -> Result<String, lillux::CanonicalJsonError> {
    let canonical = lillux::canonical_json(&event.to_value())?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

pub fn validate_bundle_identifier(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    if value == "." || value == ".." {
        anyhow::bail!("{label} must not be a path navigation component");
    }
    if value.len() > 128 {
        anyhow::bail!("{label} is too long");
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        anyhow::bail!("{label} contains unsafe character: {value}");
    }
    Ok(())
}

pub fn validate_idempotency_key(value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("idempotency_key must not be empty");
    }
    if value.len() > 256 {
        anyhow::bail!("idempotency_key is too long");
    }
    Ok(())
}

fn validate_canonical_hash(label: &str, hash: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(hash) || hash.bytes().any(|b| b.is_ascii_uppercase()) {
        anyhow::bail!("invalid {label}: {hash}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_not_embedded_in_bundle_event_body() {
        let event = BundleEventObject {
            schema: SCHEMA_VERSION,
            kind: BUNDLE_EVENT_KIND.to_string(),
            bundle_id: "ryeos-email".to_string(),
            event_kind: "email_event".to_string(),
            event_type: "email_planned".to_string(),
            schema_version: 1,
            chain_id: "email_1".to_string(),
            chain_seq: 1,
            prev_chain_event_hash: None,
            created_at: "2026-06-04T00:00:00Z".to_string(),
            attribution: BundleEventAttribution::default(),
            idempotency_key: None,
            request_fingerprint: None,
            correlation_id: None,
            causation_id: None,
            payload: serde_json::json!({"email_id":"email_1"}),
        };

        event.validate().unwrap();
        let value = event.to_value();
        assert!(value.get("event_hash").is_none());
        assert_eq!(hash_bundle_event(&event).unwrap().len(), 64);
    }

    #[test]
    fn rejects_path_unsafe_identifier() {
        assert!(validate_bundle_identifier("chain_id", "../bad").is_err());
        assert!(validate_bundle_identifier("chain_id", "email/1").is_err());
        assert!(validate_bundle_identifier("bundle_id", ".").is_err());
        assert!(validate_bundle_identifier("event_kind", "..").is_err());
        assert!(validate_bundle_identifier("event_type", "..").is_err());
        assert!(validate_bundle_identifier("chain_id", ".").is_err());
        assert!(validate_bundle_identifier("chain_id", "email_1").is_ok());
    }
}
