//! Operator-owned binding between retained external bytes and one consumer.
//!
//! Importing bytes proves only that the node captured them. This object is the
//! separate durable grant that permits one canonical implementation ref from
//! one trusted publisher to consume the retained manifest. A signed generic
//! head selects the current binding state. Released bindings deliberately do
//! not retain the manifest through CAS closure.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXTERNAL_CONTENT_BINDING_KIND: &str = "external_content_binding";
pub const EXTERNAL_CONTENT_BINDING_SCHEMA: &str = "ryeos.external_content_binding.v1";
pub const EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE: &str = "external-content-bindings";
pub const EXTERNAL_CONTENT_BINDING_SCHEMA_EPOCH: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalContentBindingState {
    Active,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentBinding {
    pub schema: String,
    pub kind: String,
    pub binding_id: String,
    pub manifest_hash: String,
    pub manifest_kind: String,
    pub consumer_ref: String,
    pub publisher_fingerprint: String,
    pub state: ExternalContentBindingState,
    pub bound_by: String,
    pub recorded_at: String,
}

impl ExternalContentBinding {
    pub fn active(
        manifest_hash: String,
        manifest_kind: String,
        consumer_ref: String,
        publisher_fingerprint: String,
        bound_by: String,
    ) -> anyhow::Result<Self> {
        let binding_id =
            Self::derive_binding_id(&manifest_hash, &consumer_ref, &publisher_fingerprint)?;
        let value = Self {
            schema: EXTERNAL_CONTENT_BINDING_SCHEMA.to_owned(),
            kind: EXTERNAL_CONTENT_BINDING_KIND.to_owned(),
            binding_id,
            manifest_hash,
            manifest_kind,
            consumer_ref,
            publisher_fingerprint,
            state: ExternalContentBindingState::Active,
            bound_by,
            recorded_at: lillux::time::iso8601_now(),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn released_from(active: &Self, released_by: String) -> anyhow::Result<Self> {
        active.validate()?;
        if active.state != ExternalContentBindingState::Active {
            anyhow::bail!("only an active external-content binding can be released");
        }
        let value = Self {
            state: ExternalContentBindingState::Released,
            bound_by: released_by,
            recorded_at: lillux::time::iso8601_now(),
            ..active.clone()
        };
        value.validate()?;
        Ok(value)
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let binding: Self = serde_json::from_value(value.clone())?;
        binding.validate()?;
        Ok(binding)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn derive_binding_id(
        manifest_hash: &str,
        consumer_ref: &str,
        publisher_fingerprint: &str,
    ) -> anyhow::Result<String> {
        validate_hash("external-content binding manifest", manifest_hash)?;
        validate_bounded_identity("external-content binding consumer ref", consumer_ref)?;
        validate_hash(
            "external-content binding publisher fingerprint",
            publisher_fingerprint,
        )?;
        let canonical = lillux::canonical_json(&serde_json::json!({
            "manifest_hash": manifest_hash,
            "consumer_ref": consumer_ref,
            "publisher_fingerprint": publisher_fingerprint,
        }))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != EXTERNAL_CONTENT_BINDING_SCHEMA {
            anyhow::bail!("external-content binding schema is not current");
        }
        if self.kind != EXTERNAL_CONTENT_BINDING_KIND {
            anyhow::bail!("external-content binding kind is invalid");
        }
        validate_hash("external-content binding id", &self.binding_id)?;
        validate_hash("external-content binding manifest", &self.manifest_hash)?;
        if !matches!(
            self.manifest_kind.as_str(),
            super::EXTERNAL_CONTENT_MANIFEST_KIND | super::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
        ) {
            anyhow::bail!("external-content binding names an unsupported manifest kind");
        }
        validate_bounded_identity("external-content binding consumer ref", &self.consumer_ref)?;
        validate_hash(
            "external-content binding publisher fingerprint",
            &self.publisher_fingerprint,
        )?;
        validate_hash(
            "external-content binding operator fingerprint",
            &self.bound_by,
        )?;
        super::parse_canonical_timestamp(&self.recorded_at)?;
        let expected = Self::derive_binding_id(
            &self.manifest_hash,
            &self.consumer_ref,
            &self.publisher_fingerprint,
        )?;
        if self.binding_id != expected {
            anyhow::bail!("external-content binding id contradicts its authority tuple");
        }
        Ok(())
    }
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    super::thread_snapshot::validate_canonical_hash(label, value)
}

fn validate_bounded_identity(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 512
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        anyhow::bail!("{label} is empty, unbounded, or non-canonical");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_binding_round_trips_and_release_does_not_change_identity() {
        let active = ExternalContentBinding::active(
            "a".repeat(64),
            super::super::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
            "worker:standard/local".to_owned(),
            "b".repeat(64),
            "c".repeat(64),
        )
        .unwrap();
        assert_eq!(
            ExternalContentBinding::from_value(&active.to_value().unwrap()).unwrap(),
            active
        );
        let released = ExternalContentBinding::released_from(&active, "d".repeat(64)).unwrap();
        assert_eq!(released.binding_id, active.binding_id);
        assert_eq!(released.state, ExternalContentBindingState::Released);
    }
}
