//! Durable one-contact authority for the independently admitted restored
//! verifier. A pending or uncertain upload/run may not be replayed.

use super::*;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierAttemptIntent;

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE restored_verifier_attempt (
    operation_id TEXT PRIMARY KEY,
    qualification_operation_id TEXT NOT NULL REFERENCES runtime_snapshot_qualification(operation_id),
    intent_json TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE TRIGGER restored_verifier_attempt_immutable
BEFORE UPDATE ON restored_verifier_attempt
WHEN NEW.operation_id!=OLD.operation_id
    OR NEW.qualification_operation_id!=OLD.qualification_operation_id
    OR NEW.intent_json!=OLD.intent_json
    OR NEW.created_at_ms!=OLD.created_at_ms
BEGIN SELECT RAISE(ABORT, 'restored verifier attempt is immutable'); END;
CREATE TRIGGER restored_verifier_attempt_no_delete
BEFORE DELETE ON restored_verifier_attempt
BEGIN SELECT RAISE(ABORT, 'restored verifier attempt is retained'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoredVerifierAttemptPhase {
    Reserved,
    AttemptPending,
    Quarantined,
}

impl RestoredVerifierAttemptPhase {
    fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            _ => anyhow::bail!("restored verifier has an invalid attempt phase"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoredVerifierAttemptRecord {
    pub intent: RestoredVerifierAttemptIntent,
    pub phase: RestoredVerifierAttemptPhase,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoredVerifierAttemptClaim {
    StartAttempt(RestoredVerifierAttemptRecord),
    Reconcile(RestoredVerifierAttemptRecord),
}

fn canonical<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(
        ryeos_external_execution_contract::canonical_json(value)?,
    )?)
}

fn read(conn: &Connection, operation_id: &str) -> Result<Option<RestoredVerifierAttemptRecord>> {
    let row: Option<(String, String, String, i64, i64)> = conn
        .query_row(
            "SELECT qualification_operation_id,intent_json,phase,created_at_ms,updated_at_ms
             FROM restored_verifier_attempt WHERE operation_id=?1",
            [operation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((qualification_id, intent_json, phase, created_at_ms, updated_at_ms)) = row else {
        return Ok(None);
    };
    ensure!(
        intent_json.len() <= 6144,
        "restored verifier intent exceeds bound"
    );
    let intent: RestoredVerifierAttemptIntent = serde_json::from_str(&intent_json)?;
    let qualification = super::runtime_snapshot_qualification::read(conn, &qualification_id)?
        .context("restored verifier qualification disappeared")?;
    let occurrence = qualification
        .occurrence
        .as_ref()
        .context("restored verifier has no restored occurrence")?;
    let source = super::runtime_snapshot::read(conn, &qualification.intent.snapshot_operation_id)?
        .context("restored verifier snapshot disappeared")?;
    let locator = source
        .locator
        .as_ref()
        .context("restored verifier snapshot has no locator")?;
    ensure!(
        source.readiness.is_some(),
        "restored verifier snapshot is not ready"
    );
    intent.validate_for(&source.intent, locator, &qualification.intent, occurrence)?;
    ensure!(
        intent.operation_id == operation_id
            && intent.qualification_operation_id == qualification_id
            && canonical(&intent)? == intent_json
            && created_at_ms > 0
            && updated_at_ms >= created_at_ms,
        "restored verifier retained attempt changed identity"
    );
    Ok(Some(RestoredVerifierAttemptRecord {
        intent,
        phase: RestoredVerifierAttemptPhase::parse(&phase)?,
        created_at_ms,
        updated_at_ms,
    }))
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT operation_id FROM restored_verifier_attempt ORDER BY operation_id")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        read(conn, &id)?.context("restored verifier row disappeared")?;
    }
    Ok(())
}

impl RuntimeDb {
    pub fn restored_verifier_attempt(
        &self,
        operation_id: &str,
    ) -> Result<Option<RestoredVerifierAttemptRecord>> {
        read(&self.conn, operation_id)
    }

    pub fn reserve_restored_verifier_attempt(
        &self,
        intent: &RestoredVerifierAttemptIntent,
    ) -> Result<RestoredVerifierAttemptRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            ensure!(
                existing.intent == *intent,
                "restored verifier reservation replay changed"
            );
            tx.commit()?;
            return Ok(existing);
        }
        let qualification =
            super::runtime_snapshot_qualification::read(&tx, &intent.qualification_operation_id)?
                .context("restored verifier qualification was not retained")?;
        let occurrence = qualification
            .occurrence
            .as_ref()
            .context("restored verifier occurrence was not bound")?;
        let source =
            super::runtime_snapshot::read(&tx, &qualification.intent.snapshot_operation_id)?
                .context("restored verifier snapshot was not retained")?;
        let locator = source
            .locator
            .as_ref()
            .context("restored verifier snapshot has no locator")?;
        ensure!(
            source.readiness.is_some(),
            "restored verifier snapshot is not ready"
        );
        intent.validate_for(&source.intent, locator, &qualification.intent, occurrence)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms.saturating_sub(now) <= 300_000,
            "restored verifier deadline is outside admission window"
        );
        tx.execute(
            "INSERT INTO restored_verifier_attempt
             (operation_id,qualification_operation_id,intent_json,phase,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,'reserved',?4,?4)",
            params![
                intent.operation_id,
                intent.qualification_operation_id,
                canonical(intent)?,
                now
            ],
        )?;
        let record =
            read(&tx, &intent.operation_id)?.context("restored verifier reservation vanished")?;
        tx.commit()?;
        Ok(record)
    }

    pub fn claim_restored_verifier_attempt(
        &self,
        operation_id: &str,
    ) -> Result<RestoredVerifierAttemptClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("restored verifier was not reserved")?;
        if record.phase != RestoredVerifierAttemptPhase::Reserved {
            tx.commit()?;
            return Ok(RestoredVerifierAttemptClaim::Reconcile(record));
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < record.intent.attempt_deadline_ms,
            "restored verifier deadline expired"
        );
        let changed = tx.execute(
            "UPDATE restored_verifier_attempt SET phase='attempt_pending',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='reserved'",
            params![operation_id, now],
        )?;
        ensure!(changed == 1, "restored verifier attempt lost durable CAS");
        let current = read(&tx, operation_id)?.context("restored verifier claim vanished")?;
        tx.commit()?;
        Ok(RestoredVerifierAttemptClaim::StartAttempt(current))
    }

    pub fn quarantine_restored_verifier_attempt(
        &self,
        operation_id: &str,
    ) -> Result<RestoredVerifierAttemptRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("restored verifier attempt is absent")?;
        if record.phase == RestoredVerifierAttemptPhase::Quarantined {
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            record.phase == RestoredVerifierAttemptPhase::AttemptPending,
            "restored verifier quarantine requires pending attempt"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "UPDATE restored_verifier_attempt SET phase='quarantined',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='attempt_pending'",
            params![operation_id, now],
        )?;
        let current = read(&tx, operation_id)?.context("restored verifier quarantine vanished")?;
        tx.commit()?;
        Ok(current)
    }
}
