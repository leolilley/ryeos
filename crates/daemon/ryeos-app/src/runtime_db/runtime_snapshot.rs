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
    source_bootstrap_operation_id TEXT UNIQUE REFERENCES runtime_snapshot_bootstrap(operation_id),
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined','late_observed','bound')),
    locator_json TEXT,
    readiness_json TEXT,
    completion_at_ms INTEGER,
    runner_deadline_exceeded INTEGER CHECK (runner_deadline_exceeded IN (0,1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((phase IN ('bound','late_observed') AND locator_json IS NOT NULL
            AND completion_at_ms IS NOT NULL AND runner_deadline_exceeded IS NOT NULL)
        OR (phase NOT IN ('bound','late_observed') AND locator_json IS NULL
            AND readiness_json IS NULL AND completion_at_ms IS NULL
            AND runner_deadline_exceeded IS NULL)),
    CHECK (readiness_json IS NULL OR phase='bound')
);
CREATE TRIGGER runtime_snapshot_operation_immutable_intent
BEFORE UPDATE ON runtime_snapshot_operation
WHEN NEW.operation_id!=OLD.operation_id OR NEW.intent_json!=OLD.intent_json
    OR NEW.intent_digest!=OLD.intent_digest OR NEW.created_at_ms!=OLD.created_at_ms
    OR NEW.source_bootstrap_operation_id IS NOT OLD.source_bootstrap_operation_id
    OR (OLD.completion_at_ms IS NOT NULL AND NEW.completion_at_ms IS NOT OLD.completion_at_ms)
    OR (OLD.runner_deadline_exceeded IS NOT NULL AND NEW.runner_deadline_exceeded IS NOT OLD.runner_deadline_exceeded)
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
    LateObserved,
    Bound,
}

impl RuntimeSnapshotPhase {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            "late_observed" => Self::LateObserved,
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
    pub completion_at_ms: Option<i64>,
    pub runner_deadline_exceeded: Option<bool>,
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

fn verify_retained_bootstrap_source(
    conn: &Connection,
    intent: &RuntimeSnapshotIntent,
) -> Result<()> {
    let Some(operation_id) = &intent.source_bootstrap_operation_id else {
        return Ok(());
    };
    let bootstrap = super::runtime_snapshot_bootstrap::read(conn, operation_id)?
        .context("materialized snapshot lost retained bootstrap source")?;
    ensure!(
        bootstrap.phase
            == super::runtime_snapshot_bootstrap::SnapshotBootstrapPhase::OccurrenceBound
            && bootstrap.readiness.is_some()
            && bootstrap
                .occurrence
                .as_ref()
                .map(|value| value.occurrence_id.as_str())
                == Some(intent.source_occurrence_id.as_str())
            && bootstrap
                .occurrence
                .as_ref()
                .and_then(|value| value.provider_creation_observation["created_at"].as_str())
                == intent.source_created_at.as_deref()
            && bootstrap
                .occurrence
                .as_ref()
                .and_then(|value| value.provider_creation_observation["timeout_seconds"].as_u64())
                == intent.source_timeout_seconds.map(u64::from)
            && bootstrap.intent.owner_principal == intent.owner_principal
            && bootstrap.intent.provider_id == intent.provider_id
            && bootstrap.intent.provider_group_id == intent.provider_group_id
            && bootstrap.intent.production_binding_digest == intent.production_profile_digest
            && bootstrap.intent.adapter_artifact_hash == intent.adapter_artifact_hash
            && bootstrap.intent.settings_digest == intent.settings_digest
            && bootstrap.intent.source == intent.source
            && intent
                .source_timeout_seconds
                .is_some_and(|seconds| { seconds <= bootstrap.intent.maximum_lifetime_seconds })
            && bootstrap.intent.guest_runtime_manifest_hash == intent.guest_runtime_manifest_hash,
        "materialized snapshot differs from exact retained bootstrap source"
    );
    Ok(())
}

pub(super) fn read(conn: &Connection, operation_id: &str) -> Result<Option<RuntimeSnapshotRecord>> {
    let raw: Option<(String, String, Option<String>, String, Option<String>, Option<String>, Option<i64>, Option<i64>, i64, i64)> = conn
        .query_row(
            "SELECT intent_json,intent_digest,source_bootstrap_operation_id,phase,locator_json,readiness_json,completion_at_ms,runner_deadline_exceeded,created_at_ms,updated_at_ms
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
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .optional()?;
    let Some((
        intent_json,
        intent_digest,
        source_bootstrap_operation_id,
        phase,
        locator_json,
        readiness_json,
        completion_at_ms,
        runner_deadline_exceeded,
        created,
        updated,
    )) = raw
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
            && intent.source_bootstrap_operation_id == source_bootstrap_operation_id
            && canonical(&intent)? == intent_json,
        "runtime snapshot retained intent changed identity"
    );
    verify_retained_bootstrap_source(conn, &intent)?;
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
        matches!(
            phase,
            RuntimeSnapshotPhase::Bound | RuntimeSnapshotPhase::LateObserved
        ) == locator.is_some()
            && (readiness.is_none() || phase == RuntimeSnapshotPhase::Bound)
            && matches!(runner_deadline_exceeded, None | Some(0) | Some(1))
            && (completion_at_ms.is_some() == runner_deadline_exceeded.is_some())
            && (completion_at_ms.is_some() == locator.is_some())
            && completion_at_ms
                .is_none_or(|completed| completed >= created && completed <= updated)
            && (phase != RuntimeSnapshotPhase::Bound
                || (runner_deadline_exceeded == Some(0)
                    && completion_at_ms
                        .is_some_and(|completed| completed < intent.attempt_deadline_ms)))
            && (phase != RuntimeSnapshotPhase::LateObserved
                || (runner_deadline_exceeded == Some(1)
                    || completion_at_ms
                        .is_some_and(|completed| completed >= intent.attempt_deadline_ms)))
            && created > 0
            && updated >= created,
        "runtime snapshot phase or timestamps contradict retained evidence"
    );
    Ok(Some(RuntimeSnapshotRecord {
        intent,
        phase,
        locator,
        readiness,
        completion_at_ms,
        runner_deadline_exceeded: runner_deadline_exceeded.map(|value| value != 0),
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
        let maximum_window_ms = if let Some(bootstrap_operation_id) =
            &intent.source_bootstrap_operation_id
        {
            verify_retained_bootstrap_source(&tx, intent)?;
            let bootstrap = super::runtime_snapshot_bootstrap::read(&tx, bootstrap_operation_id)?
                .context("materialized snapshot lost retained bootstrap")?;
            let termination_count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM runtime_snapshot_bootstrap_termination WHERE bootstrap_operation_id=?1",
                [bootstrap_operation_id], |row| row.get(0),
            )?;
            ensure!(
                termination_count == 0,
                "bootstrap source cleanup already reserved"
            );
            let created_ms = chrono::DateTime::parse_from_rfc3339(
                intent
                    .source_created_at
                    .as_deref()
                    .context("snapshot source has no creation time")?,
            )?
            .timestamp_millis();
            let source_expiry_ms = created_ms
                .checked_add(
                    i64::from(
                        intent
                            .source_timeout_seconds
                            .context("snapshot source has no timeout")?,
                    ) * 1_000,
                )
                .context("snapshot source expiry overflow")?;
            ensure!(
                intent.attempt_deadline_ms <= source_expiry_ms,
                "snapshot deadline outlives its retained source"
            );
            i64::from(bootstrap.intent.maximum_lifetime_seconds) * 1_000
        } else {
            300_000
        };
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms.saturating_sub(now) <= maximum_window_ms,
            "runtime snapshot attempt deadline is outside its admission window"
        );
        tx.execute(
            "INSERT INTO runtime_snapshot_operation
             (operation_id,intent_json,intent_digest,source_bootstrap_operation_id,phase,locator_json,readiness_json,completion_at_ms,runner_deadline_exceeded,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,?4,'reserved',NULL,NULL,NULL,NULL,?5,?5)",
            params![
                intent.operation_id,
                canonical(intent)?,
                intent.digest()?,
                intent.source_bootstrap_operation_id,
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
            RuntimeSnapshotPhase::AttemptPending
            | RuntimeSnapshotPhase::Quarantined
            | RuntimeSnapshotPhase::LateObserved => {
                tx.commit()?;
                return Ok(RuntimeSnapshotAttemptClaim::Reconcile(record));
            }
            RuntimeSnapshotPhase::Reserved => {}
        }
        let staged_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM runtime_snapshot_stage WHERE parent_operation_id=?1",
            [operation_id],
            |row| row.get(0),
        )?;
        ensure!(
            staged_count == 0,
            "runtime snapshot has a separately retained mutation stage"
        );
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

    /// A complete adapter observation retains one opaque snapshot locator.
    /// The runner's deadline classification is authoritative, and a delayed
    /// database bind conservatively remains late even when the adapter itself
    /// finished in time. Neither state establishes restored runtime bytes.
    pub fn bind_runtime_snapshot_locator(
        &self,
        locator: &RuntimeSnapshotLocator,
        deadline_exceeded: bool,
    ) -> Result<RuntimeSnapshotRecord> {
        let observed_at_ms = i64::try_from(lillux::time::timestamp_millis())?;
        self.bind_runtime_snapshot_locator_at(locator, deadline_exceeded, observed_at_ms)
    }

    fn bind_runtime_snapshot_locator_at(
        &self,
        locator: &RuntimeSnapshotLocator,
        deadline_exceeded: bool,
        observed_at_ms: i64,
    ) -> Result<RuntimeSnapshotRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record =
            read(&tx, &locator.operation_id)?.context("runtime snapshot has no attempt claim")?;
        locator.validate_for(&record.intent)?;
        if matches!(
            record.phase,
            RuntimeSnapshotPhase::Bound | RuntimeSnapshotPhase::LateObserved
        ) {
            ensure!(
                record.locator.as_ref() == Some(locator),
                "runtime snapshot result replay changed"
            );
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            observed_at_ms >= record.updated_at_ms,
            "runtime snapshot completion predates its retained attempt"
        );
        let late = deadline_exceeded || observed_at_ms >= record.intent.attempt_deadline_ms;
        ensure!(
            matches!(
                record.phase,
                RuntimeSnapshotPhase::AttemptPending | RuntimeSnapshotPhase::Quarantined
            ),
            "runtime snapshot locator did not follow its one attempt claim"
        );
        ensure!(
            late || record.phase == RuntimeSnapshotPhase::AttemptPending,
            "quarantined runtime snapshot cannot become a timely bound locator"
        );
        let target_phase = if late { "late_observed" } else { "bound" };
        let changed = tx.execute(
            "UPDATE runtime_snapshot_operation SET phase=?2,locator_json=?3,
             completion_at_ms=?4,runner_deadline_exceeded=?5,updated_at_ms=?4
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![
                locator.operation_id,
                target_phase,
                canonical(locator)?,
                observed_at_ms,
                i64::from(deadline_exceeded)
            ],
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
            source_bootstrap_operation_id: None,
            source_created_at: None,
            source_timeout_seconds: None,
            provider_group_id: "sbg-group".into(),
            production_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            provider_spec_digest: "d".repeat(64),
            settings_digest: "5".repeat(64),
            source: ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotSource::CapturedProduct {
                product_witness_hash: "6".repeat(64),
            },
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
            db.bind_runtime_snapshot_locator(&locator, false)
                .unwrap()
                .phase,
            RuntimeSnapshotPhase::Bound
        );
        let bound = db
            .runtime_snapshot_operation(&intent.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(bound.runner_deadline_exceeded, Some(false));
        assert!(
            bound
                .completion_at_ms
                .is_some_and(|at| at < intent.attempt_deadline_ms)
        );
        let replay = db
            .bind_runtime_snapshot_locator_at(&locator, true, intent.attempt_deadline_ms + 1)
            .unwrap();
        assert_eq!(replay, bound);
        assert!(matches!(
            db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
                .unwrap(),
            RuntimeSnapshotAttemptClaim::Bound(_)
        ));
        assert_eq!(
            db.bind_runtime_snapshot_locator(&locator, false)
                .unwrap()
                .locator,
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
        assert!(
            db.bind_runtime_snapshot_locator(&changed_locator, false)
                .is_err()
        );
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
        let observed = locator(&intent);
        assert!(db.bind_runtime_snapshot_locator(&observed, false).is_err());
        let late = db.bind_runtime_snapshot_locator(&observed, true).unwrap();
        assert_eq!(late.phase, RuntimeSnapshotPhase::LateObserved);
        assert_eq!(late.locator, Some(observed.clone()));
        assert_eq!(late.runner_deadline_exceeded, Some(true));
        assert!(matches!(
            db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
                .unwrap(),
            RuntimeSnapshotAttemptClaim::Reconcile(_)
        ));
        assert_eq!(
            db.bind_runtime_snapshot_locator(&observed, false).unwrap(),
            late
        );
        assert_eq!(
            db.bind_runtime_snapshot_locator(&observed, true)
                .unwrap()
                .phase,
            RuntimeSnapshotPhase::LateObserved
        );
        let mut changed = observed.clone();
        changed.snapshot_id = "snp-other".into();
        assert!(db.bind_runtime_snapshot_locator(&changed, true).is_err());
        let readiness = RuntimeSnapshotReadinessObservation {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            intent_digest: digest,
            snapshot_id: observed.snapshot_id,
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            creation_response_sha256: observed.provider_response_sha256,
            readiness_response_sha256: "b".repeat(64),
            captured_at: "2026-09-28T00:01:00Z".into(),
            size_bytes: 4096,
        };
        assert!(db.bind_runtime_snapshot_readiness(&readiness).is_err());
        validate_current(&db.conn).unwrap();
    }

    #[test]
    fn complete_response_after_deadline_retains_locator_without_promotion() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let intent = intent();
        db.reserve_runtime_snapshot(&intent).unwrap();
        let digest = intent.digest().unwrap();
        db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
            .unwrap();
        let locator = locator(&intent);
        let late = db.bind_runtime_snapshot_locator(&locator, true).unwrap();
        assert_eq!(late.phase, RuntimeSnapshotPhase::LateObserved);
        assert_eq!(late.locator, Some(locator.clone()));
        assert_eq!(
            db.bind_runtime_snapshot_locator(&locator, false).unwrap(),
            late
        );
        assert!(
            db.quarantine_runtime_snapshot_attempt(&intent.operation_id, &digest)
                .is_err()
        );
        validate_current(&db.conn).unwrap();
    }

    #[test]
    fn delayed_bind_is_late_even_if_runner_reported_timely_exit() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let intent = intent();
        db.reserve_runtime_snapshot(&intent).unwrap();
        let digest = intent.digest().unwrap();
        db.claim_runtime_snapshot_attempt(&intent.operation_id, &digest)
            .unwrap();
        let locator = locator(&intent);
        let record = db
            .bind_runtime_snapshot_locator_at(&locator, false, intent.attempt_deadline_ms)
            .unwrap();
        assert_eq!(record.phase, RuntimeSnapshotPhase::LateObserved);
        assert_eq!(record.locator, Some(locator));
        assert_eq!(record.runner_deadline_exceeded, Some(false));
        assert_eq!(record.completion_at_ms, Some(intent.attempt_deadline_ms));
        validate_current(&db.conn).unwrap();
    }
}
