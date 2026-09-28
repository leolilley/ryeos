//! One-attempt journal for operator-owned external runtime snapshot production.
//!
//! This is separate from a worker allocation. A complete provider response
//! may bind a locator, but only independent restored-guest qualification can
//! make that locator eligible for an execution binding.

use super::*;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::runtime_snapshot::{
    RUNTIME_SNAPSHOT_READINESS_PROTOCOL, RuntimeSnapshotIntent, RuntimeSnapshotLocator,
    RuntimeSnapshotReadinessObservation, RuntimeSnapshotReadinessRequest,
};

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE runtime_snapshot_operation (
    operation_id TEXT PRIMARY KEY,
    intent_json TEXT NOT NULL,
    intent_digest TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined','bound')),
    locator_json TEXT,
    readiness_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((phase='bound' AND locator_json IS NOT NULL)
        OR (phase!='bound' AND locator_json IS NULL AND readiness_json IS NULL)),
    CHECK (readiness_json IS NULL OR phase='bound')
);
CREATE TRIGGER runtime_snapshot_operation_immutable_intent
BEFORE UPDATE ON runtime_snapshot_operation
WHEN NEW.operation_id!=OLD.operation_id OR NEW.intent_json!=OLD.intent_json
    OR NEW.intent_digest!=OLD.intent_digest OR NEW.created_at_ms!=OLD.created_at_ms
BEGIN SELECT RAISE(ABORT, 'runtime snapshot intent is immutable'); END;
CREATE TRIGGER runtime_snapshot_operation_no_delete
BEFORE DELETE ON runtime_snapshot_operation
BEGIN SELECT RAISE(ABORT, 'runtime snapshot operation is retained'); END;
CREATE TRIGGER runtime_snapshot_readiness_immutable
BEFORE UPDATE ON runtime_snapshot_operation
WHEN OLD.readiness_json IS NOT NULL AND NEW.readiness_json!=OLD.readiness_json
BEGIN SELECT RAISE(ABORT, 'runtime snapshot readiness is immutable'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSnapshotPhase {
    Reserved,
    AttemptPending,
    Quarantined,
    Bound,
}

impl RuntimeSnapshotPhase {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            "bound" => Self::Bound,
            _ => anyhow::bail!("runtime snapshot has an invalid phase"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeSnapshotRecord {
    pub intent: RuntimeSnapshotIntent,
    pub phase: RuntimeSnapshotPhase,
    pub locator: Option<RuntimeSnapshotLocator>,
    pub readiness: Option<RuntimeSnapshotReadinessObservation>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeSnapshotAttemptClaim {
    StartAttempt(RuntimeSnapshotRecord),
    Reconcile(RuntimeSnapshotRecord),
    Bound(RuntimeSnapshotRecord),
}

fn canonical<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(
        ryeos_external_execution_contract::canonical_json(value)?,
    )?)
}

pub(super) fn read(conn: &Connection, operation_id: &str) -> Result<Option<RuntimeSnapshotRecord>> {
    let raw: Option<(String, String, String, Option<String>, Option<String>, i64, i64)> = conn
        .query_row(
            "SELECT intent_json,intent_digest,phase,locator_json,readiness_json,created_at_ms,updated_at_ms
             FROM runtime_snapshot_operation WHERE operation_id=?1",
            [operation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((intent_json, intent_digest, phase, locator_json, readiness_json, created, updated)) =
        raw
    else {
        return Ok(None);
    };
    ensure!(
        intent_json.len() <= 16 * 1024,
        "runtime snapshot intent exceeds its bound"
    );
    let intent: RuntimeSnapshotIntent = serde_json::from_str(&intent_json)?;
    ensure!(
        intent.operation_id == operation_id
            && intent.digest()? == intent_digest
            && canonical(&intent)? == intent_json,
        "runtime snapshot retained intent changed identity"
    );
    let phase = RuntimeSnapshotPhase::parse(&phase)?;
    let locator = locator_json
        .map(|raw| {
            ensure!(
                raw.len() <= 16 * 1024,
                "runtime snapshot locator exceeds its bound"
            );
            let locator: RuntimeSnapshotLocator = serde_json::from_str(&raw)?;
            locator.validate_for(&intent)?;
            ensure!(
                canonical(&locator)? == raw,
                "runtime snapshot locator is noncanonical"
            );
            Ok(locator)
        })
        .transpose()?;
    let readiness = readiness_json
        .map(|raw| {
            ensure!(
                raw.len() <= 4096,
                "runtime snapshot readiness exceeds its bound"
            );
            let readiness: RuntimeSnapshotReadinessObservation = serde_json::from_str(&raw)?;
            let request = RuntimeSnapshotReadinessRequest {
                protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
                intent: intent.clone(),
                locator: locator
                    .clone()
                    .context("snapshot readiness has no locator")?,
                provider_spec_digest: intent.provider_spec_digest.clone(),
            };
            readiness.validate_for(&request)?;
            ensure!(
                canonical(&readiness)? == raw,
                "runtime snapshot readiness is noncanonical"
            );
            Ok(readiness)
        })
        .transpose()?;
    ensure!(
        (phase == RuntimeSnapshotPhase::Bound) == locator.is_some()
            && (readiness.is_none() || phase == RuntimeSnapshotPhase::Bound)
            && created > 0
            && updated >= created,
        "runtime snapshot phase or timestamps contradict retained evidence"
    );
    Ok(Some(RuntimeSnapshotRecord {
        intent,
        phase,
        locator,
        readiness,
        created_at_ms: created,
        updated_at_ms: updated,
    }))
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT operation_id FROM runtime_snapshot_operation ORDER BY operation_id")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        read(conn, &id)?.context("runtime snapshot row disappeared during validation")?;
    }
    Ok(())
}

impl RuntimeDb {
    pub fn runtime_snapshot_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<RuntimeSnapshotRecord>> {
        read(&self.conn, operation_id)
    }

    /// Exact replay returns the same row. No provider contact is implied.
    pub fn reserve_runtime_snapshot(
        &self,
        intent: &RuntimeSnapshotIntent,
    ) -> Result<RuntimeSnapshotRecord> {
        intent.validate()?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            ensure!(
                existing.intent == *intent,
                "runtime snapshot reservation replay changed"
            );
            tx.commit()?;
            return Ok(existing);
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms.saturating_sub(now) <= 300_000,
            "runtime snapshot attempt deadline is outside its admission window"
        );
        tx.execute(
            "INSERT INTO runtime_snapshot_operation
             (operation_id,intent_json,intent_digest,phase,locator_json,readiness_json,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,'reserved',NULL,NULL,?4,?4)",
            params![
                intent.operation_id,
                canonical(intent)?,
                intent.digest()?,
                now
            ],
        )?;
        let record = read(&tx, &intent.operation_id)?
            .context("runtime snapshot reservation was not retained")?;
        tx.commit()?;
        Ok(record)
    }

    /// Only StartAttempt may run the bounded provider sequence. The sequence
    /// can include several requests; pending and quarantined rows never grant
    /// a second attempt after restart or lost output.
    pub fn claim_runtime_snapshot_attempt(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<RuntimeSnapshotAttemptClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("runtime snapshot was not reserved")?;
        ensure!(
            record.intent.digest()? == intent_digest,
            "runtime snapshot attempt changed intent"
        );
        match record.phase {
            RuntimeSnapshotPhase::Bound => {
                tx.commit()?;
                return Ok(RuntimeSnapshotAttemptClaim::Bound(record));
            }
            RuntimeSnapshotPhase::AttemptPending | RuntimeSnapshotPhase::Quarantined => {
                tx.commit()?;
                return Ok(RuntimeSnapshotAttemptClaim::Reconcile(record));
            }
            RuntimeSnapshotPhase::Reserved => {}
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < record.intent.attempt_deadline_ms,
            "runtime snapshot attempt deadline expired"
        );
        let changed = tx.execute(
            "UPDATE runtime_snapshot_operation SET phase='attempt_pending',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='reserved'",
            params![operation_id, now],
        )?;
        ensure!(
            changed == 1,
            "runtime snapshot attempt claim lost its durable CAS"
        );
        let current =
            read(&tx, operation_id)?.context("runtime snapshot attempt claim vanished")?;
        tx.commit()?;
        Ok(RuntimeSnapshotAttemptClaim::StartAttempt(current))
    }

    /// A complete adapter observation may bind one opaque snapshot locator.
    /// It never establishes that the provider restored the expected bytes.
    pub fn bind_runtime_snapshot_locator(
        &self,
        locator: &RuntimeSnapshotLocator,
    ) -> Result<RuntimeSnapshotRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record =
            read(&tx, &locator.operation_id)?.context("runtime snapshot has no attempt claim")?;
        locator.validate_for(&record.intent)?;
        if record.phase == RuntimeSnapshotPhase::Bound {
            ensure!(
                record.locator.as_ref() == Some(locator),
                "runtime snapshot result replay changed"
            );
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            matches!(
                record.phase,
                RuntimeSnapshotPhase::AttemptPending | RuntimeSnapshotPhase::Quarantined
            ),
            "runtime snapshot locator did not follow its one attempt claim"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let changed = tx.execute(
            "UPDATE runtime_snapshot_operation SET phase='bound',locator_json=?2,updated_at_ms=?3
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![locator.operation_id, canonical(locator)?, now],
        )?;
        ensure!(
            changed == 1,
            "runtime snapshot locator bind lost its durable CAS"
        );
        let current =
            read(&tx, &locator.operation_id)?.context("runtime snapshot locator vanished")?;
        tx.commit()?;
        Ok(current)
    }

    /// Retain one complete provider-readiness observation for an already
    /// bound locator. This does not qualify the restored bytes or authorize
    /// placement. Equivalent repeat observations return the original row.
    pub fn bind_runtime_snapshot_readiness(
        &self,
        observation: &RuntimeSnapshotReadinessObservation,
    ) -> Result<RuntimeSnapshotRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &observation.operation_id)?
            .context("runtime snapshot readiness has no retained operation")?;
        ensure!(
            record.phase == RuntimeSnapshotPhase::Bound,
            "runtime snapshot is not bound"
        );
        let request = RuntimeSnapshotReadinessRequest {
            protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
            intent: record.intent.clone(),
            locator: record
                .locator
                .clone()
                .context("bound snapshot has no locator")?,
            provider_spec_digest: record.intent.provider_spec_digest.clone(),
        };
        observation.validate_for(&request)?;
        if let Some(existing) = &record.readiness {
            ensure!(
                existing == observation,
                "runtime snapshot readiness replay changed"
            );
            tx.commit()?;
            return Ok(record);
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let changed = tx.execute(
            "UPDATE runtime_snapshot_operation SET readiness_json=?2,updated_at_ms=?3
             WHERE operation_id=?1 AND phase='bound' AND readiness_json IS NULL",
            params![observation.operation_id, canonical(observation)?, now],
        )?;
        ensure!(
            changed == 1,
            "runtime snapshot readiness lost its durable CAS"
        );
        let current =
            read(&tx, &observation.operation_id)?.context("runtime snapshot readiness vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn quarantine_runtime_snapshot_attempt(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<RuntimeSnapshotRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("runtime snapshot attempt is absent")?;
        ensure!(
            record.intent.digest()? == intent_digest,
            "runtime snapshot quarantine changed intent"
        );
        if record.phase == RuntimeSnapshotPhase::Quarantined {
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            record.phase == RuntimeSnapshotPhase::AttemptPending,
            "runtime snapshot quarantine requires pending attempt"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "UPDATE runtime_snapshot_operation SET phase='quarantined',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='attempt_pending'",
            params![operation_id, now],
        )?;
        let current = read(&tx, operation_id)?.context("runtime snapshot quarantine vanished")?;
        tx.commit()?;
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::runtime_snapshot::{
        RUNTIME_SNAPSHOT_INTENT_SCHEMA, RUNTIME_SNAPSHOT_RESULT_SCHEMA,
    };

    fn intent() -> RuntimeSnapshotIntent {
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        let mut intent = RuntimeSnapshotIntent {
            schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "2".repeat(64)),
            provider_id: "render-sandbox-early-access".into(),
            source_occurrence_id: "sbx-source".into(),
            provider_group_id: "sbg-group".into(),
            production_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            provider_spec_digest: "d".repeat(64),
            settings_digest: "5".repeat(64),
            product_witness_hash: "6".repeat(64),
            guest_runtime_manifest_hash: "7".repeat(64),
            owner_executable_sha256: "8".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [3u8; 32])
            ),
            upload_sha256: "9".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: now + 60_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
    }

    fn locator(intent: &RuntimeSnapshotIntent) -> RuntimeSnapshotLocator {
        RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: "snp-snapshot".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: lillux::sha256_hex(br#"{"schema":1}"#),
        }
    }

    #[test]
    fn exact_reservation_claim_and_result_never_restart_attempt() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let intent = intent();
        assert_eq!(
            db.reserve_runtime_snapshot(&intent).unwrap().phase,
            RuntimeSnapshotPhase::Reserved
        );
        assert_eq!(db.reserve_runtime_snapshot(&intent).unwrap().intent, intent);
        let mut changed = intent.clone();
        changed.upload_sha256 = "a".repeat(64);
        assert!(db.reserve_runtime_snapshot(&changed).is_err());
        let digest = intent.digest().unwrap();
        assert!(
            db.claim_runtime_snapshot_attempt(&intent.operation_id, &"f".repeat(64))
                .is_err()
        );
        assert!(matches!(
            db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
                .unwrap(),
            RuntimeSnapshotAttemptClaim::StartAttempt(_)
        ));
        assert!(matches!(
            db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
                .unwrap(),
            RuntimeSnapshotAttemptClaim::Reconcile(_)
        ));
        let locator = locator(&intent);
        assert_eq!(
            db.bind_runtime_snapshot_locator(&locator).unwrap().phase,
            RuntimeSnapshotPhase::Bound
        );
        assert!(matches!(
            db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
                .unwrap(),
            RuntimeSnapshotAttemptClaim::Bound(_)
        ));
        assert_eq!(
            db.bind_runtime_snapshot_locator(&locator).unwrap().locator,
            Some(locator.clone())
        );
        let readiness = RuntimeSnapshotReadinessObservation {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            creation_response_sha256: locator.provider_response_sha256.clone(),
            readiness_response_sha256: "b".repeat(64),
            captured_at: "2026-09-28T00:01:00Z".into(),
            size_bytes: 4096,
        };
        assert_eq!(
            db.bind_runtime_snapshot_readiness(&readiness)
                .unwrap()
                .readiness,
            Some(readiness.clone())
        );
        assert_eq!(
            db.bind_runtime_snapshot_readiness(&readiness)
                .unwrap()
                .readiness,
            Some(readiness.clone())
        );
        let mut changed_readiness = readiness;
        changed_readiness.size_bytes += 1;
        assert!(
            db.bind_runtime_snapshot_readiness(&changed_readiness)
                .is_err()
        );
        let mut changed_locator = locator;
        changed_locator.snapshot_id = "snp-other".into();
        assert!(db.bind_runtime_snapshot_locator(&changed_locator).is_err());
        validate_current(&db.conn).unwrap();
    }

    #[test]
    fn uncertain_attempt_stays_non_replayable() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let intent = intent();
        db.reserve_runtime_snapshot(&intent).unwrap();
        let digest = intent.digest().unwrap();
        db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
            .unwrap();
        assert_eq!(
            db.quarantine_runtime_snapshot_attempt(&intent.operation_id, &digest)
                .unwrap()
                .phase,
            RuntimeSnapshotPhase::Quarantined
        );
        assert!(matches!(
            db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
                .unwrap(),
            RuntimeSnapshotAttemptClaim::Reconcile(_)
        ));
        let late = db.bind_runtime_snapshot_locator(&locator(&intent)).unwrap();
        assert_eq!(late.phase, RuntimeSnapshotPhase::Bound);
        assert!(matches!(
            db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
                .unwrap(),
            RuntimeSnapshotAttemptClaim::Bound(_)
        ));
    }
}
