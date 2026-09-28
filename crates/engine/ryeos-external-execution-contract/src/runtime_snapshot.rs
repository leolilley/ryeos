//! Durable, provider-neutral identity of one external runtime snapshot effect.
//!
//! The intent owns one provider-sequence attempt. A provider locator is
//! only an attempt result; restored bytes and runtime behavior require an
//! independent qualification rooted in the retained product witness.

use anyhow::{Result, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::canonical_json;

pub const RUNTIME_SNAPSHOT_INTENT_SCHEMA: u32 = 1;
pub const RUNTIME_SNAPSHOT_RESULT_SCHEMA: u32 = 2;
pub const RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL: &str = "ryeos.runtime-snapshot-adapter.v2";
pub const RUNTIME_SNAPSHOT_READINESS_PROTOCOL: &str = "ryeos.runtime-snapshot-readiness.v1";
pub const RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA: u32 = 1;
pub const MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES: usize = 24 * 1024;
pub const MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES: u64 = 64 * 1024 * 1024 + 16 * 1024;

/// One sealed invocation of the exact admitted snapshot producer. The upload
/// descriptor is a process-local transport coordinate, not durable identity;
/// its bytes and digest are owned by the retained intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotAdapterRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotIntent,
    pub provider_spec_digest: String,
    pub upload_descriptor: u32,
    pub upload_bytes: u64,
    pub upload_sha256: String,
}

impl RuntimeSnapshotAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.intent.validate()?;
        ensure!(
            self.protocol == RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL
                && self.upload_descriptor > 2
                && self.provider_spec_digest == self.intent.provider_spec_digest
                && self.upload_bytes == self.intent.upload_bytes
                && self.upload_sha256 == self.intent.upload_sha256,
            "runtime snapshot adapter handoff differs from the retained intent"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "runtime snapshot adapter request exceeds its bound"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeSnapshotAdapterResponse {
    Bound {
        locator: RuntimeSnapshotLocator,
    },
    Uncertain {
        operation_id: String,
        intent_digest: String,
    },
}

impl RuntimeSnapshotAdapterResponse {
    pub fn validate_for(&self, request: &RuntimeSnapshotAdapterRequest) -> Result<()> {
        request.validate()?;
        match self {
            Self::Bound { locator } => locator.validate_for(&request.intent)?,
            Self::Uncertain {
                operation_id,
                intent_digest,
            } => {
                ensure!(
                    operation_id == &request.intent.operation_id
                        && intent_digest == &request.intent.digest()?,
                    "uncertain snapshot result changed its durable attempt"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotIntent {
    pub schema: u32,
    pub operation_id: String,
    pub owner_principal: String,
    pub provider_id: String,
    pub source_occurrence_id: String,
    pub provider_group_id: String,
    /// Signed producer authority, distinct from the later placement binding
    /// that will name the resulting snapshot ID.
    pub production_profile_digest: String,
    pub adapter_artifact_hash: String,
    pub provider_spec_digest: String,
    pub settings_digest: String,
    pub product_witness_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub owner_executable_sha256: String,
    pub controller_public_root: String,
    pub upload_sha256: String,
    pub upload_bytes: u64,
    pub attempt_deadline_ms: i64,
}

impl RuntimeSnapshotIntent {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            "unsupported runtime snapshot intent"
        );
        for (label, value) in [
            ("operation", &self.operation_id),
            ("production profile", &self.production_profile_digest),
            ("adapter", &self.adapter_artifact_hash),
            ("provider spec", &self.provider_spec_digest),
            ("settings", &self.settings_digest),
            ("product witness", &self.product_witness_hash),
            ("guest runtime manifest", &self.guest_runtime_manifest_hash),
            ("owner executable", &self.owner_executable_sha256),
            ("upload", &self.upload_sha256),
        ] {
            require_hash(value, label)?;
        }
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .ok_or_else(|| anyhow::anyhow!("runtime snapshot owner principal is invalid"))?;
        require_hash(owner, "owner")?;
        require_bounded_text(&self.provider_id, 128, "provider")?;
        require_bounded_text(&self.source_occurrence_id, 256, "source occurrence")?;
        require_bounded_text(&self.provider_group_id, 256, "provider group")?;
        let encoded = self
            .controller_public_root
            .strip_prefix("ed25519:")
            .ok_or_else(|| {
                anyhow::anyhow!("runtime snapshot controller root has no Ed25519 envelope")
            })?;
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        ensure!(
            decoded.len() == 32
                && base64::engine::general_purpose::STANDARD.encode(&decoded) == encoded,
            "runtime snapshot controller root is not canonical 32-byte base64"
        );
        ensure!(
            (1..=MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES).contains(&self.upload_bytes)
                && self.attempt_deadline_ms > 0,
            "runtime snapshot upload or deadline exceeds its bound"
        );
        ensure!(
            self.operation_id == self.derived_operation_id()?,
            "runtime snapshot operation does not match exact transfer coordinates"
        );
        Ok(())
    }

    /// Stable across a retry with a later deadline, but unique to the exact
    /// admitted source and delivery bytes. A caller cannot mint a second
    /// attempt opportunity by choosing another operation ID.
    pub fn derived_operation_id(&self) -> Result<String> {
        let coordinates = (
            "ryeos.runtime-snapshot-operation.v1",
            &self.owner_principal,
            &self.provider_id,
            &self.source_occurrence_id,
            &self.provider_group_id,
            &self.production_profile_digest,
            &self.adapter_artifact_hash,
            &self.provider_spec_digest,
            &self.settings_digest,
            &self.product_witness_hash,
            &self.guest_runtime_manifest_hash,
            &self.owner_executable_sha256,
            &self.controller_public_root,
            &self.upload_sha256,
            self.upload_bytes,
        );
        Ok(hex::encode(Sha256::digest(canonical_json(&coordinates)?)))
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotLocator {
    pub schema: u32,
    pub operation_id: String,
    pub intent_digest: String,
    pub source_occurrence_id: String,
    pub provider_group_id: String,
    pub snapshot_id: String,
    pub provider_response_sha256: String,
    /// Bounded, adapter-validated creation projection retained for later
    /// readiness interpretation. It is not a restored-content claim.
    pub provider_creation_observation: serde_json::Value,
    pub adapter_observation_sha256: String,
}

/// One separately owned restored-Sandbox qualification attempt. This is not
/// a Worker allocation: it cannot inherit a Worker runtime qualification that
/// this very attempt is intended to establish. The exact selected snapshot
/// and provider profile are retained before non-idempotent provider contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotQualificationIntent {
    pub schema: u32,
    pub operation_id: String,
    pub owner_principal: String,
    pub snapshot_operation_id: String,
    pub snapshot_intent_digest: String,
    pub snapshot_id: String,
    pub provider_id: String,
    pub provider_group_id: String,
    pub qualification_profile_digest: String,
    pub adapter_artifact_hash: String,
    pub provider_spec_digest: String,
    pub settings_digest: String,
    pub verifier_artifact_hash: String,
    pub maximum_lifetime_seconds: u32,
    pub attempt_deadline_ms: i64,
}

impl RuntimeSnapshotQualificationIntent {
    pub fn validate_for(
        &self,
        source: &RuntimeSnapshotIntent,
        locator: &RuntimeSnapshotLocator,
    ) -> Result<()> {
        locator.validate_for(source)?;
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA
                && self.owner_principal == source.owner_principal
                && self.snapshot_operation_id == source.operation_id
                && self.snapshot_intent_digest == source.digest()?
                && self.snapshot_id == locator.snapshot_id
                && self.provider_id == source.provider_id
                && self.provider_group_id == source.provider_group_id
                && self.attempt_deadline_ms > 0
                && (1..=3600).contains(&self.maximum_lifetime_seconds),
            "snapshot qualification intent differs from retained snapshot authority"
        );
        for (label, hash) in [
            ("qualification profile", &self.qualification_profile_digest),
            ("qualification adapter", &self.adapter_artifact_hash),
            ("provider spec", &self.provider_spec_digest),
            ("settings", &self.settings_digest),
            ("verifier artifact", &self.verifier_artifact_hash),
        ] {
            require_hash(hash, label)?;
        }
        ensure!(
            self.operation_id == self.derived_operation_id()?,
            "snapshot qualification operation differs from exact selected product"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "snapshot qualification intent exceeds its bound"
        );
        Ok(())
    }

    /// Deadline changes cannot mint a fresh non-idempotent create opportunity.
    pub fn derived_operation_id(&self) -> Result<String> {
        let coordinates = (
            "ryeos.runtime-snapshot-qualification.v1",
            &self.owner_principal,
            &self.snapshot_operation_id,
            &self.snapshot_intent_digest,
            &self.snapshot_id,
            &self.provider_id,
            &self.provider_group_id,
            &self.qualification_profile_digest,
            &self.adapter_artifact_hash,
            &self.provider_spec_digest,
            &self.settings_digest,
            &self.verifier_artifact_hash,
            self.maximum_lifetime_seconds,
        );
        Ok(hex::encode(Sha256::digest(canonical_json(&coordinates)?)))
    }
}

/// A read-only observation of a previously bound locator. This grants no
/// create/retry authority and no claim about restored snapshot contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotReadinessRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotIntent,
    pub locator: RuntimeSnapshotLocator,
    pub provider_spec_digest: String,
}

impl RuntimeSnapshotReadinessRequest {
    pub fn validate(&self) -> Result<()> {
        self.locator.validate_for(&self.intent)?;
        ensure!(
            self.protocol == RUNTIME_SNAPSHOT_READINESS_PROTOCOL
                && self.provider_spec_digest == self.intent.provider_spec_digest
                && canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "runtime snapshot readiness request differs from its retained locator"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotReadinessObservation {
    pub schema: u32,
    pub operation_id: String,
    pub intent_digest: String,
    pub snapshot_id: String,
    pub source_occurrence_id: String,
    pub provider_group_id: String,
    pub creation_response_sha256: String,
    pub readiness_response_sha256: String,
    pub captured_at: String,
    pub size_bytes: i64,
}

impl RuntimeSnapshotReadinessObservation {
    pub fn validate_for(&self, request: &RuntimeSnapshotReadinessRequest) -> Result<()> {
        request.validate()?;
        ensure!(
            self.schema == 1
                && self.operation_id == request.intent.operation_id
                && self.intent_digest == request.intent.digest()?
                && self.snapshot_id == request.locator.snapshot_id
                && self.source_occurrence_id == request.intent.source_occurrence_id
                && self.provider_group_id == request.intent.provider_group_id
                && self.creation_response_sha256 == request.locator.provider_response_sha256
                && self.size_bytes > 0,
            "runtime snapshot readiness changed its retained locator"
        );
        require_hash(&self.readiness_response_sha256, "readiness response")?;
        ensure!(
            self.captured_at.len() <= 64
                && !self.captured_at.is_empty()
                && self.captured_at.is_ascii(),
            "runtime snapshot readiness has invalid capture time"
        );
        Ok(())
    }
}

impl RuntimeSnapshotLocator {
    pub fn validate_for(&self, intent: &RuntimeSnapshotIntent) -> Result<()> {
        intent.validate()?;
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_RESULT_SCHEMA
                && self.operation_id == intent.operation_id
                && self.intent_digest == intent.digest()?,
            "runtime snapshot locator contradicts its durable intent"
        );
        for (label, value) in [
            ("provider response", &self.provider_response_sha256),
            ("adapter observation", &self.adapter_observation_sha256),
        ] {
            require_hash(value, label)?;
        }
        let observation = canonical_json(&self.provider_creation_observation)?;
        ensure!(
            self.provider_creation_observation.is_object()
                && !observation.is_empty()
                && observation.len() <= 4096
                && hex::encode(Sha256::digest(&observation)) == self.adapter_observation_sha256,
            "runtime snapshot creation observation changed its adapter identity"
        );
        for (label, value) in [
            ("source occurrence", &self.source_occurrence_id),
            ("provider group", &self.provider_group_id),
            ("snapshot", &self.snapshot_id),
        ] {
            require_bounded_text(value, 256, label)?;
        }
        ensure!(
            self.source_occurrence_id == intent.source_occurrence_id
                && self.provider_group_id == intent.provider_group_id,
            "runtime snapshot locator changed its source occurrence"
        );
        Ok(())
    }
}

fn require_hash(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "runtime snapshot {label} digest is invalid"
    );
    Ok(())
}

fn require_bounded_text(value: &str, maximum: usize, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= maximum
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b':')),
        "runtime snapshot {label} is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> RuntimeSnapshotIntent {
        let mut intent = RuntimeSnapshotIntent {
            schema: 1,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "2".repeat(64)),
            provider_id: "render-sandbox-early-access".into(),
            source_occurrence_id: "sbox-fixture-1".into(),
            provider_group_id: "sbg-fixture-1".into(),
            production_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            provider_spec_digest: "d".repeat(64),
            settings_digest: "5".repeat(64),
            product_witness_hash: "6".repeat(64),
            guest_runtime_manifest_hash: "7".repeat(64),
            owner_executable_sha256: "8".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode([3u8; 32])
            ),
            upload_sha256: "9".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: 42,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
    }

    #[test]
    fn source_and_delivery_changes_move_intent_identity() {
        let baseline = intent();
        let digest = baseline.digest().unwrap();
        for field in [
            "product_witness_hash",
            "guest_runtime_manifest_hash",
            "controller_public_root",
            "upload_sha256",
            "adapter_artifact_hash",
            "provider_spec_digest",
            "settings_digest",
            "production_profile_digest",
            "source_occurrence_id",
            "provider_group_id",
        ] {
            let mut changed = serde_json::to_value(&baseline).unwrap();
            changed[field] = serde_json::json!(if field == "controller_public_root" {
                format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode([4u8; 32])
                )
            } else if field == "source_occurrence_id" {
                "sbox-fixture-2".to_owned()
            } else if field == "provider_group_id" {
                "sbg-fixture-2".to_owned()
            } else {
                "a".repeat(64)
            });
            let mut changed: RuntimeSnapshotIntent = serde_json::from_value(changed).unwrap();
            assert!(changed.validate().is_err(), "{field} reused operation ID");
            changed.operation_id = changed.derived_operation_id().unwrap();
            assert_ne!(changed.digest().unwrap(), digest, "{field}");
        }
        let mut unknown = serde_json::to_value(&baseline).unwrap();
        unknown["credential"] = serde_json::json!("ambient");
        assert!(serde_json::from_value::<RuntimeSnapshotIntent>(unknown).is_err());
    }

    #[test]
    fn deadline_change_cannot_mint_another_attempt_opportunity() {
        let baseline = intent();
        let mut retried = baseline.clone();
        retried.attempt_deadline_ms += 100;
        assert_eq!(
            retried.derived_operation_id().unwrap(),
            baseline.operation_id
        );
        assert_ne!(retried.digest().unwrap(), baseline.digest().unwrap());
        retried.operation_id = "f".repeat(64);
        assert!(retried.validate().is_err());
    }

    #[test]
    fn locator_cannot_switch_source_after_attempt() {
        let intent = intent();
        let mut locator = RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: "snp-fixture-1".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: hex::encode(Sha256::digest(br#"{"schema":1}"#)),
        };
        locator.validate_for(&intent).unwrap();
        let mut changed_observation = locator.clone();
        changed_observation.provider_creation_observation["schema"] = serde_json::json!(2);
        assert!(changed_observation.validate_for(&intent).is_err());
        locator.source_occurrence_id = "sbox-other".into();
        assert!(locator.validate_for(&intent).is_err());
    }

    #[test]
    fn adapter_handoff_and_result_preserve_the_retained_attempt() {
        let intent = intent();
        let request = RuntimeSnapshotAdapterRequest {
            protocol: RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL.into(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
            upload_descriptor: 11,
            upload_bytes: intent.upload_bytes,
            upload_sha256: intent.upload_sha256.clone(),
            intent,
        };
        request.validate().unwrap();
        let uncertain = RuntimeSnapshotAdapterResponse::Uncertain {
            operation_id: request.intent.operation_id.clone(),
            intent_digest: request.intent.digest().unwrap(),
        };
        uncertain.validate_for(&request).unwrap();
        let mut wrong = request.clone();
        wrong.upload_sha256 = "b".repeat(64);
        assert!(wrong.validate().is_err());
        let mut wrong = uncertain;
        if let RuntimeSnapshotAdapterResponse::Uncertain { intent_digest, .. } = &mut wrong {
            *intent_digest = "f".repeat(64);
        }
        assert!(wrong.validate_for(&request).is_err());
    }

    #[test]
    fn readiness_is_read_only_and_bound_to_the_exact_locator() {
        let intent = intent();
        let locator = RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: "snp-fixture-1".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: hex::encode(Sha256::digest(br#"{"schema":1}"#)),
        };
        let request = RuntimeSnapshotReadinessRequest {
            protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
            intent,
            locator,
        };
        request.validate().unwrap();
        let observation = RuntimeSnapshotReadinessObservation {
            schema: 1,
            operation_id: request.intent.operation_id.clone(),
            intent_digest: request.intent.digest().unwrap(),
            snapshot_id: request.locator.snapshot_id.clone(),
            source_occurrence_id: request.locator.source_occurrence_id.clone(),
            provider_group_id: request.locator.provider_group_id.clone(),
            creation_response_sha256: request.locator.provider_response_sha256.clone(),
            readiness_response_sha256: "b".repeat(64),
            captured_at: "2026-09-28T00:01:00Z".into(),
            size_bytes: 4096,
        };
        observation.validate_for(&request).unwrap();
        let mut substituted = observation.clone();
        substituted.snapshot_id = "snp-other".into();
        assert!(substituted.validate_for(&request).is_err());
        let mut substituted = request;
        substituted.provider_spec_digest = "c".repeat(64);
        assert!(substituted.validate().is_err());
    }

    #[test]
    fn qualification_attempt_is_distinct_and_cannot_switch_snapshot() {
        let source = intent();
        let locator = RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: source.operation_id.clone(),
            intent_digest: source.digest().unwrap(),
            source_occurrence_id: source.source_occurrence_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            snapshot_id: "snp-fixture-1".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: hex::encode(Sha256::digest(br#"{"schema":1}"#)),
        };
        let mut qualification = RuntimeSnapshotQualificationIntent {
            schema: RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA,
            operation_id: String::new(),
            owner_principal: source.owner_principal.clone(),
            snapshot_operation_id: source.operation_id.clone(),
            snapshot_intent_digest: source.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            provider_id: source.provider_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            qualification_profile_digest: "1".repeat(64),
            adapter_artifact_hash: "2".repeat(64),
            provider_spec_digest: "3".repeat(64),
            settings_digest: "4".repeat(64),
            verifier_artifact_hash: "5".repeat(64),
            maximum_lifetime_seconds: 900,
            attempt_deadline_ms: source.attempt_deadline_ms + 1,
        };
        qualification.operation_id = qualification.derived_operation_id().unwrap();
        qualification.validate_for(&source, &locator).unwrap();
        let mut deadline_changed = qualification.clone();
        deadline_changed.attempt_deadline_ms += 1;
        assert_eq!(
            deadline_changed.derived_operation_id().unwrap(),
            qualification.operation_id
        );
        let mut switched = qualification.clone();
        switched.snapshot_id = "snp-other".into();
        assert!(switched.validate_for(&source, &locator).is_err());
        let mut switched = qualification;
        switched.verifier_artifact_hash = "6".repeat(64);
        assert!(switched.validate_for(&source, &locator).is_err());
    }
}
