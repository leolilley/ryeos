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
pub const BOOTSTRAP_TERMINATION_PROTOCOL: &str = "ryeos.runtime-snapshot-bootstrap-termination.v2";
pub const BOOTSTRAP_READINESS_PROTOCOL: &str = "ryeos.runtime-snapshot-bootstrap-readiness.v1";
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
    /// independently check the fields the response actually exposes (ID,
    /// plan, region, lifetime, network policy, creation time), with account
    /// scoped by its authenticated request. Render does not return group
    /// membership here; snapshot creation must prove the intended group.
    /// This object alone proves neither readiness nor snapshot suitability.
    pub provider_creation_observation: serde_json::Value,
    /// False retains a real provider ID solely for exact cleanup when its
    /// observed create attributes differ from the signed request.
    pub creation_attributes_verified: bool,
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

/// A read-only observation of an already-bound source occurrence. This
/// carries no new create opportunity and is not a snapshot/upload grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotBootstrapReadinessRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotBootstrapIntent,
    pub occurrence: RuntimeSnapshotBootstrapOccurrence,
    pub provider_spec_digest: String,
}

impl RuntimeSnapshotBootstrapReadinessRequest {
    pub fn validate(&self) -> Result<()> {
        self.occurrence.validate_for(&self.intent)?;
        ensure!(
            self.protocol == BOOTSTRAP_READINESS_PROTOCOL
                && self.provider_spec_digest == self.intent.provider_spec_digest
                && self.occurrence.creation_attributes_verified
                && !self.occurrence.contact_deadline_exceeded
                && canonical_json(self)?.len() <= MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
            "bootstrap readiness differs from a timely verified source"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotBootstrapReadinessObservation {
    pub schema: u32,
    pub operation_id: String,
    pub occurrence_id: String,
    pub provider_response_sha256: String,
    pub observed_created_at: String,
    pub observed_at_ms: i64,
}

impl RuntimeSnapshotBootstrapReadinessObservation {
    pub fn validate_for(&self, request: &RuntimeSnapshotBootstrapReadinessRequest) -> Result<()> {
        request.validate()?;
        ensure!(
            self.schema == 1
                && self.operation_id == request.intent.operation_id
                && self.occurrence_id == request.occurrence.occurrence_id
                && valid_hash(&self.provider_response_sha256)
                && !self.observed_created_at.is_empty()
                && self.observed_created_at.len() <= 64
                && self.observed_created_at.is_ascii()
                && self.observed_at_ms > 0
                && canonical_json(self)?.len() <= 1024,
            "bootstrap readiness changed the exact source"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeSnapshotBootstrapReadinessAdapterResponse {
    Running {
        observation: RuntimeSnapshotBootstrapReadinessObservation,
    },
    NotReady {
        operation_id: String,
        occurrence_id: String,
    },
}

impl RuntimeSnapshotBootstrapReadinessAdapterResponse {
    pub fn validate_for(&self, request: &RuntimeSnapshotBootstrapReadinessRequest) -> Result<()> {
        request.validate()?;
        match self {
            Self::Running { observation } => observation.validate_for(request),
            Self::NotReady {
                operation_id,
                occurrence_id,
            } => {
                ensure!(
                    operation_id == &request.intent.operation_id
                        && occurrence_id == &request.occurrence.occurrence_id,
                    "not-ready bootstrap response changed source"
                );
                Ok(())
            }
        }
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

/// The source Sandbox has one cleanup operation, whether its create result
/// was timely or cleanup-only. Deadline changes cannot remint it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapCleanupMode {
    TerminateOnce,
    ObserveOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotBootstrapTerminationIntent {
    pub schema: u32,
    pub mode: BootstrapCleanupMode,
    pub operation_id: String,
    pub bootstrap_operation_id: String,
    pub occurrence_id: String,
    pub owner_principal: String,
    pub provider_id: String,
    pub provider_group_id: String,
    pub provider_spec_digest: String,
    pub attempt_deadline_ms: i64,
}

impl RuntimeSnapshotBootstrapTerminationIntent {
    pub fn derived_operation_id(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(canonical_json(&(
            BOOTSTRAP_TERMINATION_PROTOCOL,
            &self.bootstrap_operation_id,
            &self.occurrence_id,
        ))?)))
    }

    pub fn validate_for(
        &self,
        bootstrap: &RuntimeSnapshotBootstrapIntent,
        occurrence: &RuntimeSnapshotBootstrapOccurrence,
    ) -> Result<()> {
        occurrence.validate_for(bootstrap)?;
        ensure!(
            self.schema == 2
                && self.operation_id == self.derived_operation_id()?
                && self.bootstrap_operation_id == bootstrap.operation_id
                && self.occurrence_id == occurrence.occurrence_id
                && self.owner_principal == bootstrap.owner_principal
                && self.provider_id == bootstrap.provider_id
                && self.provider_group_id == bootstrap.provider_group_id
                && self.provider_spec_digest == bootstrap.provider_spec_digest
                && self.attempt_deadline_ms > 0
                && canonical_json(self)?.len() <= 2048,
            "bootstrap termination differs from its retained source occurrence"
        );
        Ok(())
    }
}

/// Provider terminal status is not proof that guest writers have settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotBootstrapTerminalObservation {
    pub schema: u32,
    pub operation_id: String,
    pub occurrence_id: String,
    pub provider_response_sha256: String,
    pub terminated_at: String,
    pub contact_deadline_exceeded: bool,
}

impl RuntimeSnapshotBootstrapTerminalObservation {
    pub fn validate_for(&self, intent: &RuntimeSnapshotBootstrapTerminationIntent) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.operation_id == intent.operation_id
                && self.occurrence_id == intent.occurrence_id
                && valid_hash(&self.provider_response_sha256)
                && !self.terminated_at.is_empty()
                && self.terminated_at.len() <= 64
                && canonical_json(self)?.len() <= 1024,
            "bootstrap terminal observation changed its occurrence"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotBootstrapTerminationAdapterRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotBootstrapTerminationIntent,
    pub bootstrap_intent: RuntimeSnapshotBootstrapIntent,
    pub occurrence: RuntimeSnapshotBootstrapOccurrence,
    pub provider_spec_digest: String,
}

impl RuntimeSnapshotBootstrapTerminationAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.intent
            .validate_for(&self.bootstrap_intent, &self.occurrence)?;
        ensure!(
            self.protocol == BOOTSTRAP_TERMINATION_PROTOCOL
                && self.provider_spec_digest == self.intent.provider_spec_digest
                && canonical_json(self)?.len() <= MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
            "bootstrap termination request differs from retained authority"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeSnapshotBootstrapTerminationAdapterResponse {
    Terminal {
        observation: RuntimeSnapshotBootstrapTerminalObservation,
    },
    Uncertain {
        operation_id: String,
    },
}

impl RuntimeSnapshotBootstrapTerminationAdapterResponse {
    pub fn validate_for(
        &self,
        request: &RuntimeSnapshotBootstrapTerminationAdapterRequest,
    ) -> Result<()> {
        request.validate()?;
        match self {
            Self::Terminal { observation } => observation.validate_for(&request.intent)?,
            Self::Uncertain { operation_id } => ensure!(
                operation_id == &request.intent.operation_id,
                "uncertain termination changed bootstrap source"
            ),
        }
        Ok(())
    }
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
            creation_attributes_verified: true,
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

    #[test]
    fn source_termination_has_one_identity_across_deadline_changes() {
        let source = intent();
        let occurrence = RuntimeSnapshotBootstrapOccurrence {
            schema: 1,
            operation_id: source.operation_id.clone(),
            occurrence_id: "sbx-exact".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"status":"creating"}),
            creation_attributes_verified: true,
            contact_deadline_exceeded: true,
        };
        let mut termination = RuntimeSnapshotBootstrapTerminationIntent {
            schema: 2,
            mode: BootstrapCleanupMode::TerminateOnce,
            operation_id: String::new(),
            bootstrap_operation_id: source.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            owner_principal: source.owner_principal.clone(),
            provider_id: source.provider_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            provider_spec_digest: source.provider_spec_digest.clone(),
            attempt_deadline_ms: source.attempt_deadline_ms,
        };
        termination.operation_id = termination.derived_operation_id().unwrap();
        termination.validate_for(&source, &occurrence).unwrap();
        let original = termination.operation_id.clone();
        termination.attempt_deadline_ms += 10_000;
        assert_eq!(termination.derived_operation_id().unwrap(), original);
        let mut wrong = termination;
        wrong.occurrence_id = "sbx-other".into();
        assert!(wrong.validate_for(&source, &occurrence).is_err());
    }
}
