//! Provider-neutral challenge and measurement for a restored guest owner tree.
//!
//! The guest returns measured bytes, not a provider identity. The controller
//! must independently bind its authenticated run/occurrence to the retained
//! snapshot locator and admitted verifier artifact before accepting this result.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::canonical_json;
use crate::runtime_snapshot::{
    RUNTIME_SNAPSHOT_READINESS_PROTOCOL, RuntimeSnapshotIntent, RuntimeSnapshotLocator,
    RuntimeSnapshotQualificationIntent, RuntimeSnapshotQualificationOccurrence,
    RuntimeSnapshotReadinessObservation, RuntimeSnapshotReadinessRequest,
};

pub const RESTORED_OWNER_MEASUREMENT_PROTOCOL: &str = "ryeos.restored-owner-measurement.v1";
pub const RESTORATION_VERIFIER_REMOTE_DIRECTORY: &str = "/ryeos/qualification";
pub const RESTORATION_VERIFIER_REMOTE_NAME: &str = "ryeos-external-guest-restoration-verifier";
pub const MAX_RESTORATION_VERIFIER_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_RESTORED_OWNER_CHALLENGE_BYTES: usize = 4096;
pub const MAX_RESTORED_OWNER_RESULT_BYTES: usize = 4096;
pub const RESTORED_VERIFIER_ADAPTER_PROTOCOL: &str = "ryeos.restored-verifier-adapter.v1";
pub const MAX_RESTORED_VERIFIER_ADAPTER_REQUEST_BYTES: usize = 32 * 1024;

/// Stable coordinates for a consumer verification on an already measured
/// qualification occurrence. These are commitments, not authentication or
/// permission to contact a provider. The daemon must join them to its born
/// accepted root, signed scenario and same-occurrence measurement journal.
/// Nonce, deadlines and upload representations deliberately do not belong
/// here: changing them must not mint another logical verification contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerRuntimeVerificationCoordinate {
    pub schema: u32,
    pub accepted_root_id: String,
    pub accepted_capsule_hash: String,
    pub qualification_purpose_digest: String,
    pub scenario_id: String,
    pub scenario_source_digest: String,
    pub subject_digest: String,
    pub use_digest: String,
    pub prerequisite_measurement_attempt_id: String,
    pub prerequisite_measurement_observation_digest: String,
}

/// Protected selection for a consumer-verifier artifact, distinct from the
/// prerequisite owner-measurement artifact. The artifact must still be
/// resolved and sealed by the installed runtime owner before contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerRuntimeVerifierSelection {
    pub scenario_source_digest: String,
    pub verifier_artifact_hash: String,
}

impl ConsumerRuntimeVerifierSelection {
    pub fn validate(&self) -> Result<()> {
        require_hash(&self.scenario_source_digest, "consumer scenario source")?;
        require_hash(&self.verifier_artifact_hash, "consumer verifier artifact")
    }

    pub fn validate_coordinate(
        &self,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
    ) -> Result<()> {
        self.validate()?;
        coordinate.validate()?;
        ensure!(
            self.scenario_source_digest == coordinate.scenario_source_digest,
            "consumer verifier selection differs from accepted scenario source"
        );
        Ok(())
    }
}

impl ConsumerRuntimeVerificationCoordinate {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported consumer verification coordinate"
        );
        ensure!(
            self.accepted_root_id.starts_with("T-")
                && self.accepted_root_id.len() <= 128
                && self.accepted_root_id.len() > 2
                && self
                    .accepted_root_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "consumer verification root coordinate is invalid"
        );
        ensure!(
            !self.scenario_id.is_empty()
                && self.scenario_id.len() <= 128
                && self
                    .scenario_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "consumer verification scenario coordinate is invalid"
        );
        for (label, digest) in [
            ("accepted capsule", &self.accepted_capsule_hash),
            ("qualification purpose", &self.qualification_purpose_digest),
            ("scenario source", &self.scenario_source_digest),
            ("subject", &self.subject_digest),
            ("use", &self.use_digest),
            (
                "prerequisite attempt",
                &self.prerequisite_measurement_attempt_id,
            ),
            (
                "prerequisite observation",
                &self.prerequisite_measurement_observation_digest,
            ),
        ] {
            require_hash(digest, label)?;
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(hex::encode(Sha256::digest(canonical_json(&(
            "ryeos.consumer-runtime-verification-coordinate.v1",
            self,
        ))?)))
    }
}

/// Sealed, one-contact handoff. The descriptor number is process-local; the
/// exact upload bytes and hash remain part of the retained attempt. No field
/// here grants a second attempt or qualifies the restored runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredVerifierAdapterRequest {
    pub protocol: String,
    pub intent: RestoredVerifierAttemptIntent,
    pub source_intent: RuntimeSnapshotIntent,
    pub locator: RuntimeSnapshotLocator,
    pub readiness: RuntimeSnapshotReadinessObservation,
    pub qualification_intent: RuntimeSnapshotQualificationIntent,
    pub occurrence: RuntimeSnapshotQualificationOccurrence,
    pub provider_spec_digest: String,
    pub upload_descriptor: u32,
    pub upload_bytes: u64,
    pub upload_sha256: String,
}

impl RestoredVerifierAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.intent.validate_for(
            &self.source_intent,
            &self.locator,
            &self.qualification_intent,
            &self.occurrence,
        )?;
        let readiness_request = RuntimeSnapshotReadinessRequest {
            protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
            intent: self.source_intent.clone(),
            locator: self.locator.clone(),
            provider_spec_digest: self.source_intent.provider_spec_digest.clone(),
        };
        self.readiness.validate_for(&readiness_request)?;
        ensure!(
            self.protocol == RESTORED_VERIFIER_ADAPTER_PROTOCOL
                && self.provider_spec_digest == self.qualification_intent.provider_spec_digest
                && self.upload_descriptor > 2
                && self.upload_bytes == self.intent.upload_bytes
                && self.upload_sha256 == self.intent.upload_sha256,
            "restored verifier handoff changed its signed or sealed authority"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_RESTORED_VERIFIER_ADAPTER_REQUEST_BYTES,
            "restored verifier handoff exceeds its bound"
        );
        Ok(())
    }
}

/// Complete adapter-observed upload and run stream, still not a qualification
/// claim. The daemon must authenticate the admitted adapter attempt and join
/// its execution evidence before treating this content as a measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredVerifierAdapterObservation {
    pub schema: u32,
    pub operation_id: String,
    pub occurrence_id: String,
    pub upload_token_execution_id: String,
    pub run_token_execution_id: String,
    pub upload_response_sha256: String,
    pub run_stream_sha256: String,
    pub measurement: RestoredOwnerMeasurement,
    /// Replaced by daemon-observed timing before durable retention. The
    /// adapter must emit false and cannot author host deadline testimony.
    pub contact_deadline_exceeded: bool,
}

impl RestoredVerifierAdapterObservation {
    pub fn validate_for(&self, request: &RestoredVerifierAdapterRequest) -> Result<()> {
        request.validate()?;
        self.validate_for_retained(
            &request.intent,
            &request.source_intent,
            &request.locator,
            &request.readiness,
            &request.qualification_intent,
            &request.occurrence,
        )
    }

    /// Validate replayed content against retained authorities without
    /// inventing a process-local descriptor from the old adapter invocation.
    pub fn validate_for_retained(
        &self,
        intent: &RestoredVerifierAttemptIntent,
        source: &RuntimeSnapshotIntent,
        locator: &RuntimeSnapshotLocator,
        readiness: &RuntimeSnapshotReadinessObservation,
        qualification: &RuntimeSnapshotQualificationIntent,
        occurrence: &RuntimeSnapshotQualificationOccurrence,
    ) -> Result<()> {
        intent.validate_for(source, locator, qualification, occurrence)?;
        let readiness_request = RuntimeSnapshotReadinessRequest {
            protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
            intent: source.clone(),
            locator: locator.clone(),
            provider_spec_digest: source.provider_spec_digest.clone(),
        };
        readiness.validate_for(&readiness_request)?;
        ensure!(
            self.schema == 1
                && self.operation_id == intent.operation_id
                && self.occurrence_id == occurrence.occurrence_id,
            "restored verifier observation changed its attempt or occurrence"
        );
        for execution_id in [
            &self.upload_token_execution_id,
            &self.run_token_execution_id,
        ] {
            ensure!(
                execution_id.starts_with("exe-")
                    && execution_id.len() <= 128
                    && execution_id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
                "restored verifier token execution identity is invalid"
            );
        }
        require_hash(&self.upload_response_sha256, "upload response")?;
        require_hash(&self.run_stream_sha256, "run stream")?;
        self.measurement.validate_content_for_bound_snapshot(
            intent.owner_challenge()?,
            source,
            locator,
            readiness,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RestoredVerifierAdapterResponse {
    Observed {
        observation: Box<RestoredVerifierAdapterObservation>,
    },
    Uncertain {
        operation_id: String,
    },
}

impl RestoredVerifierAdapterResponse {
    pub fn validate_for(&self, request: &RestoredVerifierAdapterRequest) -> Result<()> {
        request.validate()?;
        match self {
            Self::Observed { observation } => observation.validate_for(request),
            Self::Uncertain { operation_id } => {
                ensure!(
                    operation_id == &request.intent.operation_id,
                    "uncertain verifier result changed its durable attempt"
                );
                Ok(())
            }
        }
    }
}

/// One independently owned verifier contact after the restored occurrence is
/// bound. A new nonce or deadline cannot mint another provider attempt for the
/// same exact occurrence and admitted verifier. Replay must recover this whole
/// retained intent, including its original challenge, rather than construct a
/// replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredVerifierAttemptIntent {
    pub schema: u32,
    pub operation_id: String,
    pub qualification_operation_id: String,
    pub restored_occurrence_id: String,
    pub verifier_artifact_hash: String,
    pub upload_sha256: String,
    pub upload_bytes: u64,
    pub purpose: RemoteVerificationPurpose,
    pub attempt_deadline_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteVerificationPurpose {
    OwnerMeasurement {
        challenge: RestoredOwnerChallenge,
    },
    ConsumerRuntime {
        coordinate: ConsumerRuntimeVerificationCoordinate,
        nonce_hex: String,
    },
}

impl RemoteVerificationPurpose {
    /// Logical purpose only. Challenge randomness cannot create another
    /// attempt; the journal compares complete immutable intent separately.
    fn coordinate_digest(&self) -> Result<String> {
        let value = match self {
            Self::OwnerMeasurement { challenge } => {
                challenge.validate()?;
                serde_json::json!({"kind": "owner_measurement"})
            }
            Self::ConsumerRuntime {
                coordinate,
                nonce_hex,
            } => {
                coordinate.validate()?;
                require_hash(nonce_hex, "consumer challenge nonce")?;
                serde_json::json!({"kind": "consumer_runtime", "coordinate": coordinate})
            }
        };
        Ok(hex::encode(Sha256::digest(canonical_json(&value)?)))
    }
}

impl RestoredVerifierAttemptIntent {
    pub fn owner_challenge(&self) -> Result<&RestoredOwnerChallenge> {
        match &self.purpose {
            RemoteVerificationPurpose::OwnerMeasurement { challenge } => Ok(challenge),
            RemoteVerificationPurpose::ConsumerRuntime { .. } => {
                anyhow::bail!("consumer verification cannot use owner-measurement contact")
            }
        }
    }

    pub fn validate_for(
        &self,
        source: &RuntimeSnapshotIntent,
        locator: &RuntimeSnapshotLocator,
        qualification: &RuntimeSnapshotQualificationIntent,
        occurrence: &RuntimeSnapshotQualificationOccurrence,
    ) -> Result<()> {
        qualification.validate_for(source, locator)?;
        occurrence.validate_for(qualification)?;
        let challenge = self.owner_challenge()?;
        challenge.validate_for(source, locator)?;
        ensure!(
            self.schema == 2
                && self.qualification_operation_id == qualification.operation_id
                && self.restored_occurrence_id == occurrence.occurrence_id
                && challenge.restored_occurrence_id == occurrence.occurrence_id
                && self.verifier_artifact_hash == qualification.verifier_artifact_hash
                && !occurrence.contact_deadline_exceeded
                && self.upload_bytes > 0
                && self.upload_bytes <= MAX_RESTORATION_VERIFIER_BYTES + 16 * 1024
                && self.attempt_deadline_ms > 0,
            "restored verifier attempt differs from timely qualified occurrence"
        );
        require_hash(&self.upload_sha256, "upload")?;
        ensure!(
            self.operation_id == self.derived_operation_id()?,
            "restored verifier attempt changed its durable identity"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_RESTORED_OWNER_CHALLENGE_BYTES + 2048,
            "restored verifier attempt exceeds its bound"
        );
        Ok(())
    }

    /// The challenge, wall-clock deadline and upload representation are
    /// retained attempt data, not coordinates that mint another contact.
    /// Reservation replay compares the entire intent, including those fields.
    pub fn derived_operation_id(&self) -> Result<String> {
        let coordinates = (
            "ryeos.restored-verifier-attempt.v2",
            &self.qualification_operation_id,
            &self.restored_occurrence_id,
            &self.verifier_artifact_hash,
            self.purpose.coordinate_digest()?,
        );
        Ok(hex::encode(Sha256::digest(canonical_json(&coordinates)?)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredOwnerChallenge {
    pub schema: u32,
    pub protocol: String,
    pub operation_id: String,
    pub snapshot_id: String,
    pub restored_occurrence_id: String,
    pub nonce_hex: String,
}

impl RestoredOwnerChallenge {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.protocol == RESTORED_OWNER_MEASUREMENT_PROTOCOL,
            "unsupported restored-owner challenge"
        );
        require_hash(&self.operation_id, "operation")?;
        require_hash(&self.nonce_hex, "nonce")?;
        for (label, value) in [
            ("snapshot", &self.snapshot_id),
            ("restored occurrence", &self.restored_occurrence_id),
        ] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 256
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
                "restored-owner {label} identity is invalid"
            );
        }
        ensure!(
            canonical_json(self)?.len() <= MAX_RESTORED_OWNER_CHALLENGE_BYTES,
            "restored-owner challenge exceeds its bound"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }

    pub fn validate_for(
        &self,
        intent: &RuntimeSnapshotIntent,
        locator: &RuntimeSnapshotLocator,
    ) -> Result<()> {
        self.validate()?;
        locator.validate_for(intent)?;
        ensure!(
            self.operation_id == intent.operation_id
                && self.snapshot_id == locator.snapshot_id
                && self.restored_occurrence_id != locator.source_occurrence_id,
            "restored-owner challenge differs from bound snapshot or source"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredOwnerMeasurement {
    pub schema: u32,
    pub protocol: String,
    pub challenge_digest: String,
    pub manifest_hash: String,
    pub owner_executable_sha256: String,
    pub controller_public_root: String,
}

impl RestoredOwnerMeasurement {
    pub fn validate_for(
        &self,
        challenge: &RestoredOwnerChallenge,
        intent: &RuntimeSnapshotIntent,
    ) -> Result<()> {
        challenge.validate()?;
        intent.validate()?;
        ensure!(
            self.schema == 1
                && self.protocol == RESTORED_OWNER_MEASUREMENT_PROTOCOL
                && self.challenge_digest == challenge.digest()?
                && challenge.operation_id == intent.operation_id
                && self.manifest_hash == intent.guest_runtime_manifest_hash
                && self.owner_executable_sha256 == intent.owner_executable_sha256
                && self.controller_public_root == intent.controller_public_root,
            "restored owner measurement differs from exact retained product"
        );
        require_hash(&self.challenge_digest, "challenge")?;
        ensure!(
            canonical_json(self)?.len() <= MAX_RESTORED_OWNER_RESULT_BYTES,
            "restored owner measurement exceeds its bound"
        );
        Ok(())
    }

    /// Join only the content and provider-readiness coordinates. This is not
    /// qualification: the caller must still prove that the admitted verifier
    /// executed inside the exact authenticated restored occurrence and that
    /// its complete output came from that run.
    pub fn validate_content_for_bound_snapshot(
        &self,
        challenge: &RestoredOwnerChallenge,
        intent: &RuntimeSnapshotIntent,
        locator: &RuntimeSnapshotLocator,
        readiness: &RuntimeSnapshotReadinessObservation,
    ) -> Result<()> {
        challenge.validate_for(intent, locator)?;
        let readiness_request = RuntimeSnapshotReadinessRequest {
            protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
            intent: intent.clone(),
            locator: locator.clone(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
        };
        readiness.validate_for(&readiness_request)?;
        self.validate_for(challenge, intent)
    }
}

fn require_hash(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "restored-owner {label} digest is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn consumer_coordinate_binds_each_authoritative_identity() {
        let coordinate = super::ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-fixture-root".into(),
            accepted_capsule_hash: "a".repeat(64),
            qualification_purpose_digest: "b".repeat(64),
            scenario_id: "routed-consumer".into(),
            scenario_source_digest: "c".repeat(64),
            subject_digest: "d".repeat(64),
            use_digest: "e".repeat(64),
            prerequisite_measurement_attempt_id: "f".repeat(64),
            prerequisite_measurement_observation_digest: "1".repeat(64),
        };
        let original = coordinate.digest().unwrap();
        let selection = super::ConsumerRuntimeVerifierSelection {
            scenario_source_digest: coordinate.scenario_source_digest.clone(),
            verifier_artifact_hash: "3".repeat(64),
        };
        selection.validate_coordinate(&coordinate).unwrap();
        let mut mismatched = selection.clone();
        mismatched.scenario_source_digest = "4".repeat(64);
        assert!(mismatched.validate_coordinate(&coordinate).is_err());
        mismatched = selection;
        mismatched.verifier_artifact_hash.clear();
        assert!(mismatched.validate().is_err());
        let value = serde_json::to_value(&coordinate).unwrap();
        for field in [
            "accepted_capsule_hash",
            "qualification_purpose_digest",
            "scenario_source_digest",
            "subject_digest",
            "use_digest",
            "prerequisite_measurement_attempt_id",
            "prerequisite_measurement_observation_digest",
        ] {
            let mut changed = value.clone();
            changed[field] = serde_json::json!("2".repeat(64));
            let changed: super::ConsumerRuntimeVerificationCoordinate =
                serde_json::from_value(changed).unwrap();
            assert_ne!(changed.digest().unwrap(), original, "{field}");
        }
        let mut changed = coordinate.clone();
        changed.accepted_root_id = "T-other-root".into();
        assert_ne!(changed.digest().unwrap(), original);
        changed = coordinate.clone();
        changed.scenario_id = "different-scenario".into();
        assert_ne!(changed.digest().unwrap(), original);
        changed = coordinate.clone();
        changed.prerequisite_measurement_observation_digest.clear();
        assert!(changed.validate().is_err());
        let mut excess = value;
        excess["qualified"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<super::ConsumerRuntimeVerificationCoordinate>(excess).is_err()
        );
    }

    use super::*;
    use base64::Engine as _;

    #[test]
    fn verifier_attempt_identity_cannot_be_reminted_by_nonce_or_deadline() {
        let mut attempt = RestoredVerifierAttemptIntent {
            schema: 2,
            operation_id: String::new(),
            qualification_operation_id: "1".repeat(64),
            restored_occurrence_id: "sbx-exact".into(),
            verifier_artifact_hash: "2".repeat(64),
            upload_sha256: "3".repeat(64),
            upload_bytes: 1024,
            purpose: RemoteVerificationPurpose::OwnerMeasurement {
                challenge: RestoredOwnerChallenge {
                    schema: 1,
                    protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
                    operation_id: "4".repeat(64),
                    snapshot_id: "snp-exact".into(),
                    restored_occurrence_id: "sbx-exact".into(),
                    nonce_hex: "5".repeat(64),
                },
            },
            attempt_deadline_ms: 42,
        };
        let identity = attempt.derived_operation_id().unwrap();
        let RemoteVerificationPurpose::OwnerMeasurement { challenge } = &mut attempt.purpose else {
            unreachable!()
        };
        challenge.nonce_hex = "6".repeat(64);
        attempt.attempt_deadline_ms += 1;
        assert_eq!(attempt.derived_operation_id().unwrap(), identity);
        attempt.restored_occurrence_id = "sbx-other".into();
        assert_ne!(attempt.derived_operation_id().unwrap(), identity);
        attempt.restored_occurrence_id = "sbx-exact".into();
        attempt.upload_sha256 = "7".repeat(64);
        assert_eq!(attempt.derived_operation_id().unwrap(), identity);
        let coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-consumer-root".into(),
            accepted_capsule_hash: "a".repeat(64),
            qualification_purpose_digest: "b".repeat(64),
            scenario_id: "routed-consumer".into(),
            scenario_source_digest: "c".repeat(64),
            subject_digest: "d".repeat(64),
            use_digest: "e".repeat(64),
            prerequisite_measurement_attempt_id: "f".repeat(64),
            prerequisite_measurement_observation_digest: "1".repeat(64),
        };
        attempt.purpose = RemoteVerificationPurpose::ConsumerRuntime {
            coordinate,
            nonce_hex: "2".repeat(64),
        };
        let consumer_identity = attempt.derived_operation_id().unwrap();
        assert_ne!(consumer_identity, identity);
        assert!(attempt.owner_challenge().is_err());
        let RemoteVerificationPurpose::ConsumerRuntime { nonce_hex, .. } = &mut attempt.purpose
        else {
            unreachable!()
        };
        *nonce_hex = "3".repeat(64);
        assert_eq!(attempt.derived_operation_id().unwrap(), consumer_identity);
        let RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. } = &mut attempt.purpose
        else {
            unreachable!()
        };
        coordinate.use_digest = "4".repeat(64);
        assert_ne!(attempt.derived_operation_id().unwrap(), consumer_identity);
    }

    #[test]
    fn challenge_cannot_be_reused_for_another_snapshot_or_occurrence() {
        let challenge = RestoredOwnerChallenge {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: "1".repeat(64),
            snapshot_id: "snp-exact".into(),
            restored_occurrence_id: "sbx-exact".into(),
            nonce_hex: "2".repeat(64),
        };
        let digest = challenge.digest().unwrap();
        let mut changed = challenge.clone();
        changed.snapshot_id = "snp-other".into();
        assert_ne!(changed.digest().unwrap(), digest);
        changed = challenge.clone();
        changed.restored_occurrence_id = "sbx-other".into();
        assert_ne!(changed.digest().unwrap(), digest);
        changed = challenge;
        changed.nonce_hex = "3".repeat(64);
        assert_ne!(changed.digest().unwrap(), digest);
    }

    #[test]
    fn measurement_must_match_retained_product_and_fresh_challenge() {
        let mut intent = RuntimeSnapshotIntent {
            schema: crate::runtime_snapshot::RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "1".repeat(64)),
            provider_id: "provider".into(),
            source_occurrence_id: "source".into(),
            source_bootstrap_operation_id: None,
            source_created_at: None,
            source_timeout_seconds: None,
            provider_group_id: "group".into(),
            production_profile_digest: "2".repeat(64),
            adapter_artifact_hash: "3".repeat(64),
            provider_spec_digest: "4".repeat(64),
            settings_digest: "5".repeat(64),
            source: crate::runtime_snapshot::RuntimeSnapshotSource::CapturedProduct {
                product_witness_hash: "6".repeat(64),
            },
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
        let challenge = RestoredOwnerChallenge {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: intent.operation_id.clone(),
            snapshot_id: "snapshot".into(),
            restored_occurrence_id: "restored".into(),
            nonce_hex: "a".repeat(64),
        };
        let measured = RestoredOwnerMeasurement {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            challenge_digest: challenge.digest().unwrap(),
            manifest_hash: intent.guest_runtime_manifest_hash.clone(),
            owner_executable_sha256: intent.owner_executable_sha256.clone(),
            controller_public_root: intent.controller_public_root.clone(),
        };
        measured.validate_for(&challenge, &intent).unwrap();
        let locator = RuntimeSnapshotLocator {
            schema: crate::runtime_snapshot::RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: challenge.snapshot_id.clone(),
            provider_response_sha256: "c".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: hex::encode(Sha256::digest(br#"{"schema":1}"#)),
        };
        let readiness = RuntimeSnapshotReadinessObservation {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            creation_response_sha256: locator.provider_response_sha256.clone(),
            readiness_response_sha256: "d".repeat(64),
            captured_at: "2026-09-28T00:01:00Z".into(),
            size_bytes: 4096,
        };
        measured
            .validate_content_for_bound_snapshot(&challenge, &intent, &locator, &readiness)
            .unwrap();
        let mut wrong_locator = locator.clone();
        wrong_locator.snapshot_id = "different".into();
        assert!(
            measured
                .validate_content_for_bound_snapshot(
                    &challenge,
                    &intent,
                    &wrong_locator,
                    &readiness
                )
                .is_err()
        );
        let mut wrong_readiness = readiness;
        wrong_readiness.creation_response_sha256 = "e".repeat(64);
        assert!(
            measured
                .validate_content_for_bound_snapshot(
                    &challenge,
                    &intent,
                    &locator,
                    &wrong_readiness
                )
                .is_err()
        );
        let mut wrong = measured.clone();
        wrong.owner_executable_sha256 = "b".repeat(64);
        assert!(wrong.validate_for(&challenge, &intent).is_err());
        wrong = measured;
        wrong.challenge_digest = "b".repeat(64);
        assert!(wrong.validate_for(&challenge, &intent).is_err());
    }
}
