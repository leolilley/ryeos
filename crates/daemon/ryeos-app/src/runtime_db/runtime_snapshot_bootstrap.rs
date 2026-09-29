//! One-attempt source-Sandbox bootstrap journal, separate from Worker
//! allocation, snapshot production, and restored-runtime qualification.
//! Pending or quarantined provider contact is never blindly retried.

use super::*;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::runtime_snapshot_bootstrap::{
    RuntimeSnapshotBootstrapIntent, RuntimeSnapshotBootstrapOccurrence,
};

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE runtime_snapshot_bootstrap (
    operation_id TEXT PRIMARY KEY,
    intent_json TEXT NOT NULL,
    intent_digest TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    provider_group_id TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined','occurrence_bound','late_occurrence_bound')),
    occurrence_id TEXT,
    occurrence_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((occurrence_json IS NULL) = (occurrence_id IS NULL)),
    CHECK ((phase IN ('occurrence_bound','late_occurrence_bound')) =
           (occurrence_json IS NOT NULL AND occurrence_id IS NOT NULL)),
    UNIQUE(provider_id,provider_group_id,occurrence_id)
);
CREATE TRIGGER runtime_snapshot_bootstrap_immutable_intent
BEFORE UPDATE ON runtime_snapshot_bootstrap
WHEN NEW.operation_id!=OLD.operation_id OR NEW.intent_json!=OLD.intent_json
    OR NEW.intent_digest!=OLD.intent_digest OR NEW.provider_id!=OLD.provider_id
    OR NEW.provider_group_id!=OLD.provider_group_id OR NEW.created_at_ms!=OLD.created_at_ms
BEGIN SELECT RAISE(ABORT, 'snapshot bootstrap intent is immutable'); END;
CREATE TRIGGER runtime_snapshot_bootstrap_no_delete
BEFORE DELETE ON runtime_snapshot_bootstrap
BEGIN SELECT RAISE(ABORT, 'snapshot bootstrap operation is retained'); END;
CREATE TRIGGER runtime_snapshot_bootstrap_immutable_occurrence
BEFORE UPDATE ON runtime_snapshot_bootstrap
WHEN OLD.occurrence_json IS NOT NULL AND
    (NEW.occurrence_json IS NOT OLD.occurrence_json OR NEW.occurrence_id IS NOT OLD.occurrence_id)
BEGIN SELECT RAISE(ABORT, 'snapshot bootstrap occurrence is immutable'); END;
CREATE TRIGGER runtime_snapshot_bootstrap_monotonic_phase
BEFORE UPDATE ON runtime_snapshot_bootstrap
WHEN NOT (
    NEW.phase=OLD.phase OR
    (OLD.phase='reserved' AND NEW.phase='attempt_pending') OR
    (OLD.phase='attempt_pending' AND NEW.phase IN ('quarantined','occurrence_bound','late_occurrence_bound')) OR
    (OLD.phase='quarantined' AND NEW.phase='late_occurrence_bound')
)
BEGIN SELECT RAISE(ABORT, 'snapshot bootstrap phase cannot move backward'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotBootstrapPhase {
    Reserved,
    AttemptPending,
    Quarantined,
    OccurrenceBound,
    LateOccurrenceBound,
}

impl SnapshotBootstrapPhase {
    fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            "occurrence_bound" => Self::OccurrenceBound,
            "late_occurrence_bound" => Self::LateOccurrenceBound,
            _ => anyhow::bail!("snapshot bootstrap has an invalid phase"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotBootstrapRecord {
    pub intent: RuntimeSnapshotBootstrapIntent,
    pub phase: SnapshotBootstrapPhase,
    pub occurrence: Option<RuntimeSnapshotBootstrapOccurrence>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotBootstrapAttemptClaim {
    StartAttempt(SnapshotBootstrapRecord),
    Reconcile(SnapshotBootstrapRecord),
    OccurrenceBound(SnapshotBootstrapRecord),
    LateOccurrenceBound(SnapshotBootstrapRecord),
}

fn canonical<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(
        ryeos_external_execution_contract::canonical_json(value)?,
    )?)
}

pub(super) fn read(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<SnapshotBootstrapRecord>> {
    let row: Option<(String, String, String, String, String, Option<String>, Option<String>, i64, i64)> = conn
        .query_row(
            "SELECT intent_json,intent_digest,provider_id,provider_group_id,phase,occurrence_id,occurrence_json,created_at_ms,updated_at_ms
             FROM runtime_snapshot_bootstrap WHERE operation_id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?)),
        )
        .optional()?;
    let Some((
        intent_json,
        intent_digest,
        provider_id,
        provider_group_id,
        phase,
        occurrence_id,
        occurrence_json,
        created,
        updated,
    )) = row
    else {
        return Ok(None);
    };
    ensure!(
        intent_json.len() <= 24 * 1024,
        "bootstrap intent exceeds bound"
    );
    let intent: RuntimeSnapshotBootstrapIntent = serde_json::from_str(&intent_json)?;
    ensure!(
        intent.operation_id == operation_id
            && intent.digest()? == intent_digest
            && intent.provider_id == provider_id
            && intent.provider_group_id == provider_group_id
            && canonical(&intent)? == intent_json,
        "bootstrap retained intent changed identity"
    );
    let phase = SnapshotBootstrapPhase::parse(&phase)?;
    let occurrence = occurrence_json
        .map(|raw| {
            ensure!(raw.len() <= 8192, "bootstrap occurrence exceeds bound");
            let occurrence: RuntimeSnapshotBootstrapOccurrence = serde_json::from_str(&raw)?;
            occurrence.validate_for(&intent)?;
            ensure!(
                canonical(&occurrence)? == raw,
                "bootstrap occurrence is noncanonical"
            );
            Ok(occurrence)
        })
        .transpose()?;
    ensure!(
        matches!(
            phase,
            SnapshotBootstrapPhase::OccurrenceBound | SnapshotBootstrapPhase::LateOccurrenceBound
        ) == occurrence.is_some()
            && occurrence
                .as_ref()
                .map(|value| value.occurrence_id.as_str())
                == occurrence_id.as_deref()
            && created > 0
            && updated >= created,
        "bootstrap phase or timestamps contradict retained evidence"
    );
    Ok(Some(SnapshotBootstrapRecord {
        intent,
        phase,
        occurrence,
        created_at_ms: created,
        updated_at_ms: updated,
    }))
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT operation_id FROM runtime_snapshot_bootstrap ORDER BY operation_id")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        read(conn, &id)?.context("snapshot bootstrap row disappeared")?;
    }
    Ok(())
}

impl RuntimeDb {
    pub fn snapshot_bootstrap_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<SnapshotBootstrapRecord>> {
        read(&self.conn, operation_id)
    }

    pub fn reserve_snapshot_bootstrap(
        &self,
        intent: &RuntimeSnapshotBootstrapIntent,
    ) -> Result<SnapshotBootstrapRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            ensure!(
                existing.intent == *intent,
                "bootstrap reservation replay changed"
            );
            tx.commit()?;
            return Ok(existing);
        }
        intent.validate()?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms.saturating_sub(now) <= 300_000,
            "bootstrap deadline is outside its admission window"
        );
        tx.execute(
            "INSERT INTO runtime_snapshot_bootstrap
             (operation_id,intent_json,intent_digest,provider_id,provider_group_id,phase,occurrence_id,occurrence_json,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,?4,?5,'reserved',NULL,NULL,?6,?6)",
            params![intent.operation_id, canonical(intent)?, intent.digest()?, intent.provider_id, intent.provider_group_id, now],
        )?;
        let record = read(&tx, &intent.operation_id)?.context("bootstrap reservation vanished")?;
        tx.commit()?;
        Ok(record)
    }

    pub fn claim_snapshot_bootstrap_attempt(
        &self,
        operation_id: &str,
    ) -> Result<SnapshotBootstrapAttemptClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("bootstrap was not reserved")?;
        match record.phase {
            SnapshotBootstrapPhase::OccurrenceBound => {
                tx.commit()?;
                return Ok(SnapshotBootstrapAttemptClaim::OccurrenceBound(record));
            }
            SnapshotBootstrapPhase::LateOccurrenceBound => {
                tx.commit()?;
                return Ok(SnapshotBootstrapAttemptClaim::LateOccurrenceBound(record));
            }
            SnapshotBootstrapPhase::AttemptPending | SnapshotBootstrapPhase::Quarantined => {
                tx.commit()?;
                return Ok(SnapshotBootstrapAttemptClaim::Reconcile(record));
            }
            SnapshotBootstrapPhase::Reserved => {}
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < record.intent.attempt_deadline_ms,
            "bootstrap deadline expired"
        );
        let changed = tx.execute(
            "UPDATE runtime_snapshot_bootstrap SET phase='attempt_pending',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='reserved'",
            params![operation_id, now],
        )?;
        ensure!(changed == 1, "bootstrap attempt lost durable CAS");
        let current = read(&tx, operation_id)?.context("bootstrap attempt vanished")?;
        tx.commit()?;
        Ok(SnapshotBootstrapAttemptClaim::StartAttempt(current))
    }

    pub fn bind_snapshot_bootstrap_occurrence(
        &self,
        occurrence: &RuntimeSnapshotBootstrapOccurrence,
    ) -> Result<SnapshotBootstrapRecord> {
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        self.bind_snapshot_bootstrap_occurrence_at(occurrence, now)
    }

    fn bind_snapshot_bootstrap_occurrence_at(
        &self,
        occurrence: &RuntimeSnapshotBootstrapOccurrence,
        now: i64,
    ) -> Result<SnapshotBootstrapRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &occurrence.operation_id)?.context("bootstrap attempt is absent")?;
        occurrence.validate_for(&record.intent)?;
        if matches!(
            record.phase,
            SnapshotBootstrapPhase::OccurrenceBound | SnapshotBootstrapPhase::LateOccurrenceBound
        ) {
            ensure!(
                record.occurrence.as_ref() == Some(occurrence),
                "bootstrap occurrence changed after binding"
            );
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            matches!(
                record.phase,
                SnapshotBootstrapPhase::AttemptPending | SnapshotBootstrapPhase::Quarantined
            ),
            "bootstrap attempt was not contacted"
        );
        let late = record.phase == SnapshotBootstrapPhase::Quarantined
            || occurrence.contact_deadline_exceeded
            || now >= record.intent.attempt_deadline_ms;
        let phase = if late {
            "late_occurrence_bound"
        } else {
            "occurrence_bound"
        };
        let changed = tx.execute(
            "UPDATE runtime_snapshot_bootstrap SET phase=?2,occurrence_id=?3,occurrence_json=?4,updated_at_ms=?5
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![occurrence.operation_id, phase, occurrence.occurrence_id, canonical(occurrence)?, now],
        )?;
        ensure!(changed == 1, "bootstrap occurrence lost durable CAS");
        let bound = read(&tx, &occurrence.operation_id)?.context("bound bootstrap vanished")?;
        tx.commit()?;
        Ok(bound)
    }

    pub fn quarantine_snapshot_bootstrap_attempt(
        &self,
        operation_id: &str,
    ) -> Result<SnapshotBootstrapRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("bootstrap attempt is absent")?;
        ensure!(
            matches!(
                record.phase,
                SnapshotBootstrapPhase::AttemptPending | SnapshotBootstrapPhase::Quarantined
            ),
            "bootstrap attempt is not uncertain"
        );
        if record.phase == SnapshotBootstrapPhase::AttemptPending {
            let now = i64::try_from(lillux::time::timestamp_millis())?;
            tx.execute(
                "UPDATE runtime_snapshot_bootstrap SET phase='quarantined',updated_at_ms=?2
                 WHERE operation_id=?1 AND phase='attempt_pending'",
                params![operation_id, now],
            )?;
        }
        let current = read(&tx, operation_id)?.context("quarantined bootstrap vanished")?;
        tx.commit()?;
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotSource;
    use ryeos_external_execution_contract::runtime_snapshot_bootstrap::BOOTSTRAP_INTENT_SCHEMA;

    fn intent() -> RuntimeSnapshotBootstrapIntent {
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
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
            attempt_deadline_ms: now + 60_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
    }

    #[test]
    fn create_claim_is_one_shot_and_uncertain_result_never_recontacts() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let intent = intent();
        let reserved = db.reserve_snapshot_bootstrap(&intent).unwrap();
        assert_eq!(reserved.phase, SnapshotBootstrapPhase::Reserved);
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::StartAttempt(_)
        ));
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::Reconcile(_)
        ));
        let quarantined = db
            .quarantine_snapshot_bootstrap_attempt(&intent.operation_id)
            .unwrap();
        assert_eq!(quarantined.phase, SnapshotBootstrapPhase::Quarantined);
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::Reconcile(_)
        ));
        let occurrence = RuntimeSnapshotBootstrapOccurrence {
            schema: 1,
            operation_id: intent.operation_id,
            occurrence_id: "sbx-exact".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"status": "creating"}),
            contact_deadline_exceeded: false,
        };
        let late = db.bind_snapshot_bootstrap_occurrence(&occurrence).unwrap();
        assert_eq!(late.phase, SnapshotBootstrapPhase::LateOccurrenceBound);
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&late.intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::LateOccurrenceBound(_)
        ));
        assert_eq!(
            db.bind_snapshot_bootstrap_occurrence(&occurrence).unwrap(),
            late
        );
    }

    #[test]
    fn bound_occurrence_is_retained_and_cannot_be_switched() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let intent = intent();
        db.reserve_snapshot_bootstrap(&intent).unwrap();
        db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
            .unwrap();
        let occurrence = RuntimeSnapshotBootstrapOccurrence {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: "sbx-exact".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"status": "creating"}),
            contact_deadline_exceeded: false,
        };
        let bound = db.bind_snapshot_bootstrap_occurrence(&occurrence).unwrap();
        assert_eq!(bound.phase, SnapshotBootstrapPhase::OccurrenceBound);
        assert_eq!(bound.occurrence, Some(occurrence.clone()));
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::OccurrenceBound(_)
        ));
        let mut switched = occurrence;
        switched.occurrence_id = "sbx-other".into();
        assert!(db.bind_snapshot_bootstrap_occurrence(&switched).is_err());
        assert!(db.conn.execute(
            "UPDATE runtime_snapshot_bootstrap SET phase='reserved',occurrence_json=NULL,occurrence_id=NULL WHERE operation_id=?1",
            [&intent.operation_id],
        ).is_err());
        assert_eq!(
            db.snapshot_bootstrap_operation(&intent.operation_id)
                .unwrap(),
            Some(bound)
        );
    }

    #[test]
    fn phase_cannot_reopen_contact_and_occurrence_is_unique_per_provider_group() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let first = intent();
        db.reserve_snapshot_bootstrap(&first).unwrap();
        db.claim_snapshot_bootstrap_attempt(&first.operation_id)
            .unwrap();
        assert!(
            db.conn
                .execute(
                    "UPDATE runtime_snapshot_bootstrap SET phase='reserved' WHERE operation_id=?1",
                    [&first.operation_id],
                )
                .is_err()
        );
        let occurrence = RuntimeSnapshotBootstrapOccurrence {
            schema: 1,
            operation_id: first.operation_id.clone(),
            occurrence_id: "sbx-exact".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"status": "creating"}),
            contact_deadline_exceeded: false,
        };
        db.bind_snapshot_bootstrap_occurrence(&occurrence).unwrap();
        let mut second = intent();
        second.source = RuntimeSnapshotSource::BundleMaterialization {
            materialization_attestation_hash: "b".repeat(64),
            source_coordinate_digest: "7".repeat(64),
            materialization_binding_digest: "8".repeat(64),
        };
        second.operation_id = second.derived_operation_id().unwrap();
        db.reserve_snapshot_bootstrap(&second).unwrap();
        db.claim_snapshot_bootstrap_attempt(&second.operation_id)
            .unwrap();
        let mut duplicate = occurrence;
        duplicate.operation_id = second.operation_id.clone();
        assert!(db.bind_snapshot_bootstrap_occurrence(&duplicate).is_err());
        assert_eq!(
            db.snapshot_bootstrap_operation(&second.operation_id)
                .unwrap()
                .unwrap()
                .phase,
            SnapshotBootstrapPhase::AttemptPending
        );
    }

    #[test]
    fn occurrence_after_local_deadline_is_cleanup_only_even_if_adapter_flag_is_false() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let intent = intent();
        db.reserve_snapshot_bootstrap(&intent).unwrap();
        db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
            .unwrap();
        let occurrence = RuntimeSnapshotBootstrapOccurrence {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: "sbx-late".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"status": "creating"}),
            contact_deadline_exceeded: false,
        };
        let record = db
            .bind_snapshot_bootstrap_occurrence_at(&occurrence, intent.attempt_deadline_ms)
            .unwrap();
        assert_eq!(record.phase, SnapshotBootstrapPhase::LateOccurrenceBound);
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::LateOccurrenceBound(_)
        ));
    }

    #[test]
    fn pending_contact_remains_reconcile_only_after_database_reopen() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let intent = intent();
        {
            let db = RuntimeDb::open(&path).unwrap();
            db.reserve_snapshot_bootstrap(&intent).unwrap();
            assert!(matches!(
                db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                    .unwrap(),
                SnapshotBootstrapAttemptClaim::StartAttempt(_)
            ));
        }
        let db = RuntimeDb::open(&path).unwrap();
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::Reconcile(_)
        ));
    }
}
