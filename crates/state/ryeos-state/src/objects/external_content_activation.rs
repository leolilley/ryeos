//! Node-signed completion receipt for one declarative external-content activation.
//!
//! The receipt does not replace external-content manifests or consumer
//! bindings. It roots the exact complete set produced from one retained signed
//! activation program so restart recovery and operators can distinguish a
//! settled activation from a partially published set of bindings.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXTERNAL_CONTENT_ACTIVATION_KIND: &str = "external_content_activation";
pub const EXTERNAL_CONTENT_ACTIVATION_SCHEMA: &str = "ryeos.external_content_activation.v1";
pub const EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE: &str = "external-content-activations";
pub const MAX_EXTERNAL_CONTENT_ACTIVATION_SOURCES: usize = 8;
pub const MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentActivationSourceReceipt {
    pub id: String,
    pub archive_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentActivationComponentReceipt {
    pub id: String,
    pub source_id: String,
    pub member_sha256: String,
    pub manifest_hash: String,
    pub manifest_kind: String,
    pub binding_id: String,
    pub binding_hash: String,
    pub shape: String,
    pub storage: String,
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
    pub sources: Vec<ExternalContentActivationSourceReceipt>,
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
        sources: Vec<ExternalContentActivationSourceReceipt>,
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
            sources,
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
        if self.sources.is_empty()
            || self.sources.len() > MAX_EXTERNAL_CONTENT_ACTIVATION_SOURCES
            || self.components.is_empty()
            || self.components.len() > MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS
        {
            anyhow::bail!("external-content activation receipt has an invalid set size");
        }
        let mut source_ids = std::collections::BTreeSet::new();
        for source in &self.sources {
            validate_id("activation source id", &source.id)?;
            validate_hash("activation source archive", &source.archive_sha256)?;
            if !source_ids.insert(source.id.as_str()) {
                anyhow::bail!("external-content activation receipt repeats a source id");
            }
        }
        let mut component_ids = std::collections::BTreeSet::new();
        let mut binding_ids = std::collections::BTreeSet::new();
        for component in &self.components {
            validate_id("activation component id", &component.id)?;
            validate_id("activation component source id", &component.source_id)?;
            if !source_ids.contains(component.source_id.as_str()) {
                anyhow::bail!("activation component names an absent source");
            }
            for (label, hash) in [
                ("activation member digest", &component.member_sha256),
                ("activation manifest hash", &component.manifest_hash),
                ("activation binding id", &component.binding_id),
                ("activation binding hash", &component.binding_hash),
            ] {
                validate_hash(label, hash)?;
            }
            if !matches!(
                component.manifest_kind.as_str(),
                super::EXTERNAL_CONTENT_MANIFEST_KIND | super::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
            ) || !matches!(component.shape.as_str(), "file" | "tree")
                || !matches!(component.storage.as_str(), "content" | "large_content")
            {
                anyhow::bail!("activation component has an unsupported shape or storage kind");
            }
            if !component_ids.insert(component.id.as_str())
                || !binding_ids.insert(component.binding_id.as_str())
            {
                anyhow::bail!("external-content activation receipt repeats a component");
            }
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
            vec![ExternalContentActivationSourceReceipt {
                id: "package".to_owned(),
                archive_sha256: "e".repeat(64),
            }],
            vec![ExternalContentActivationComponentReceipt {
                id: "runtime".to_owned(),
                source_id: "package".to_owned(),
                member_sha256: "f".repeat(64),
                manifest_hash: "1".repeat(64),
                manifest_kind: super::super::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
                binding_id: "2".repeat(64),
                binding_hash: "3".repeat(64),
                shape: "file".to_owned(),
                storage: "large_content".to_owned(),
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
    fn activation_receipt_rejects_an_unretained_source_or_duplicate_binding() {
        let mut receipt = fixture();
        receipt.components[0].source_id = "missing".to_owned();
        assert!(receipt.validate().is_err());

        let mut receipt = fixture();
        let duplicate = receipt.components[0].clone();
        receipt.components.push(duplicate);
        assert!(receipt.validate().is_err());
    }
}
