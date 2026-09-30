//! Durable one-contact authority for the independently admitted restored
//! verifier. A pending or uncertain upload/run may not be replayed.

use super::*;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeVerificationCoordinate, RestoredVerifierAdapterObservation,
    RestoredVerifierAttemptIntent,
};

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE restored_verifier_attempt (
    operation_id TEXT PRIMARY KEY,
    qualification_operation_id TEXT NOT NULL REFERENCES runtime_snapshot_qualification(operation_id),
    intent_json TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined','observed')),
    observation_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((phase='observed') = (observation_json IS NOT NULL))
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
CREATE TRIGGER restored_verifier_attempt_immutable_observation
BEFORE UPDATE ON restored_verifier_attempt
WHEN OLD.observation_json IS NOT NULL AND NEW.observation_json IS NOT OLD.observation_json
BEGIN SELECT RAISE(ABORT, 'restored verifier observation is immutable'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoredVerifierAttemptPhase {
    Reserved,
    AttemptPending,
    Quarantined,
    Observed,
}

impl RestoredVerifierAttemptPhase {
    fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            "observed" => Self::Observed,
            _ => anyhow::bail!("restored verifier has an invalid attempt phase"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoredVerifierAttemptRecord {
    pub intent: RestoredVerifierAttemptIntent,
    pub phase: RestoredVerifierAttemptPhase,
    pub observation: Option<RestoredVerifierAdapterObservation>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoredVerifierAttemptClaim {
    StartAttempt(RestoredVerifierAttemptRecord),
    Reconcile(RestoredVerifierAttemptRecord),
    Observed(RestoredVerifierAttemptRecord),
}

fn canonical<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(
        ryeos_external_execution_contract::canonical_json(value)?,
    )?)
}

fn read(conn: &Connection, operation_id: &str) -> Result<Option<RestoredVerifierAttemptRecord>> {
    let row: Option<(String, String, String, Option<String>, i64, i64)> = conn
        .query_row(
            "SELECT qualification_operation_id,intent_json,phase,observation_json,created_at_ms,updated_at_ms
             FROM restored_verifier_attempt WHERE operation_id=?1",
            [operation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((
        qualification_id,
        intent_json,
        phase,
        observation_json,
        created_at_ms,
        updated_at_ms,
    )) = row
    else {
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
    let readiness = source
        .readiness
        .as_ref()
        .context("restored verifier snapshot is not ready")?;
    intent.validate_for(&source.intent, locator, &qualification.intent, occurrence)?;
    ensure!(
        intent.operation_id == operation_id
            && intent.qualification_operation_id == qualification_id
            && canonical(&intent)? == intent_json
            && created_at_ms > 0
            && updated_at_ms >= created_at_ms,
        "restored verifier retained attempt changed identity"
    );
    let phase = RestoredVerifierAttemptPhase::parse(&phase)?;
    let observation = observation_json
        .map(|raw| -> Result<_> {
            ensure!(
                raw.len() <= 8192,
                "restored verifier observation exceeds bound"
            );
            let value: RestoredVerifierAdapterObservation = serde_json::from_str(&raw)?;
            value.validate_for_retained(
                &intent,
                &source.intent,
                locator,
                readiness,
                &qualification.intent,
                occurrence,
            )?;
            ensure!(
                canonical(&value)? == raw,
                "restored verifier observation is noncanonical"
            );
            Ok(value)
        })
        .transpose()?;
    ensure!(
        (phase == RestoredVerifierAttemptPhase::Observed) == observation.is_some(),
        "restored verifier phase contradicts retained observation"
    );
    Ok(Some(RestoredVerifierAttemptRecord {
        intent,
        phase,
        observation,
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
    /// Load authenticated prerequisite evidence without granting contact.
    /// The consumer reservation/claim must repeat these checks in its own
    /// transaction; this read cannot exclude a later termination. Accepted-root,
    /// signed scenario and current provider authority remain operator checks.
    pub fn observed_consumer_verification_prerequisite(
        &self,
        qualification_operation_id: &str,
        owner_principal: &str,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
    ) -> Result<RestoredVerifierAttemptRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &coordinate.prerequisite_measurement_attempt_id)?
            .context("consumer verification prerequisite is absent")?;
        ensure!(
            record.phase == RestoredVerifierAttemptPhase::Observed
                && record.intent.qualification_operation_id == qualification_operation_id,
            "consumer verification requires an observed same-occurrence prerequisite"
        );
        let qualification =
            super::runtime_snapshot_qualification::read(&tx, qualification_operation_id)?
                .context("consumer verification qualification is absent")?;
        ensure!(
            qualification.intent.owner_principal == owner_principal,
            "consumer verification prerequisite belongs to another operator"
        );
        let termination_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_snapshot_qualification_termination
             WHERE qualification_operation_id=?1)",
            [qualification_operation_id],
            |row| row.get(0),
        )?;
        ensure!(
            !termination_exists,
            "consumer verification occurrence has a retained termination intent"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        // Reservation predates provider creation, so this is a conservative
        // upper bound, not a fresh lease minted from observation time.
        let expires_at = qualification
            .created_at_ms
            .checked_add(i64::from(qualification.intent.maximum_lifetime_seconds) * 1000)
            .context("consumer verification occurrence lifetime overflow")?;
        ensure!(
            now >= qualification.created_at_ms && now < expires_at,
            "consumer verification occurrence is outside its retained lifetime"
        );
        let occurrence = qualification
            .occurrence
            .as_ref()
            .context("consumer verification occurrence is absent")?;
        let source =
            super::runtime_snapshot::read(&tx, &qualification.intent.snapshot_operation_id)?
                .context("consumer verification snapshot is absent")?;
        record
            .observation
            .as_ref()
            .context("consumer prerequisite observation is absent")?
            .validate_consumer_prerequisite(
                &record.intent,
                coordinate,
                &source.intent,
                source
                    .locator
                    .as_ref()
                    .context("consumer snapshot locator is absent")?,
                source
                    .readiness
                    .as_ref()
                    .context("consumer snapshot readiness is absent")?,
                &qualification.intent,
                occurrence,
            )?;
        tx.commit()?;
        Ok(record)
    }

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
             (operation_id,qualification_operation_id,intent_json,phase,observation_json,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,'reserved',NULL,?4,?4)",
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
        if record.phase == RestoredVerifierAttemptPhase::Observed {
            tx.commit()?;
            return Ok(RestoredVerifierAttemptClaim::Observed(record));
        }
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

    pub fn bind_restored_verifier_observation(
        &self,
        observation: &RestoredVerifierAdapterObservation,
    ) -> Result<RestoredVerifierAttemptRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &observation.operation_id)?
            .context("restored verifier observation has no claimed attempt")?;
        if record.phase == RestoredVerifierAttemptPhase::Observed {
            ensure!(
                record.observation.as_ref() == Some(observation),
                "restored verifier observation replay changed"
            );
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            matches!(
                record.phase,
                RestoredVerifierAttemptPhase::AttemptPending
                    | RestoredVerifierAttemptPhase::Quarantined
            ),
            "restored verifier observation did not follow a contact claim"
        );
        let qualification = super::runtime_snapshot_qualification::read(
            &tx,
            &record.intent.qualification_operation_id,
        )?
        .context("restored verifier qualification disappeared")?;
        let occurrence = qualification
            .occurrence
            .as_ref()
            .context("restored occurrence disappeared")?;
        let source =
            super::runtime_snapshot::read(&tx, &qualification.intent.snapshot_operation_id)?
                .context("restored verifier source disappeared")?;
        observation.validate_for_retained(
            &record.intent,
            &source.intent,
            source
                .locator
                .as_ref()
                .context("restored verifier locator disappeared")?,
            source
                .readiness
                .as_ref()
                .context("restored verifier readiness disappeared")?,
            &qualification.intent,
            occurrence,
        )?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let changed = tx.execute(
            "UPDATE restored_verifier_attempt SET phase='observed',observation_json=?2,updated_at_ms=?3
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![observation.operation_id, canonical(observation)?, now],
        )?;
        ensure!(
            changed == 1,
            "restored verifier observation bind lost durable CAS"
        );
        let current = read(&tx, &observation.operation_id)?
            .context("restored verifier observation vanished")?;
        tx.commit()?;
        Ok(current)
    }
}
