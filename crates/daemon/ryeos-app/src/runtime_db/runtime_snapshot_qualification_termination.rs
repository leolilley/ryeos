//! One-shot termination journal for a restored qualification Sandbox.
//! A pending POST is never replayed; reconciliation may only observe the
//! retained occurrence. Provider terminal status is not guest writer proof.

use super::*;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::runtime_snapshot::{
    RuntimeSnapshotQualificationTerminalObservation, RuntimeSnapshotQualificationTerminationIntent,
};

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE runtime_snapshot_qualification_termination (
    operation_id TEXT PRIMARY KEY,
    qualification_operation_id TEXT NOT NULL REFERENCES runtime_snapshot_qualification(operation_id),
    intent_json TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined','terminal')),
    observation_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((phase='terminal') = (observation_json IS NOT NULL))
);
CREATE TRIGGER runtime_snapshot_qualification_termination_immutable_intent
BEFORE UPDATE ON runtime_snapshot_qualification_termination
WHEN NEW.operation_id!=OLD.operation_id
    OR NEW.qualification_operation_id!=OLD.qualification_operation_id
    OR NEW.intent_json!=OLD.intent_json
    OR NEW.created_at_ms!=OLD.created_at_ms
BEGIN SELECT RAISE(ABORT, 'qualification termination intent is immutable'); END;
CREATE TRIGGER runtime_snapshot_qualification_termination_no_delete
BEFORE DELETE ON runtime_snapshot_qualification_termination
BEGIN SELECT RAISE(ABORT, 'qualification termination is retained'); END;
CREATE TRIGGER runtime_snapshot_qualification_termination_immutable_observation
BEFORE UPDATE ON runtime_snapshot_qualification_termination
WHEN OLD.observation_json IS NOT NULL AND NEW.observation_json IS NOT OLD.observation_json
BEGIN SELECT RAISE(ABORT, 'qualification terminal observation is immutable'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationTerminationPhase {
    Reserved,
    AttemptPending,
    Quarantined,
    Terminal,
}

impl QualificationTerminationPhase {
    fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            "terminal" => Self::Terminal,
            _ => anyhow::bail!("qualification termination has an invalid phase"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QualificationTerminationRecord {
    pub intent: RuntimeSnapshotQualificationTerminationIntent,
    pub phase: QualificationTerminationPhase,
    pub observation: Option<RuntimeSnapshotQualificationTerminalObservation>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualificationTerminationClaim {
    StartAttempt(QualificationTerminationRecord),
    Reconcile(QualificationTerminationRecord),
    Terminal(QualificationTerminationRecord),
}

fn canonical<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(
        ryeos_external_execution_contract::canonical_json(value)?,
    )?)
}

pub(super) fn read(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<QualificationTerminationRecord>> {
    let row: Option<(String, String, String, Option<String>, i64, i64)> = conn
        .query_row(
            "SELECT qualification_operation_id,intent_json,phase,observation_json,created_at_ms,updated_at_ms
             FROM runtime_snapshot_qualification_termination WHERE operation_id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .optional()?;
    let Some((qualification_id, intent_json, phase, observation_json, created, updated)) = row
    else {
        return Ok(None);
    };
    ensure!(
        intent_json.len() <= 2048,
        "qualification termination intent exceeds bound"
    );
    let intent: RuntimeSnapshotQualificationTerminationIntent = serde_json::from_str(&intent_json)?;
    let qualification = super::runtime_snapshot_qualification::read(conn, &qualification_id)?
        .context("qualification termination source disappeared")?;
    let occurrence = qualification
        .occurrence
        .as_ref()
        .context("qualification termination source has no occurrence")?;
    intent.validate_for(&qualification.intent, occurrence)?;
    ensure!(
        intent.operation_id == operation_id
            && intent.qualification_operation_id == qualification_id
            && canonical(&intent)? == intent_json,
        "qualification termination retained intent changed identity"
    );
    let phase = QualificationTerminationPhase::parse(&phase)?;
    let observation = observation_json
        .map(|raw| -> Result<_> {
            ensure!(
                raw.len() <= 1024,
                "qualification terminal observation exceeds bound"
            );
            let value: RuntimeSnapshotQualificationTerminalObservation =
                serde_json::from_str(&raw)?;
            value.validate_for(&intent)?;
            ensure!(
                canonical(&value)? == raw,
                "qualification terminal observation is noncanonical"
            );
            Ok(value)
        })
        .transpose()?;
    ensure!(
        (phase == QualificationTerminationPhase::Terminal) == observation.is_some()
            && created > 0
            && updated >= created,
        "qualification termination phase contradicts retained evidence"
    );
    Ok(Some(QualificationTerminationRecord {
        intent,
        phase,
        observation,
        created_at_ms: created,
        updated_at_ms: updated,
    }))
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT operation_id FROM runtime_snapshot_qualification_termination ORDER BY operation_id",
    )?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        read(conn, &id)?.context("qualification termination row disappeared")?;
    }
    Ok(())
}

impl RuntimeDb {
    pub fn qualification_termination_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<QualificationTerminationRecord>> {
        read(&self.conn, operation_id)
    }

    pub fn reserve_qualification_termination(
        &self,
        intent: &RuntimeSnapshotQualificationTerminationIntent,
    ) -> Result<QualificationTerminationRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            ensure!(
                existing.intent == *intent,
                "qualification termination reservation replay changed"
            );
            tx.commit()?;
            return Ok(existing);
        }
        let qualification =
            super::runtime_snapshot_qualification::read(&tx, &intent.qualification_operation_id)?
                .context("qualification termination source was not retained")?;
        let occurrence = qualification
            .occurrence
            .as_ref()
            .context("qualification termination source has no occurrence")?;
        intent.validate_for(&qualification.intent, occurrence)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms.saturating_sub(now) <= 300_000,
            "qualification termination deadline is outside its admission window"
        );
        tx.execute(
            "INSERT INTO runtime_snapshot_qualification_termination
             (operation_id,qualification_operation_id,intent_json,phase,observation_json,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,'reserved',NULL,?4,?4)",
            params![intent.operation_id, intent.qualification_operation_id, canonical(intent)?, now],
        )?;
        let record = read(&tx, &intent.operation_id)?
            .context("qualification termination reservation vanished")?;
        tx.commit()?;
        Ok(record)
    }

    pub fn claim_qualification_termination_attempt(
        &self,
        operation_id: &str,
    ) -> Result<QualificationTerminationClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record =
            read(&tx, operation_id)?.context("qualification termination was not reserved")?;
        match record.phase {
            QualificationTerminationPhase::Terminal => {
                tx.commit()?;
                return Ok(QualificationTerminationClaim::Terminal(record));
            }
            QualificationTerminationPhase::AttemptPending
            | QualificationTerminationPhase::Quarantined => {
                tx.commit()?;
                return Ok(QualificationTerminationClaim::Reconcile(record));
            }
            QualificationTerminationPhase::Reserved => {}
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < record.intent.attempt_deadline_ms,
            "qualification termination deadline expired"
        );
        let changed = tx.execute(
            "UPDATE runtime_snapshot_qualification_termination SET phase='attempt_pending',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='reserved'",
            params![operation_id, now],
        )?;
        ensure!(
            changed == 1,
            "qualification termination attempt lost durable CAS"
        );
        let current =
            read(&tx, operation_id)?.context("qualification termination attempt vanished")?;
        tx.commit()?;
        Ok(QualificationTerminationClaim::StartAttempt(current))
    }

    pub fn quarantine_qualification_termination_attempt(
        &self,
        operation_id: &str,
    ) -> Result<QualificationTerminationRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record =
            read(&tx, operation_id)?.context("qualification termination attempt is absent")?;
        if record.phase == QualificationTerminationPhase::Quarantined {
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            record.phase == QualificationTerminationPhase::AttemptPending,
            "qualification termination quarantine requires pending attempt"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "UPDATE runtime_snapshot_qualification_termination SET phase='quarantined',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='attempt_pending'",
            params![operation_id, now],
        )?;
        let current =
            read(&tx, operation_id)?.context("qualification termination quarantine vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn bind_qualification_terminal_observation(
        &self,
        observation: &RuntimeSnapshotQualificationTerminalObservation,
    ) -> Result<QualificationTerminationRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &observation.operation_id)?
            .context("qualification termination attempt is absent")?;
        observation.validate_for(&record.intent)?;
        if let Some(existing) = record.observation.as_ref() {
            ensure!(
                existing == observation,
                "qualification terminal observation changed on replay"
            );
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            matches!(
                record.phase,
                QualificationTerminationPhase::AttemptPending
                    | QualificationTerminationPhase::Quarantined
            ),
            "qualification termination had no claimed provider contact"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "UPDATE runtime_snapshot_qualification_termination SET phase='terminal',observation_json=?2,updated_at_ms=?3
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![observation.operation_id, canonical(observation)?, now],
        )?;
        let current = read(&tx, &observation.operation_id)?
            .context("qualification terminal observation vanished")?;
        tx.commit()?;
        Ok(current)
    }
}
