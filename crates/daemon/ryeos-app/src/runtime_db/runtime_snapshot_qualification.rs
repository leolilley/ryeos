//! One-attempt restored-Sandbox qualification journal, separate from Worker
//! allocation and from snapshot production. Provider contact never follows a
//! replayed pending or quarantined row.

use super::*;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::runtime_snapshot::{
    RuntimeSnapshotQualificationIntent, RuntimeSnapshotQualificationOccurrence,
};

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE runtime_snapshot_qualification (
    operation_id TEXT PRIMARY KEY,
    snapshot_operation_id TEXT NOT NULL REFERENCES runtime_snapshot_operation(operation_id),
    intent_json TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined','occurrence_bound')),
    occurrence_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((phase='occurrence_bound') = (occurrence_json IS NOT NULL))
);
CREATE TRIGGER runtime_snapshot_qualification_immutable_intent
BEFORE UPDATE ON runtime_snapshot_qualification
WHEN NEW.operation_id!=OLD.operation_id
    OR NEW.snapshot_operation_id!=OLD.snapshot_operation_id
    OR NEW.intent_json!=OLD.intent_json
    OR NEW.created_at_ms!=OLD.created_at_ms
BEGIN SELECT RAISE(ABORT, 'snapshot qualification intent is immutable'); END;
CREATE TRIGGER runtime_snapshot_qualification_no_delete
BEFORE DELETE ON runtime_snapshot_qualification
BEGIN SELECT RAISE(ABORT, 'snapshot qualification operation is retained'); END;
CREATE TRIGGER runtime_snapshot_qualification_immutable_occurrence
BEFORE UPDATE ON runtime_snapshot_qualification
WHEN OLD.occurrence_json IS NOT NULL AND NEW.occurrence_json!=OLD.occurrence_json
BEGIN SELECT RAISE(ABORT, 'snapshot qualification occurrence is immutable'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotQualificationPhase {
    Reserved,
    AttemptPending,
    Quarantined,
    OccurrenceBound,
}

impl SnapshotQualificationPhase {
    fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            "occurrence_bound" => Self::OccurrenceBound,
            _ => anyhow::bail!("snapshot qualification has an invalid phase"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotQualificationRecord {
    pub intent: RuntimeSnapshotQualificationIntent,
    pub phase: SnapshotQualificationPhase,
    pub occurrence: Option<RuntimeSnapshotQualificationOccurrence>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotQualificationAttemptClaim {
    StartAttempt(SnapshotQualificationRecord),
    Reconcile(SnapshotQualificationRecord),
    OccurrenceBound(SnapshotQualificationRecord),
}

fn canonical<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(
        ryeos_external_execution_contract::canonical_json(value)?,
    )?)
}

pub(super) fn read(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<SnapshotQualificationRecord>> {
    let row: Option<(String, String, String, Option<String>, i64, i64)> = conn
        .query_row(
            "SELECT snapshot_operation_id,intent_json,phase,occurrence_json,created_at_ms,updated_at_ms
             FROM runtime_snapshot_qualification WHERE operation_id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .optional()?;
    let Some((source_id, intent_json, phase, occurrence_json, created, updated)) = row else {
        return Ok(None);
    };
    ensure!(
        intent_json.len() <= 24 * 1024,
        "snapshot qualification intent exceeds bound"
    );
    let intent: RuntimeSnapshotQualificationIntent = serde_json::from_str(&intent_json)?;
    let source = super::runtime_snapshot::read(conn, &source_id)?
        .context("snapshot qualification source disappeared")?;
    let locator = source
        .locator
        .as_ref()
        .context("snapshot qualification source has no locator")?;
    ensure!(
        source.readiness.is_some(),
        "snapshot qualification source has no readiness"
    );
    intent.validate_for(&source.intent, locator)?;
    ensure!(
        intent.operation_id == operation_id
            && intent.snapshot_operation_id == source_id
            && canonical(&intent)? == intent_json,
        "snapshot qualification retained intent changed identity"
    );
    let phase = SnapshotQualificationPhase::parse(&phase)?;
    let occurrence = occurrence_json
        .map(|raw| {
            ensure!(
                raw.len() <= 1024,
                "snapshot qualification occurrence exceeds bound"
            );
            let value: RuntimeSnapshotQualificationOccurrence = serde_json::from_str(&raw)?;
            value.validate_for(&intent)?;
            ensure!(
                canonical(&value)? == raw,
                "snapshot qualification occurrence is noncanonical"
            );
            Ok(value)
        })
        .transpose()?;
    ensure!(
        (phase == SnapshotQualificationPhase::OccurrenceBound) == occurrence.is_some()
            && created > 0
            && updated >= created,
        "snapshot qualification phase contradicts retained evidence"
    );
    Ok(Some(SnapshotQualificationRecord {
        intent,
        phase,
        occurrence,
        created_at_ms: created,
        updated_at_ms: updated,
    }))
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let mut statement = conn
        .prepare("SELECT operation_id FROM runtime_snapshot_qualification ORDER BY operation_id")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        read(conn, &id)?.context("snapshot qualification row disappeared")?;
    }
    Ok(())
}

impl RuntimeDb {
    pub fn snapshot_qualification_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<SnapshotQualificationRecord>> {
        read(&self.conn, operation_id)
    }

    pub fn reserve_snapshot_qualification(
        &self,
        intent: &RuntimeSnapshotQualificationIntent,
    ) -> Result<SnapshotQualificationRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            ensure!(
                existing.intent == *intent,
                "snapshot qualification reservation replay changed"
            );
            tx.commit()?;
            return Ok(existing);
        }
        let source = super::runtime_snapshot::read(&tx, &intent.snapshot_operation_id)?
            .context("snapshot qualification source was not retained")?;
        let locator = source
            .locator
            .as_ref()
            .context("snapshot qualification source has no locator")?;
        ensure!(
            source.readiness.is_some(),
            "snapshot qualification source is not ready"
        );
        intent.validate_for(&source.intent, locator)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms.saturating_sub(now) <= 300_000,
            "snapshot qualification deadline is outside its admission window"
        );
        tx.execute(
            "INSERT INTO runtime_snapshot_qualification
             (operation_id,snapshot_operation_id,intent_json,phase,occurrence_json,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,'reserved',NULL,?4,?4)",
            params![intent.operation_id, intent.snapshot_operation_id, canonical(intent)?, now],
        )?;
        let record = read(&tx, &intent.operation_id)?
            .context("snapshot qualification reservation vanished")?;
        tx.commit()?;
        Ok(record)
    }

    pub fn claim_snapshot_qualification_attempt(
        &self,
        operation_id: &str,
    ) -> Result<SnapshotQualificationAttemptClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("snapshot qualification was not reserved")?;
        match record.phase {
            SnapshotQualificationPhase::OccurrenceBound => {
                tx.commit()?;
                return Ok(SnapshotQualificationAttemptClaim::OccurrenceBound(record));
            }
            SnapshotQualificationPhase::AttemptPending
            | SnapshotQualificationPhase::Quarantined => {
                tx.commit()?;
                return Ok(SnapshotQualificationAttemptClaim::Reconcile(record));
            }
            SnapshotQualificationPhase::Reserved => {}
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < record.intent.attempt_deadline_ms,
            "snapshot qualification deadline expired"
        );
        let changed = tx.execute(
            "UPDATE runtime_snapshot_qualification SET phase='attempt_pending',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='reserved'",
            params![operation_id, now],
        )?;
        ensure!(
            changed == 1,
            "snapshot qualification attempt lost durable CAS"
        );
        let current =
            read(&tx, operation_id)?.context("snapshot qualification attempt vanished")?;
        tx.commit()?;
        Ok(SnapshotQualificationAttemptClaim::StartAttempt(current))
    }

    pub fn bind_snapshot_qualification_occurrence(
        &self,
        occurrence: &RuntimeSnapshotQualificationOccurrence,
    ) -> Result<SnapshotQualificationRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &occurrence.operation_id)?
            .context("snapshot qualification has no attempt claim")?;
        occurrence.validate_for(&record.intent)?;
        if record.phase == SnapshotQualificationPhase::OccurrenceBound {
            ensure!(
                record.occurrence.as_ref() == Some(occurrence),
                "snapshot qualification occurrence replay changed"
            );
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            matches!(
                record.phase,
                SnapshotQualificationPhase::AttemptPending
                    | SnapshotQualificationPhase::Quarantined
            ),
            "snapshot qualification occurrence did not follow attempt"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let changed = tx.execute(
            "UPDATE runtime_snapshot_qualification SET phase='occurrence_bound',occurrence_json=?2,updated_at_ms=?3
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![occurrence.operation_id, canonical(occurrence)?, now],
        )?;
        ensure!(
            changed == 1,
            "snapshot qualification occurrence bind lost durable CAS"
        );
        let current = read(&tx, &occurrence.operation_id)?
            .context("snapshot qualification occurrence vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn quarantine_snapshot_qualification_attempt(
        &self,
        operation_id: &str,
    ) -> Result<SnapshotQualificationRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record =
            read(&tx, operation_id)?.context("snapshot qualification attempt is absent")?;
        if record.phase == SnapshotQualificationPhase::Quarantined {
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            record.phase == SnapshotQualificationPhase::AttemptPending,
            "snapshot qualification quarantine requires pending attempt"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "UPDATE runtime_snapshot_qualification SET phase='quarantined',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='attempt_pending'",
            params![operation_id, now],
        )?;
        let current =
            read(&tx, operation_id)?.context("snapshot qualification quarantine vanished")?;
        tx.commit()?;
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::restored_runtime_measurement::{
        ConsumerRuntimeVerificationCoordinate, RESTORED_OWNER_MEASUREMENT_PROTOCOL,
        RestoredOwnerChallenge, RestoredOwnerMeasurement, RestoredVerifierAdapterObservation,
        RestoredVerifierAttemptIntent,
    };
    use ryeos_external_execution_contract::runtime_snapshot::{
        RUNTIME_SNAPSHOT_INTENT_SCHEMA, RUNTIME_SNAPSHOT_RESULT_SCHEMA, RuntimeSnapshotIntent,
        RuntimeSnapshotLocator, RuntimeSnapshotQualificationTerminalObservation,
        RuntimeSnapshotQualificationTerminationIntent, RuntimeSnapshotReadinessObservation,
    };
    use sha2::{Digest, Sha256};

    #[test]
    fn qualified_sandbox_create_has_one_durable_contact_attempt() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        let mut source = RuntimeSnapshotIntent {
            schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "2".repeat(64)),
            provider_id: "render-sandbox-early-access".into(),
            source_occurrence_id: "sbx-source".into(),
            source_bootstrap_operation_id: None,
            source_created_at: None,
            source_timeout_seconds: None,
            provider_group_id: "sbg-group".into(),
            production_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            provider_spec_digest: "5".repeat(64),
            settings_digest: "6".repeat(64),
            source: ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotSource::CapturedProduct {
                product_witness_hash: "7".repeat(64),
            },
            guest_runtime_manifest_hash: "8".repeat(64),
            owner_executable_sha256: "9".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [3u8; 32])
            ),
            upload_sha256: "a".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: now + 60_000,
        };
        source.operation_id = source.derived_operation_id().unwrap();
        db.reserve_runtime_snapshot(&source).unwrap();
        db.claim_runtime_snapshot_attempt(&source.operation_id, &source.digest().unwrap())
            .unwrap();
        let locator = RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: source.operation_id.clone(),
            intent_digest: source.digest().unwrap(),
            source_occurrence_id: source.source_occurrence_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            snapshot_id: "snp-exact".into(),
            provider_response_sha256: "b".repeat(64),
            provider_creation_observation: serde_json::json!({"schema":1}),
            adapter_observation_sha256: lillux::sha256_hex(br#"{"schema":1}"#),
        };
        db.bind_runtime_snapshot_locator(&locator, false).unwrap();
        let readiness = RuntimeSnapshotReadinessObservation {
            schema: 1,
            operation_id: source.operation_id.clone(),
            intent_digest: source.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            source_occurrence_id: source.source_occurrence_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            creation_response_sha256: locator.provider_response_sha256.clone(),
            readiness_response_sha256: "c".repeat(64),
            captured_at: "2026-09-28T00:01:00Z".into(),
            size_bytes: 4096,
        };
        db.bind_runtime_snapshot_readiness(&readiness).unwrap();
        let mut intent = RuntimeSnapshotQualificationIntent {
            schema: 1,
            operation_id: String::new(),
            owner_principal: source.owner_principal.clone(),
            snapshot_operation_id: source.operation_id.clone(),
            snapshot_intent_digest: source.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            provider_id: source.provider_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            qualification_profile_digest: "d".repeat(64),
            adapter_artifact_hash: "e".repeat(64),
            provider_spec_digest: "f".repeat(64),
            settings_digest: "1".repeat(64),
            verifier_artifact_hash: "2".repeat(64),
            maximum_lifetime_seconds: 900,
            attempt_deadline_ms: now + 60_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        db.reserve_snapshot_qualification(&intent).unwrap();
        assert!(matches!(
            db.claim_snapshot_qualification_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotQualificationAttemptClaim::StartAttempt(_)
        ));
        assert!(matches!(
            db.claim_snapshot_qualification_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotQualificationAttemptClaim::Reconcile(_)
        ));
        db.quarantine_snapshot_qualification_attempt(&intent.operation_id)
            .unwrap();
        assert!(matches!(
            db.claim_snapshot_qualification_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotQualificationAttemptClaim::Reconcile(_)
        ));
        let occurrence = RuntimeSnapshotQualificationOccurrence {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: "sbx-restored".into(),
            provider_response_sha256: "3".repeat(64),
            contact_deadline_exceeded: false,
        };
        db.bind_snapshot_qualification_occurrence(&occurrence)
            .unwrap();
        assert!(matches!(
            db.claim_snapshot_qualification_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotQualificationAttemptClaim::OccurrenceBound(_)
        ));

        let mut verifier = RestoredVerifierAttemptIntent {
            schema: 2,
            operation_id: String::new(),
            qualification_operation_id: intent.operation_id.clone(),
            restored_occurrence_id: "sbx-restored".into(),
            verifier_artifact_hash: intent.verifier_artifact_hash.clone(),
            upload_sha256: "4".repeat(64),
            upload_bytes: 1024,
            purpose: ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose::OwnerMeasurement { challenge: RestoredOwnerChallenge {
                schema: 1,
                protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
                operation_id: source.operation_id.clone(),
                snapshot_id: locator.snapshot_id.clone(),
                restored_occurrence_id: "sbx-restored".into(),
                nonce_hex: "5".repeat(64),
            } },
            attempt_deadline_ms: now + 60_000,
        };
        verifier.operation_id = verifier.derived_operation_id().unwrap();
        db.reserve_restored_verifier_attempt(&verifier).unwrap();
        assert!(matches!(
            db.claim_restored_verifier_attempt(&verifier.operation_id)
                .unwrap(),
            super::super::restored_verifier_attempt::RestoredVerifierAttemptClaim::StartAttempt(_)
        ));
        assert!(matches!(
            db.claim_restored_verifier_attempt(&verifier.operation_id)
                .unwrap(),
            super::super::restored_verifier_attempt::RestoredVerifierAttemptClaim::Reconcile(_)
        ));
        db.quarantine_restored_verifier_attempt(&verifier.operation_id)
            .unwrap();
        assert!(matches!(
            db.claim_restored_verifier_attempt(&verifier.operation_id)
                .unwrap(),
            super::super::restored_verifier_attempt::RestoredVerifierAttemptClaim::Reconcile(_)
        ));
        let mut reminted = verifier.clone();
        let ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose::OwnerMeasurement { challenge } = &mut reminted.purpose else { unreachable!() };
        challenge.nonce_hex = "6".repeat(64);
        assert_eq!(
            reminted.derived_operation_id().unwrap(),
            verifier.operation_id
        );
        assert!(db.reserve_restored_verifier_attempt(&reminted).is_err());
        let observation = RestoredVerifierAdapterObservation {
            schema: 1,
            operation_id: verifier.operation_id.clone(),
            occurrence_id: verifier.restored_occurrence_id.clone(),
            upload_token_execution_id: "exe-upload".into(),
            run_token_execution_id: "exe-run".into(),
            upload_response_sha256: "7".repeat(64),
            run_stream_sha256: "8".repeat(64),
            measurement: RestoredOwnerMeasurement {
                schema: 1,
                protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
                challenge_digest: verifier.owner_challenge().unwrap().digest().unwrap(),
                manifest_hash: source.guest_runtime_manifest_hash.clone(),
                owner_executable_sha256: source.owner_executable_sha256.clone(),
                controller_public_root: source.controller_public_root.clone(),
            },
            contact_deadline_exceeded: false,
        };
        db.bind_restored_verifier_observation(&observation).unwrap();
        // A verifier reservation is not permission to contact after a
        // termination intent is retained, even if its own deadline is live.
        let mut racing_qualification = intent.clone();
        racing_qualification.qualification_profile_digest = "6".repeat(64);
        racing_qualification.operation_id = racing_qualification.derived_operation_id().unwrap();
        db.reserve_snapshot_qualification(&racing_qualification)
            .unwrap();
        db.claim_snapshot_qualification_attempt(&racing_qualification.operation_id)
            .unwrap();
        let mut racing_occurrence = occurrence.clone();
        racing_occurrence.operation_id = racing_qualification.operation_id.clone();
        racing_occurrence.occurrence_id = "sbx-termination-race".into();
        db.bind_snapshot_qualification_occurrence(&racing_occurrence)
            .unwrap();
        let mut racing_verifier = verifier.clone();
        racing_verifier.qualification_operation_id = racing_qualification.operation_id.clone();
        racing_verifier.restored_occurrence_id = racing_occurrence.occurrence_id.clone();
        let ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose::OwnerMeasurement { challenge } = &mut racing_verifier.purpose else {
            panic!("fixture requires owner measurement");
        };
        challenge.restored_occurrence_id = racing_occurrence.occurrence_id.clone();
        racing_verifier.operation_id = racing_verifier.derived_operation_id().unwrap();
        db.reserve_restored_verifier_attempt(&racing_verifier)
            .unwrap();
        let mut racing_termination = RuntimeSnapshotQualificationTerminationIntent {
            schema: 1,
            operation_id: String::new(),
            qualification_operation_id: racing_qualification.operation_id.clone(),
            occurrence_id: racing_occurrence.occurrence_id.clone(),
            owner_principal: racing_qualification.owner_principal.clone(),
            provider_id: racing_qualification.provider_id.clone(),
            provider_spec_digest: racing_qualification.provider_spec_digest.clone(),
            attempt_deadline_ms: now + 60_000,
        };
        racing_termination.operation_id = racing_termination.derived_operation_id().unwrap();
        db.reserve_qualification_termination(&racing_termination)
            .unwrap();
        let refused = db
            .claim_restored_verifier_attempt(&racing_verifier.operation_id)
            .unwrap_err();
        assert!(refused.to_string().contains("retained termination intent"));
        assert_eq!(
            db.restored_verifier_attempt(&racing_verifier.operation_id)
                .unwrap()
                .unwrap()
                .phase,
            super::super::restored_verifier_attempt::RestoredVerifierAttemptPhase::Reserved
        );
        let mut termination = RuntimeSnapshotQualificationTerminationIntent {
            schema: 1,
            operation_id: String::new(),
            qualification_operation_id: intent.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            owner_principal: intent.owner_principal.clone(),
            provider_id: intent.provider_id.clone(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
            attempt_deadline_ms: now + 60_000,
        };
        termination.operation_id = termination.derived_operation_id().unwrap();
        db.reserve_qualification_termination(&termination).unwrap();
        assert!(matches!(
            db.claim_qualification_termination_attempt(&termination.operation_id).unwrap(),
            super::super::runtime_snapshot_qualification_termination::QualificationTerminationClaim::StartAttempt(_)
        ));
        assert!(matches!(
            db.claim_qualification_termination_attempt(&termination.operation_id).unwrap(),
            super::super::runtime_snapshot_qualification_termination::QualificationTerminationClaim::Reconcile(_)
        ));
        db.quarantine_qualification_termination_attempt(&termination.operation_id)
            .unwrap();
        let mut changed_deadline = termination.clone();
        changed_deadline.attempt_deadline_ms += 1;
        assert_eq!(
            changed_deadline.derived_operation_id().unwrap(),
            termination.operation_id
        );
        assert!(
            db.reserve_qualification_termination(&changed_deadline)
                .is_err()
        );
        let terminal = RuntimeSnapshotQualificationTerminalObservation {
            schema: 1,
            operation_id: termination.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            provider_response_sha256: "a".repeat(64),
            terminated_at: "2026-09-28T00:02:00Z".into(),
            contact_deadline_exceeded: false,
        };
        db.bind_qualification_terminal_observation(&terminal)
            .unwrap();
        assert!(matches!(
            db.claim_qualification_termination_attempt(&termination.operation_id).unwrap(),
            super::super::runtime_snapshot_qualification_termination::QualificationTerminationClaim::Terminal(_)
        ));
        let mut changed_terminal = terminal;
        changed_terminal.provider_response_sha256 = "b".repeat(64);
        assert!(
            db.bind_qualification_terminal_observation(&changed_terminal)
                .is_err()
        );
        assert!(db.conn.execute(
            "UPDATE runtime_snapshot_qualification_termination SET phase='attempt_pending',observation_json=NULL WHERE operation_id=?1",
            [&termination.operation_id],
        ).is_err());
        let mut changed = occurrence.clone();
        changed.occurrence_id = "sbx-other".into();
        assert!(db.bind_snapshot_qualification_occurrence(&changed).is_err());
        // This checks retained evidence content only. The occurrence above has
        // already been terminated: this helper must not authorize new contact.
        let coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-consumer-fixture".into(),
            accepted_capsule_hash: "a".repeat(64),
            qualification_purpose_digest: "b".repeat(64),
            scenario_id: "codex-runtime".into(),
            scenario_source_digest: "c".repeat(64),
            subject_digest: "d".repeat(64),
            use_digest: "e".repeat(64),
            prerequisite_measurement_attempt_id: verifier.operation_id.clone(),
            prerequisite_measurement_observation_digest: hex::encode(Sha256::digest(
                ryeos_external_execution_contract::canonical_json(&observation).unwrap(),
            )),
        };
        // Consumer validation is a data join, not contact permission. This
        // occurrence is already terminated; journal admission still refuses.
        let selection = ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerifierSelection {
            scenario_source_digest: coordinate.scenario_source_digest.clone(),
            verifier_artifact_hash: "9".repeat(64),
        };
        let mut consumer = verifier.clone();
        consumer.verifier_artifact_hash = selection.verifier_artifact_hash.clone();
        consumer.purpose = ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose::ConsumerRuntime {
            coordinate: coordinate.clone(), nonce_hex: "8".repeat(64),
        };
        consumer.operation_id = consumer.derived_operation_id().unwrap();
        assert!(
            db.reserve_restored_verifier_attempt(&consumer)
                .unwrap_err()
                .to_string()
                .contains("matching admission lane")
        );
        // A null owner selection is immutable, not an empty slot that may
        // later be converted into consumer placement authority.
        assert!(db.conn.execute(
            "UPDATE restored_verifier_attempt SET consumer_selection_json=?2 WHERE operation_id=?1",
            params![verifier.operation_id, serde_json::to_string(&selection).unwrap()],
        ).is_err());
        consumer
            .validate_consumer_for(
                &source,
                &locator,
                &intent,
                &occurrence,
                &selection,
                &coordinate,
            )
            .unwrap();
        assert!(
            consumer
                .validate_for(&source, &locator, &intent, &occurrence)
                .is_err()
        );
        assert!(
            verifier
                .validate_consumer_for(
                    &source,
                    &locator,
                    &intent,
                    &occurrence,
                    &selection,
                    &coordinate
                )
                .is_err()
        );
        let mut wrong_selection = selection.clone();
        wrong_selection.verifier_artifact_hash = "a".repeat(64);
        assert!(
            consumer
                .validate_consumer_for(
                    &source,
                    &locator,
                    &intent,
                    &occurrence,
                    &wrong_selection,
                    &coordinate
                )
                .is_err()
        );
        let mut wrong_coordinate = coordinate.clone();
        wrong_coordinate.accepted_capsule_hash = "f".repeat(64);
        assert!(
            consumer
                .validate_consumer_for(
                    &source,
                    &locator,
                    &intent,
                    &occurrence,
                    &selection,
                    &wrong_coordinate
                )
                .is_err()
        );
        let mut reminted_consumer = consumer.clone();
        reminted_consumer.attempt_deadline_ms += 1;
        reminted_consumer.upload_sha256 = "7".repeat(64);
        assert_eq!(
            reminted_consumer.derived_operation_id().unwrap(),
            consumer.operation_id
        );
        assert_ne!(reminted_consumer, consumer);
        let check_prerequisite =
            |observed: &RestoredVerifierAdapterObservation,
             proposed: &ConsumerRuntimeVerificationCoordinate| {
                observed.validate_consumer_prerequisite(
                    &verifier,
                    proposed,
                    &source,
                    &locator,
                    &readiness,
                    &intent,
                    &occurrence,
                )
            };
        check_prerequisite(&observation, &coordinate).unwrap();
        // Unlike the content-only validator, the journal-owned read refuses
        // this occurrence because termination has already been retained.
        let refusal = db
            .observed_consumer_verification_prerequisite(
                &intent.operation_id,
                &intent.owner_principal,
                &coordinate,
            )
            .unwrap_err();
        assert!(refusal.to_string().contains("retained termination intent"));
        let mut wrong_attempt = coordinate.clone();
        wrong_attempt.prerequisite_measurement_attempt_id = "f".repeat(64);
        assert!(check_prerequisite(&observation, &wrong_attempt).is_err());
        let mut wrong_digest = coordinate.clone();
        wrong_digest.prerequisite_measurement_observation_digest = "f".repeat(64);
        assert!(check_prerequisite(&observation, &wrong_digest).is_err());
        let mut altered_observation = observation.clone();
        altered_observation.run_stream_sha256 = "f".repeat(64);
        assert!(check_prerequisite(&altered_observation, &coordinate).is_err());
        let mut late_observation = observation.clone();
        late_observation.contact_deadline_exceeded = true;
        let mut late_coordinate = coordinate.clone();
        late_coordinate.prerequisite_measurement_observation_digest = hex::encode(Sha256::digest(
            ryeos_external_execution_contract::canonical_json(&late_observation).unwrap(),
        ));
        assert!(check_prerequisite(&late_observation, &late_coordinate).is_err());
        assert!(matches!(
            db.claim_restored_verifier_attempt(&verifier.operation_id)
                .unwrap(),
            super::super::restored_verifier_attempt::RestoredVerifierAttemptClaim::Observed(_)
        ));
        let mut changed_observation = observation;
        changed_observation.run_stream_sha256 = "9".repeat(64);
        assert!(
            db.bind_restored_verifier_observation(&changed_observation)
                .is_err()
        );
        assert!(
            db.conn.execute(
                "UPDATE restored_verifier_attempt SET phase='attempt_pending',observation_json=NULL WHERE operation_id=?1",
                [&verifier.operation_id],
            ).is_err()
        );
    }
}
