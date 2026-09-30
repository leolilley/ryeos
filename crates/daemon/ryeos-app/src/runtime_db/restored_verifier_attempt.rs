//! Durable one-contact authority for the independently admitted restored
//! verifier. A pending or uncertain upload/run may not be replayed.

use super::*;
use crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeVerificationCoordinate, ConsumerRuntimeVerifierSelection,
    ConsumerVerifierAdapterObservation, RemoteVerificationPurpose,
    RestoredVerifierAdapterObservation, RestoredVerifierAttemptIntent, RestoredVerifierObservation,
};

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE restored_verifier_attempt (
    operation_id TEXT PRIMARY KEY,
    qualification_operation_id TEXT NOT NULL REFERENCES runtime_snapshot_qualification(operation_id),
    intent_json TEXT NOT NULL,
    consumer_selection_json TEXT,
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
    OR NEW.consumer_selection_json IS NOT OLD.consumer_selection_json
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
    pub consumer_selection: Option<ConsumerRuntimeVerifierSelection>,
    pub phase: RestoredVerifierAttemptPhase,
    pub observation: Option<RestoredVerifierObservation>,
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

fn admitted_consumer_selection(
    intent: &RestoredVerifierAttemptIntent,
    qualification: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationIntent,
    admission: Option<&AuthenticatedConsumerRoot>,
) -> Result<Option<ConsumerRuntimeVerifierSelection>> {
    match (&intent.purpose, admission) {
        (RemoteVerificationPurpose::OwnerMeasurement { .. }, None) => Ok(None),
        (RemoteVerificationPurpose::ConsumerRuntime { .. }, Some(admission)) => {
            admission.require_attempt(intent, qualification)?;
            Ok(Some(admission.selection().clone()))
        }
        _ => anyhow::bail!("verifier attempt requires its matching admission lane"),
    }
}

fn read(conn: &Connection, operation_id: &str) -> Result<Option<RestoredVerifierAttemptRecord>> {
    let row: Option<(String, String, Option<String>, String, Option<String>, i64, i64)> = conn
        .query_row(
            "SELECT qualification_operation_id,intent_json,consumer_selection_json,phase,observation_json,created_at_ms,updated_at_ms
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
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((
        qualification_id,
        intent_json,
        selection_json,
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
    let consumer_selection = selection_json
        .map(|raw| -> Result<ConsumerRuntimeVerifierSelection> {
            ensure!(raw.len() <= 1024, "consumer selection exceeds bound");
            let selection: ConsumerRuntimeVerifierSelection = serde_json::from_str(&raw)?;
            selection.validate()?;
            ensure!(
                canonical(&selection)? == raw,
                "consumer selection is noncanonical"
            );
            Ok(selection)
        })
        .transpose()?;
    match (&intent.purpose, &consumer_selection) {
        (RemoteVerificationPurpose::OwnerMeasurement { .. }, None) => {
            intent.validate_for(&source.intent, locator, &qualification.intent, occurrence)?;
        }
        (RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. }, Some(selection)) => {
            intent.validate_consumer_for(
                &source.intent,
                locator,
                &qualification.intent,
                occurrence,
                selection,
                coordinate,
            )?;
            read_retained_consumer_prerequisite(
                conn,
                &qualification_id,
                &qualification.intent.owner_principal,
                coordinate,
            )?;
        }
        _ => anyhow::bail!("verifier purpose contradicts retained selection"),
    }
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
            let value: RestoredVerifierObservation = serde_json::from_str(&raw)?;
            match (&intent.purpose, &value) {
                (
                    RemoteVerificationPurpose::OwnerMeasurement { .. },
                    RestoredVerifierObservation::OwnerMeasurement { observation },
                ) => {
                    observation.validate_for_retained(
                        &intent,
                        &source.intent,
                        locator,
                        readiness,
                        &qualification.intent,
                        occurrence,
                    )?;
                }
                (
                    RemoteVerificationPurpose::ConsumerRuntime { .. },
                    RestoredVerifierObservation::ConsumerRuntime { observation },
                ) => {
                    observation.validate_for_intent(&intent)?;
                }
                _ => anyhow::bail!("verifier observation contradicts retained purpose"),
            }
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
        consumer_selection,
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

/// Called inside the reservation/contact transaction. A prior read cannot
/// exclude a subsequently retained termination intent or extend guest life.
fn require_live_occurrence(
    conn: &Connection,
    qualification: &super::runtime_snapshot_qualification::SnapshotQualificationRecord,
    now: i64,
) -> Result<()> {
    ensure!(
        qualification.occurrence.is_some(),
        "restored verifier occurrence is absent"
    );
    let termination_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_snapshot_qualification_termination
         WHERE qualification_operation_id=?1)",
        [&qualification.intent.operation_id],
        |row| row.get(0),
    )?;
    ensure!(
        !termination_exists,
        "restored verifier occurrence has a retained termination intent"
    );
    let expires_at = qualification
        .created_at_ms
        .checked_add(i64::from(qualification.intent.maximum_lifetime_seconds) * 1000)
        .context("restored verifier occurrence lifetime overflow")?;
    ensure!(
        now >= qualification.created_at_ms && now < expires_at,
        "restored verifier occurrence is outside its retained lifetime"
    );
    Ok(())
}

/// Transaction-local prerequisite authentication. The caller owns the
/// transaction and must hold the occurrence contact gate before using this
/// check for a reservation or contact claim.
fn read_consumer_prerequisite(
    conn: &Connection,
    qualification_operation_id: &str,
    owner_principal: &str,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
) -> Result<RestoredVerifierAttemptRecord> {
    let record = read_retained_consumer_prerequisite(
        conn,
        qualification_operation_id,
        owner_principal,
        coordinate,
    )?;
    let qualification =
        super::runtime_snapshot_qualification::read(conn, qualification_operation_id)?
            .context("consumer verification qualification is absent")?;
    let now = i64::try_from(lillux::time::timestamp_millis())?;
    require_live_occurrence(conn, &qualification, now)?;
    Ok(record)
}

/// Historical measurement authentication is distinct from new-contact
/// eligibility: settled occurrences must remain readable after termination.
/// Only an owner observation can satisfy this prerequisite, so consumer
/// evidence cannot recursively become another consumer's measurement root.
fn read_retained_consumer_prerequisite(
    conn: &Connection,
    qualification_operation_id: &str,
    owner_principal: &str,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
) -> Result<RestoredVerifierAttemptRecord> {
    // Reject a consumer prerequisite before `read` follows its own prerequisite.
    // This both enforces the owner-only root and bounds corrupt-record recursion.
    let raw: Option<String> = conn
        .query_row(
            "SELECT intent_json FROM restored_verifier_attempt WHERE operation_id=?1",
            [&coordinate.prerequisite_measurement_attempt_id],
            |row| row.get(0),
        )
        .optional()?;
    let raw = raw.context("consumer verification prerequisite is absent")?;
    ensure!(
        raw.len() <= 6144,
        "consumer prerequisite intent exceeds bound"
    );
    let prerequisite: RestoredVerifierAttemptIntent = serde_json::from_str(&raw)?;
    ensure!(
        matches!(
            prerequisite.purpose,
            RemoteVerificationPurpose::OwnerMeasurement { .. }
        ),
        "consumer prerequisite is not owner measurement"
    );
    let record = read(conn, &coordinate.prerequisite_measurement_attempt_id)?
        .context("consumer verification prerequisite is absent")?;
    ensure!(
        record.phase == RestoredVerifierAttemptPhase::Observed
            && record.intent.qualification_operation_id == qualification_operation_id,
        "consumer verification requires an observed same-occurrence prerequisite"
    );
    let qualification =
        super::runtime_snapshot_qualification::read(conn, qualification_operation_id)?
            .context("consumer verification qualification is absent")?;
    ensure!(
        qualification.intent.owner_principal == owner_principal,
        "consumer verification prerequisite belongs to another operator"
    );
    let occurrence = qualification
        .occurrence
        .as_ref()
        .context("consumer verification occurrence is absent")?;
    let source = super::runtime_snapshot::read(conn, &qualification.intent.snapshot_operation_id)?
        .context("consumer verification snapshot is absent")?;
    record
        .observation
        .as_ref()
        .context("consumer prerequisite observation is absent")?
        .owner_measurement()?
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
    Ok(record)
}

impl RuntimeDb {
    /// Raw evidence is a Blob root, never an inferred generic CAS Object.
    /// These bytes are retained by the existing attempt, not another ledger.
    pub(crate) fn restored_verifier_evidence_blob_roots(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT operation_id FROM restored_verifier_attempt WHERE phase='observed' ORDER BY operation_id",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut roots = BTreeSet::new();
        for id in ids {
            let record = read(&self.conn, &id)?.context("verifier evidence root disappeared")?;
            if let Some(RestoredVerifierObservation::ConsumerRuntime { observation }) =
                record.observation
            {
                roots.insert(observation.evidence_sha256);
            }
        }
        Ok(roots.into_iter().collect())
    }

    /// Store complete transport observation only. Guest settlement and semantic
    /// qualification must be joined by their existing owners before banking.
    /// StateStore must stage these exact evidence bytes before this write.
    pub(crate) fn bind_consumer_verifier_observation(
        &self,
        observation: &ConsumerVerifierAdapterObservation,
        evidence: &[u8],
    ) -> Result<RestoredVerifierAttemptRecord> {
        observation.verify_evidence_bytes(evidence)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &observation.operation_id)?
            .context("consumer observation has no claimed attempt")?;
        observation.validate_for_intent(&record.intent)?;
        let tagged = RestoredVerifierObservation::ConsumerRuntime {
            observation: observation.clone(),
        };
        if record.phase == RestoredVerifierAttemptPhase::Observed {
            ensure!(
                record.observation.as_ref() == Some(&tagged),
                "consumer observation replay changed"
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
            "consumer observation did not follow a contact claim"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let changed = tx.execute(
            "UPDATE restored_verifier_attempt SET phase='observed',observation_json=?2,updated_at_ms=?3
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![observation.operation_id, canonical(&tagged)?, now],
        )?;
        ensure!(changed == 1, "consumer observation bind lost durable CAS");
        let current =
            read(&tx, &observation.operation_id)?.context("consumer observation vanished")?;
        tx.commit()?;
        Ok(current)
    }

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
        let record = read_consumer_prerequisite(
            &tx,
            qualification_operation_id,
            owner_principal,
            coordinate,
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
        self.reserve_verifier_attempt(intent, None)
    }

    pub(crate) fn reserve_consumer_verifier_attempt(
        &self,
        intent: &RestoredVerifierAttemptIntent,
        admission: AuthenticatedConsumerRoot,
    ) -> Result<RestoredVerifierAttemptRecord> {
        self.reserve_verifier_attempt(intent, Some(&admission))
    }

    fn reserve_verifier_attempt(
        &self,
        intent: &RestoredVerifierAttemptIntent,
        admission: Option<&AuthenticatedConsumerRoot>,
    ) -> Result<RestoredVerifierAttemptRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let qualification =
            super::runtime_snapshot_qualification::read(&tx, &intent.qualification_operation_id)?
                .context("restored verifier qualification was not retained")?;
        let selection = admitted_consumer_selection(intent, &qualification.intent, admission)?;
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            ensure!(
                existing.intent == *intent && existing.consumer_selection == selection,
                "restored verifier reservation replay changed"
            );
            tx.commit()?;
            return Ok(existing);
        }
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
        if let Some(admission) = admission {
            intent.validate_consumer_for(
                &source.intent,
                locator,
                &qualification.intent,
                occurrence,
                admission.selection(),
                admission.coordinate(),
            )?;
            read_consumer_prerequisite(
                &tx,
                &qualification.intent.operation_id,
                &qualification.intent.owner_principal,
                admission.coordinate(),
            )?;
        } else {
            intent.validate_for(&source.intent, locator, &qualification.intent, occurrence)?;
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        require_live_occurrence(&tx, &qualification, now)?;
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms.saturating_sub(now) <= 300_000,
            "restored verifier deadline is outside admission window"
        );
        tx.execute(
            "INSERT INTO restored_verifier_attempt
             (operation_id,qualification_operation_id,intent_json,consumer_selection_json,phase,observation_json,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,?4,'reserved',NULL,?5,?5)",
            params![
                intent.operation_id,
                intent.qualification_operation_id,
                canonical(intent)?,
                selection.as_ref().map(canonical).transpose()?,
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
        self.claim_verifier_attempt(operation_id, None)
    }

    pub(crate) fn claim_consumer_verifier_attempt(
        &self,
        operation_id: &str,
        admission: AuthenticatedConsumerRoot,
    ) -> Result<RestoredVerifierAttemptClaim> {
        self.claim_verifier_attempt(operation_id, Some(&admission))
    }

    fn claim_verifier_attempt(
        &self,
        operation_id: &str,
        admission: Option<&AuthenticatedConsumerRoot>,
    ) -> Result<RestoredVerifierAttemptClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("restored verifier was not reserved")?;
        let qualification = super::runtime_snapshot_qualification::read(
            &tx,
            &record.intent.qualification_operation_id,
        )?
        .context("restored verifier qualification disappeared before contact claim")?;
        ensure!(
            admitted_consumer_selection(&record.intent, &qualification.intent, admission)?
                == record.consumer_selection,
            "consumer claim changed admitted selection"
        );
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
        if let Some(admission) = admission {
            read_consumer_prerequisite(
                &tx,
                &qualification.intent.operation_id,
                &qualification.intent.owner_principal,
                admission.coordinate(),
            )?;
        }
        require_live_occurrence(&tx, &qualification, now)?;
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
        let tagged = RestoredVerifierObservation::OwnerMeasurement {
            observation: observation.clone(),
        };
        if record.phase == RestoredVerifierAttemptPhase::Observed {
            ensure!(
                record.observation.as_ref() == Some(&tagged),
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
            params![observation.operation_id, canonical(&tagged)?, now],
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
