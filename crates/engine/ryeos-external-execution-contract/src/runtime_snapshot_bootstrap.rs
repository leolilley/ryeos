//! A bounded source occurrence for producing an external runtime snapshot.
//!
//! Bootstrap has no Worker, candidate, provider-session, or qualified-runtime
//! authority. Its create attempt is durable before provider contact; an
//! uncertain response may only be reconciled, never blindly created again.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::canonical_json;
use crate::runtime_snapshot::RuntimeSnapshotSource;

pub const BOOTSTRAP_INTENT_SCHEMA: u32 = 1;
pub const BOOTSTRAP_ADAPTER_PROTOCOL: &str = "ryeos.runtime-snapshot-bootstrap-adapter.v1";
pub const MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotBootstrapIntent {
    pub schema: u32,
    pub operation_id: String,
    pub owner_principal: String,
    pub provider_id: String,
    pub provider_group_id: String,
    pub production_binding_digest: String,
    pub bootstrap_profile_digest: String,
    pub adapter_artifact_hash: String,
    pub provider_spec_digest: String,
    pub settings_digest: String,
    pub source: RuntimeSnapshotSource,
    pub guest_runtime_manifest_hash: String,
    pub maximum_lifetime_seconds: u32,
    pub attempt_deadline_ms: i64,
}

impl RuntimeSnapshotBootstrapIntent {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == BOOTSTRAP_INTENT_SCHEMA,
            "unsupported bootstrap intent"
        );
        for (label, hash) in [
            ("operation", &self.operation_id),
            ("production binding", &self.production_binding_digest),
            ("bootstrap profile", &self.bootstrap_profile_digest),
            ("adapter", &self.adapter_artifact_hash),
            ("provider spec", &self.provider_spec_digest),
            ("settings", &self.settings_digest),
            ("runtime manifest", &self.guest_runtime_manifest_hash),
        ] {
            ensure!(valid_hash(hash), "bootstrap {label} digest is invalid");
        }
        let principal = self.owner_principal.strip_prefix("fp:").unwrap_or_default();
        ensure!(
            valid_hash(principal),
            "bootstrap owner principal is invalid"
        );
        for (label, value, maximum) in [
            ("provider", &self.provider_id, 128),
            ("group", &self.provider_group_id, 256),
        ] {
            ensure!(
                !value.is_empty()
                    && value.len() <= maximum
                    && value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                    }),
                "bootstrap {label} is invalid"
            );
        }
        self.source.validate()?;
        ensure!(
            matches!(
                self.source,
                RuntimeSnapshotSource::BundleMaterialization { .. }
            ),
            "bootstrap cannot represent captured-product or arbitrary worker source"
        );
        ensure!(
            (1..=3600).contains(&self.maximum_lifetime_seconds) && self.attempt_deadline_ms > 0,
            "bootstrap lifetime or deadline exceeds its bound"
        );
        ensure!(
            self.operation_id == self.derived_operation_id()?,
            "bootstrap operation differs from exact source and provider authority"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
            "bootstrap intent exceeds its bound"
        );
        Ok(())
    }

    /// Deadline is not part of identity: a lost create acknowledgment cannot
    /// be reminted into a second opportunity by choosing a later timeout.
    pub fn derived_operation_id(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(canonical_json(&(
            "ryeos.runtime-snapshot-bootstrap-operation.v1",
            &self.owner_principal,
            &self.provider_id,
            &self.provider_group_id,
            &self.production_binding_digest,
            &self.bootstrap_profile_digest,
            &self.adapter_artifact_hash,
            &self.provider_spec_digest,
            &self.settings_digest,
            &self.source,
            &self.guest_runtime_manifest_hash,
            self.maximum_lifetime_seconds,
        ))?)))
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotBootstrapAdapterRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotBootstrapIntent,
    pub provider_spec_digest: String,
}

impl RuntimeSnapshotBootstrapAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.intent.validate()?;
        ensure!(
            self.protocol == BOOTSTRAP_ADAPTER_PROTOCOL
                && self.provider_spec_digest == self.intent.provider_spec_digest
                && canonical_json(self)?.len() <= MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
            "bootstrap adapter request differs from exact retained intent"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotBootstrapOccurrence {
    pub schema: u32,
    pub operation_id: String,
    pub occurrence_id: String,
    pub provider_response_sha256: String,
    /// Bounded provider create-response evidence. The Render adapter must
    /// independently check account, group, plan, region, lifetime, and
    /// deny-all network policy before the occurrence can authorize use.
    /// This object alone neither proves those checks nor proves readiness.
    pub provider_creation_observation: serde_json::Value,
    pub contact_deadline_exceeded: bool,
}

impl RuntimeSnapshotBootstrapOccurrence {
    pub fn validate_for(&self, intent: &RuntimeSnapshotBootstrapIntent) -> Result<()> {
        intent.validate()?;
        ensure!(
            self.schema == 1
                && self.operation_id == intent.operation_id
                && !self.occurrence_id.is_empty()
                && self.occurrence_id.len() <= 256
                && self
                    .occurrence_id
                    .bytes()
                    .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') })
                && valid_hash(&self.provider_response_sha256)
                && self.provider_creation_observation.is_object()
                && canonical_json(&self.provider_creation_observation)?.len() <= 4096,
            "bootstrap occurrence is not bound to its exact create attempt"
        );
        ensure!(
            canonical_json(self)?.len() <= 8192,
            "bootstrap occurrence exceeds its bound"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeSnapshotBootstrapAdapterResponse {
    OccurrenceBound {
        occurrence: RuntimeSnapshotBootstrapOccurrence,
    },
    Uncertain {
        operation_id: String,
        intent_digest: String,
    },
}

impl RuntimeSnapshotBootstrapAdapterResponse {
    pub fn validate_for(&self, request: &RuntimeSnapshotBootstrapAdapterRequest) -> Result<()> {
        request.validate()?;
        match self {
            Self::OccurrenceBound { occurrence } => occurrence.validate_for(&request.intent)?,
            Self::Uncertain {
                operation_id,
                intent_digest,
            } => ensure!(
                operation_id == &request.intent.operation_id
                    && intent_digest == &request.intent.digest()?,
                "uncertain bootstrap result changed its durable attempt"
            ),
        }
        Ok(())
    }
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> RuntimeSnapshotBootstrapIntent {
        let mut intent = RuntimeSnapshotBootstrapIntent {
            schema: BOOTSTRAP_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "1".repeat(64)),
            provider_id: "render-sandbox-early-access".into(),
            provider_group_id: "sbg-exact".into(),
            production_binding_digest: "2".repeat(64),
            bootstrap_profile_digest: "a".repeat(64),
            adapter_artifact_hash: "3".repeat(64),
            provider_spec_digest: "4".repeat(64),
            settings_digest: "5".repeat(64),
            source: RuntimeSnapshotSource::BundleMaterialization {
                materialization_attestation_hash: "6".repeat(64),
                source_coordinate_digest: "7".repeat(64),
                materialization_binding_digest: "8".repeat(64),
            },
            guest_runtime_manifest_hash: "9".repeat(64),
            maximum_lifetime_seconds: 900,
            attempt_deadline_ms: 1_800_000_000_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
    }

    #[test]
    fn exact_materialization_and_provider_binding_define_one_create_opportunity() {
        let original = intent();
        original.validate().unwrap();
        let mut changed = original.clone();
        changed.attempt_deadline_ms += 1000;
        assert_eq!(
            changed.derived_operation_id().unwrap(),
            original.operation_id
        );
        changed = original.clone();
        changed.source = RuntimeSnapshotSource::CapturedProduct {
            product_witness_hash: "6".repeat(64),
        };
        changed.operation_id = changed.derived_operation_id().unwrap();
        assert!(changed.validate().is_err());
        changed = original.clone();
        changed.production_binding_digest = "a".repeat(64);
        assert!(changed.validate().is_err());
        changed.operation_id = changed.derived_operation_id().unwrap();
        assert_ne!(changed.operation_id, original.operation_id);
    }

    #[test]
    fn bound_or_uncertain_result_cannot_switch_attempt() {
        let intent = intent();
        let request = RuntimeSnapshotBootstrapAdapterRequest {
            protocol: BOOTSTRAP_ADAPTER_PROTOCOL.into(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
            intent: intent.clone(),
        };
        let occurrence = RuntimeSnapshotBootstrapOccurrence {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: "sbx-exact".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"status": "creating"}),
            contact_deadline_exceeded: false,
        };
        RuntimeSnapshotBootstrapAdapterResponse::OccurrenceBound {
            occurrence: occurrence.clone(),
        }
        .validate_for(&request)
        .unwrap();
        let mut wrong = occurrence;
        wrong.operation_id = "b".repeat(64);
        assert!(wrong.validate_for(&intent).is_err());
        let uncertain = RuntimeSnapshotBootstrapAdapterResponse::Uncertain {
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
        };
        uncertain.validate_for(&request).unwrap();
        let mut wrong_request = request;
        wrong_request.provider_spec_digest = "c".repeat(64);
        assert!(uncertain.validate_for(&wrong_request).is_err());
    }
}
