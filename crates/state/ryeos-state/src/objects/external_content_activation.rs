//! Node-signed completion receipt for one declarative external-content activation.
//!
//! Existing manifests and consumer bindings remain launch authority. This
//! compact receipt roots the exact complete binding set produced from one
//! signed acquisition program without restating the program or binding facts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXTERNAL_CONTENT_ACTIVATION_KIND: &str = "external_content_activation";
pub const EXTERNAL_CONTENT_ACTIVATION_SCHEMA: &str = "ryeos.external_content_activation.v1";
pub const EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE: &str = "external-content-activations";
pub const MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentActivationComponentReceipt {
    pub id: String,
    pub binding_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentActivationReceipt {
    pub schema: String,
    pub kind: String,
    pub activation_id: String,
    pub activation_ref: String,
    pub activation_program_digest: String,
    pub consumer_ref: String,
    pub publisher_fingerprint: String,
    pub node_fingerprint: String,
    pub policy_digest: String,
    pub components: Vec<ExternalContentActivationComponentReceipt>,
    pub activated_by: String,
    pub recorded_at: String,
}

impl ExternalContentActivationReceipt {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        activation_ref: String,
        activation_program_digest: String,
        consumer_ref: String,
        publisher_fingerprint: String,
        node_fingerprint: String,
        policy_digest: String,
        components: Vec<ExternalContentActivationComponentReceipt>,
        activated_by: String,
    ) -> anyhow::Result<Self> {
        let activation_id = Self::derive_activation_id(
            &activation_program_digest,
            &consumer_ref,
            &publisher_fingerprint,
        )?;
        let receipt = Self {
            schema: EXTERNAL_CONTENT_ACTIVATION_SCHEMA.to_owned(),
            kind: EXTERNAL_CONTENT_ACTIVATION_KIND.to_owned(),
            activation_id,
            activation_ref,
            activation_program_digest,
            consumer_ref,
            publisher_fingerprint,
            node_fingerprint,
            policy_digest,
            components,
            activated_by,
            recorded_at: lillux::time::iso8601_now(),
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn derive_activation_id(
        activation_program_digest: &str,
        consumer_ref: &str,
        publisher_fingerprint: &str,
    ) -> anyhow::Result<String> {
        validate_hash("activation program digest", activation_program_digest)?;
        validate_identity("activation consumer ref", consumer_ref)?;
        validate_hash("activation publisher fingerprint", publisher_fingerprint)?;
        let canonical = lillux::canonical_json(&serde_json::json!({
            "schema": EXTERNAL_CONTENT_ACTIVATION_SCHEMA,
            "activation_program_digest": activation_program_digest,
            "consumer_ref": consumer_ref,
            "publisher_fingerprint": publisher_fingerprint,
        }))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let receipt: Self = serde_json::from_value(value.clone())?;
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != EXTERNAL_CONTENT_ACTIVATION_SCHEMA
            || self.kind != EXTERNAL_CONTENT_ACTIVATION_KIND
        {
            anyhow::bail!("external-content activation receipt schema or kind is not current");
        }
        validate_hash("activation id", &self.activation_id)?;
        validate_identity("activation ref", &self.activation_ref)?;
        validate_hash("activation program digest", &self.activation_program_digest)?;
        validate_identity("activation consumer ref", &self.consumer_ref)?;
        for (label, hash) in [
            (
                "activation publisher fingerprint",
                &self.publisher_fingerprint,
            ),
            ("activation node fingerprint", &self.node_fingerprint),
            ("activation policy digest", &self.policy_digest),
            ("activation operator fingerprint", &self.activated_by),
        ] {
            validate_hash(label, hash)?;
        }
        super::parse_canonical_timestamp(&self.recorded_at)?;
        if self.components.is_empty()
            || self.components.len() > MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS
        {
            anyhow::bail!("external-content activation receipt has an invalid component set");
        }
        let mut component_ids = std::collections::BTreeSet::new();
        for component in &self.components {
            validate_id("activation component id", &component.id)?;
            validate_hash("activation component binding", &component.binding_hash)?;
            if !component_ids.insert(component.id.as_str()) {
                anyhow::bail!("external-content activation receipt repeats a component");
            }
        }
        if !self
            .components
            .windows(2)
            .all(|pair| pair[0].id < pair[1].id)
        {
            anyhow::bail!("external-content activation receipt components are not canonical");
        }
        let expected = Self::derive_activation_id(
            &self.activation_program_digest,
            &self.consumer_ref,
            &self.publisher_fingerprint,
        )?;
        if self.activation_id != expected {
            anyhow::bail!("external-content activation id contradicts its authority tuple");
        }
        Ok(())
    }
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    super::thread_snapshot::validate_canonical_hash(label, value)
}

fn validate_identity(label: &str, value: &str) -> anyhow::Result<()> {
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

fn validate_id(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        anyhow::bail!("{label} is not canonical");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ExternalContentActivationReceipt {
        ExternalContentActivationReceipt::new(
            "config:fixture/activation".to_owned(),
            "a".repeat(64),
            "worker:fixture/hosted".to_owned(),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            vec![ExternalContentActivationComponentReceipt {
                id: "runtime".to_owned(),
                binding_hash: "3".repeat(64),
            }],
            "4".repeat(64),
        )
        .unwrap()
    }

    #[test]
    fn activation_receipt_round_trips_and_roots_one_complete_set() {
        let receipt = fixture();
        assert_eq!(
            ExternalContentActivationReceipt::from_value(&receipt.to_value().unwrap()).unwrap(),
            receipt
        );
    }

    #[test]
    fn activation_receipt_requires_a_canonical_unique_component_set() {
        let mut receipt = fixture();
        receipt
            .components
            .push(ExternalContentActivationComponentReceipt {
                id: "another".to_owned(),
                binding_hash: "5".repeat(64),
            });
        assert!(receipt.validate().is_err());

        receipt
            .components
            .sort_by(|left, right| left.id.cmp(&right.id));
        receipt.components[1].binding_hash = receipt.components[0].binding_hash.clone();
        assert!(receipt.validate().is_ok());

        receipt.components[1].id = receipt.components[0].id.clone();
        receipt.components[1].binding_hash = receipt.components[0].binding_hash.clone();
        assert!(receipt.validate().is_err());
    }
}
