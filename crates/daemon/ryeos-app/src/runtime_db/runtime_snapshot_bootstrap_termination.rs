//! One-shot termination of a snapshot-bootstrap source occurrence. An
//! uncertain POST is observed or quarantined, never sent a second time.
//! Provider terminal status does not itself prove guest-writer exclusion.

use super::*;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::runtime_snapshot_bootstrap::{
    BootstrapCleanupMode, RuntimeSnapshotBootstrapTerminalObservation,
    RuntimeSnapshotBootstrapTerminationIntent,
};

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE runtime_snapshot_bootstrap_termination (
    operation_id TEXT PRIMARY KEY,
    bootstrap_operation_id TEXT NOT NULL REFERENCES runtime_snapshot_bootstrap(operation_id),
    intent_json TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined','terminal')),
    observation_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((phase='terminal') = (observation_json IS NOT NULL))
);
CREATE TRIGGER runtime_snapshot_bootstrap_termination_immutable_intent
BEFORE UPDATE ON runtime_snapshot_bootstrap_termination
WHEN NEW.operation_id!=OLD.operation_id
    OR NEW.bootstrap_operation_id!=OLD.bootstrap_operation_id
    OR NEW.created_at_ms!=OLD.created_at_ms
    OR (NEW.intent_json!=OLD.intent_json AND NOT (
        OLD.phase='reserved' AND NEW.phase='reserved'
        AND json_remove(NEW.intent_json,'$.attempt_deadline_ms')
            = json_remove(OLD.intent_json,'$.attempt_deadline_ms')
        AND json_type(NEW.intent_json,'$.attempt_deadline_ms')='integer'
        AND json_extract(NEW.intent_json,'$.attempt_deadline_ms')
            > json_extract(OLD.intent_json,'$.attempt_deadline_ms')
    ))
BEGIN SELECT RAISE(ABORT, 'bootstrap termination intent is immutable'); END;
CREATE TRIGGER runtime_snapshot_bootstrap_termination_immutable_observation
BEFORE UPDATE ON runtime_snapshot_bootstrap_termination
WHEN OLD.observation_json IS NOT NULL AND NEW.observation_json IS NOT OLD.observation_json
BEGIN SELECT RAISE(ABORT, 'bootstrap terminal observation is immutable'); END;
CREATE TRIGGER runtime_snapshot_bootstrap_termination_monotonic_phase
BEFORE UPDATE ON runtime_snapshot_bootstrap_termination
WHEN NOT (
    NEW.phase=OLD.phase OR
    (OLD.phase='reserved' AND NEW.phase='attempt_pending') OR
    (OLD.phase='attempt_pending' AND NEW.phase IN ('quarantined','terminal')) OR
    (OLD.phase='quarantined' AND NEW.phase='terminal')
)
    OR NEW.updated_at_ms < OLD.updated_at_ms
BEGIN SELECT RAISE(ABORT, 'bootstrap termination phase cannot move backward'); END;
CREATE TRIGGER runtime_snapshot_bootstrap_termination_no_delete
BEFORE DELETE ON runtime_snapshot_bootstrap_termination
BEGIN SELECT RAISE(ABORT, 'bootstrap termination is retained'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapTerminationPhase {
    Reserved,
    AttemptPending,
    Quarantined,
    Terminal,
}

impl BootstrapTerminationPhase {
    fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            "terminal" => Self::Terminal,
            _ => anyhow::bail!("bootstrap termination has invalid phase"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BootstrapTerminationRecord {
    pub intent: RuntimeSnapshotBootstrapTerminationIntent,
    pub phase: BootstrapTerminationPhase,
    pub observation: Option<RuntimeSnapshotBootstrapTerminalObservation>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapTerminationClaim {
    StartAttempt(BootstrapTerminationRecord),
    Reconcile(BootstrapTerminationRecord),
    Terminal(BootstrapTerminationRecord),
}

fn canonical<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(
        ryeos_external_execution_contract::canonical_json(value)?,
    )?)
}

pub(super) fn read(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<BootstrapTerminationRecord>> {
    let row: Option<(String, String, String, Option<String>, i64, i64)> = conn.query_row(
        "SELECT bootstrap_operation_id,intent_json,phase,observation_json,created_at_ms,updated_at_ms
         FROM runtime_snapshot_bootstrap_termination WHERE operation_id=?1",
        [operation_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    ).optional()?;
    let Some((bootstrap_id, intent_json, phase, observation_json, created, updated)) = row else {
        return Ok(None);
    };
    ensure!(
        intent_json.len() <= 2048,
        "bootstrap termination intent exceeds bound"
    );
    let intent: RuntimeSnapshotBootstrapTerminationIntent = serde_json::from_str(&intent_json)?;
    let bootstrap = super::runtime_snapshot_bootstrap::read(conn, &bootstrap_id)?
        .context("bootstrap termination source disappeared")?;
    let occurrence = bootstrap
        .occurrence
        .as_ref()
        .context("bootstrap termination source has no occurrence")?;
    intent.validate_for(&bootstrap.intent, occurrence)?;
    ensure!(
        intent.operation_id == operation_id
            && intent.bootstrap_operation_id == bootstrap_id
            && canonical(&intent)? == intent_json,
        "bootstrap termination retained intent changed identity"
    );
    let phase = BootstrapTerminationPhase::parse(&phase)?;
    let observation = observation_json
        .map(|raw| -> Result<_> {
            ensure!(
                raw.len() <= 1024,
                "bootstrap terminal observation exceeds bound"
            );
            let value: RuntimeSnapshotBootstrapTerminalObservation = serde_json::from_str(&raw)?;
            value.validate_for(&intent)?;
            ensure!(
                canonical(&value)? == raw,
                "bootstrap terminal observation is noncanonical"
            );
            Ok(value)
        })
        .transpose()?;
    ensure!(
        (phase == BootstrapTerminationPhase::Terminal) == observation.is_some()
            && created > 0
            && updated >= created
            && (phase != BootstrapTerminationPhase::Reserved
                || (intent.attempt_deadline_ms > updated
                    && intent.attempt_deadline_ms.saturating_sub(updated) <= 300_000)),
        "bootstrap termination phase contradicts evidence"
    );
    Ok(Some(BootstrapTerminationRecord {
        intent,
        phase,
        observation,
        created_at_ms: created,
        updated_at_ms: updated,
    }))
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT operation_id FROM runtime_snapshot_bootstrap_termination ORDER BY operation_id",
    )?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        read(conn, &id)?.context("bootstrap termination row disappeared")?;
    }
    Ok(())
}

impl RuntimeDb {
    pub fn bootstrap_has_unsettled_snapshot(&self, bootstrap_operation_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM runtime_snapshot_operation WHERE source_bootstrap_operation_id=?1 AND phase!='bound'",
            [bootstrap_operation_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn bootstrap_termination_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<BootstrapTerminationRecord>> {
        read(&self.conn, operation_id)
    }

    pub fn reserve_bootstrap_termination(
        &self,
        intent: &RuntimeSnapshotBootstrapTerminationIntent,
    ) -> Result<BootstrapTerminationRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            if existing.intent != *intent {
                let mut same_core = existing.intent.clone();
                same_core.attempt_deadline_ms = intent.attempt_deadline_ms;
                ensure!(
                    same_core == *intent,
                    "bootstrap termination reservation changed authority"
                );
                ensure!(
                    existing.phase == BootstrapTerminationPhase::Reserved,
                    "contacted bootstrap termination cannot renew its deadline"
                );
                let now = i64::try_from(lillux::time::timestamp_millis())?;
                ensure!(
                    intent.attempt_deadline_ms > existing.intent.attempt_deadline_ms
                        && intent.attempt_deadline_ms > now
                        && intent.attempt_deadline_ms.saturating_sub(now) <= 300_000,
                    "bootstrap termination renewal exceeds its pre-contact bound"
                );
                tx.execute(
                    "UPDATE runtime_snapshot_bootstrap_termination SET intent_json=?2,updated_at_ms=?3
                     WHERE operation_id=?1 AND phase='reserved'",
                    params![intent.operation_id, canonical(intent)?, now],
                )?;
                let renewed = read(&tx, &intent.operation_id)?
                    .context("renewed bootstrap termination disappeared")?;
                tx.commit()?;
                return Ok(renewed);
            }
            tx.commit()?;
            return Ok(existing);
        }
        let bootstrap =
            super::runtime_snapshot_bootstrap::read(&tx, &intent.bootstrap_operation_id)?
                .context("bootstrap termination source was not retained")?;
        let occurrence = bootstrap
            .occurrence
            .as_ref()
            .context("bootstrap termination source has no occurrence")?;
        intent.validate_for(&bootstrap.intent, occurrence)?;
        let unsettled_snapshots: i64 = tx.query_row(
            "SELECT COUNT(*) FROM runtime_snapshot_operation WHERE source_bootstrap_operation_id=?1 AND phase!='bound'",
            [&intent.bootstrap_operation_id],
            |row| row.get(0),
        )?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        match intent.mode {
            BootstrapCleanupMode::TerminateOnce => ensure!(
                unsettled_snapshots == 0,
                "bootstrap source has an unsettled snapshot upload"
            ),
            BootstrapCleanupMode::ObserveOnly => {
                let created_at = occurrence.provider_creation_observation["created_at"]
                    .as_str()
                    .context("read-only cleanup has no exact creation timestamp")?;
                let created = chrono::DateTime::parse_from_rfc3339(created_at)?.timestamp_millis();
                let timeout_seconds = occurrence.provider_creation_observation["timeout_seconds"]
                    .as_u64()
                    .context("read-only cleanup has no exact lifetime")?;
                let lifetime_ms = i64::try_from(timeout_seconds)?
                    .checked_mul(1_000)
                    .context("bootstrap lifetime overflow")?;
                ensure!(
                    occurrence.creation_attributes_verified
                        && now >= created.saturating_add(lifetime_ms),
                    "read-only cleanup requires the exact source lifetime to elapse"
                );
            }
        }
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms.saturating_sub(now) <= 300_000,
            "bootstrap termination deadline outside admission window"
        );
        tx.execute(
            "INSERT INTO runtime_snapshot_bootstrap_termination
             (operation_id,bootstrap_operation_id,intent_json,phase,observation_json,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,'reserved',NULL,?4,?4)",
            params![intent.operation_id, intent.bootstrap_operation_id, canonical(intent)?, now],
        )?;
        let record = read(&tx, &intent.operation_id)?
            .context("bootstrap termination reservation vanished")?;
        tx.commit()?;
        Ok(record)
    }

    pub fn claim_bootstrap_termination_attempt(
        &self,
        operation_id: &str,
    ) -> Result<BootstrapTerminationClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("bootstrap termination was not reserved")?;
        match record.phase {
            BootstrapTerminationPhase::Terminal => {
                tx.commit()?;
                return Ok(BootstrapTerminationClaim::Terminal(record));
            }
            BootstrapTerminationPhase::AttemptPending | BootstrapTerminationPhase::Quarantined => {
                tx.commit()?;
                return Ok(BootstrapTerminationClaim::Reconcile(record));
            }
            BootstrapTerminationPhase::Reserved => {}
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < record.intent.attempt_deadline_ms
                && record.intent.attempt_deadline_ms.saturating_sub(now) <= 300_000,
            "bootstrap termination deadline is outside its contact window"
        );
        let changed = tx.execute(
            "UPDATE runtime_snapshot_bootstrap_termination SET phase='attempt_pending',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='reserved'",
            params![operation_id, now],
        )?;
        ensure!(changed == 1, "bootstrap termination lost durable CAS");
        let current = read(&tx, operation_id)?.context("bootstrap termination attempt vanished")?;
        tx.commit()?;
        Ok(BootstrapTerminationClaim::StartAttempt(current))
    }

    pub fn quarantine_bootstrap_termination_attempt(
        &self,
        operation_id: &str,
    ) -> Result<BootstrapTerminationRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("bootstrap termination attempt absent")?;
        ensure!(
            matches!(
                record.phase,
                BootstrapTerminationPhase::AttemptPending | BootstrapTerminationPhase::Quarantined
            ),
            "bootstrap termination attempt is not uncertain"
        );
        if record.phase == BootstrapTerminationPhase::AttemptPending {
            let now = i64::try_from(lillux::time::timestamp_millis())?;
            tx.execute(
                "UPDATE runtime_snapshot_bootstrap_termination SET phase='quarantined',updated_at_ms=?2
                 WHERE operation_id=?1 AND phase='attempt_pending'",
                params![operation_id, now],
            )?;
        }
        let current =
            read(&tx, operation_id)?.context("bootstrap termination quarantine vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn bind_bootstrap_terminal_observation(
        &self,
        observation: &RuntimeSnapshotBootstrapTerminalObservation,
    ) -> Result<BootstrapTerminationRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &observation.operation_id)?
            .context("bootstrap termination attempt absent")?;
        observation.validate_for(&record.intent)?;
        if record.phase == BootstrapTerminationPhase::Terminal {
            ensure!(
                record.observation.as_ref() == Some(observation),
                "bootstrap terminal observation changed"
            );
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            matches!(
                record.phase,
                BootstrapTerminationPhase::AttemptPending | BootstrapTerminationPhase::Quarantined
            ),
            "bootstrap termination was not contacted"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let changed = tx.execute(
            "UPDATE runtime_snapshot_bootstrap_termination SET phase='terminal',observation_json=?2,updated_at_ms=?3
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![observation.operation_id, canonical(observation)?, now],
        )?;
        ensure!(
            changed == 1,
            "bootstrap terminal observation lost durable CAS"
        );
        let current = read(&tx, &observation.operation_id)?
            .context("bootstrap terminal observation vanished")?;
        tx.commit()?;
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotSource;
    use ryeos_external_execution_contract::runtime_snapshot_bootstrap::{
        BOOTSTRAP_INTENT_SCHEMA, RuntimeSnapshotBootstrapIntent, RuntimeSnapshotBootstrapOccurrence,
    };

    fn source(
        db: &RuntimeDb,
        late: bool,
    ) -> (
        RuntimeSnapshotBootstrapIntent,
        RuntimeSnapshotBootstrapOccurrence,
    ) {
        source_with_created_at(db, late, &chrono::Utc::now().to_rfc3339())
    }

    fn source_with_created_at(
        db: &RuntimeDb,
        late: bool,
        created_at: &str,
    ) -> (
        RuntimeSnapshotBootstrapIntent,
        RuntimeSnapshotBootstrapOccurrence,
    ) {
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        let mut intent = RuntimeSnapshotBootstrapIntent {
            schema: BOOTSTRAP_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "1".repeat(64)),
            provider_id: "render-sandbox-early-access".into(),
            provider_group_id: "sbg-exact".into(),
            production_binding_digest: "2".repeat(64),
            bootstrap_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            provider_spec_digest: "5".repeat(64),
            settings_digest: "6".repeat(64),
            source: RuntimeSnapshotSource::BundleMaterialization {
                materialization_attestation_hash: "7".repeat(64),
                source_coordinate_digest: "8".repeat(64),
                materialization_binding_digest: "9".repeat(64),
            },
            guest_runtime_manifest_hash: "a".repeat(64),
            maximum_lifetime_seconds: 900,
            attempt_deadline_ms: now + 60_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        db.reserve_snapshot_bootstrap(&intent).unwrap();
        db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
            .unwrap();
        if late {
            db.quarantine_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap();
        }
        let occurrence = RuntimeSnapshotBootstrapOccurrence {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: "sbx-exact".into(),
            provider_response_sha256: "b".repeat(64),
            provider_creation_observation: serde_json::json!({"status":"creating","created_at":created_at,"timeout_seconds":900}),
            creation_attributes_verified: true,
            contact_deadline_exceeded: false,
        };
        db.bind_snapshot_bootstrap_occurrence(&occurrence).unwrap();
        (intent, occurrence)
    }

    fn termination(
        source: &RuntimeSnapshotBootstrapIntent,
        occurrence: &RuntimeSnapshotBootstrapOccurrence,
    ) -> RuntimeSnapshotBootstrapTerminationIntent {
        let mut intent = RuntimeSnapshotBootstrapTerminationIntent {
            schema: 2,
            mode: BootstrapCleanupMode::TerminateOnce,
            operation_id: String::new(),
            bootstrap_operation_id: source.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            owner_principal: source.owner_principal.clone(),
            provider_id: source.provider_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            provider_spec_digest: source.provider_spec_digest.clone(),
            attempt_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap() + 60_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
    }

    fn ready_snapshot(
        db: &RuntimeDb,
        source: &RuntimeSnapshotBootstrapIntent,
        occurrence: &RuntimeSnapshotBootstrapOccurrence,
    ) -> ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotIntent {
        let created_at = occurrence.provider_creation_observation["created_at"]
            .as_str()
            .unwrap()
            .to_owned();
        let observed = ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapReadinessObservation {
            schema: 1,
            operation_id: source.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            provider_response_sha256: "c".repeat(64),
            observed_created_at: created_at.clone(),
            observed_at_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap(),
        };
        db.bind_snapshot_bootstrap_readiness(&observed).unwrap();
        let mut snapshot = ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotIntent {
            schema: ryeos_external_execution_contract::runtime_snapshot::RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: source.owner_principal.clone(),
            provider_id: source.provider_id.clone(),
            source_occurrence_id: occurrence.occurrence_id.clone(),
            source_bootstrap_operation_id: Some(source.operation_id.clone()),
            source_created_at: Some(created_at),
            source_timeout_seconds: Some(source.maximum_lifetime_seconds),
            provider_group_id: source.provider_group_id.clone(),
            production_profile_digest: source.production_binding_digest.clone(),
            adapter_artifact_hash: source.adapter_artifact_hash.clone(),
            provider_spec_digest: source.provider_spec_digest.clone(),
            settings_digest: source.settings_digest.clone(),
            source: source.source.clone(),
            guest_runtime_manifest_hash: source.guest_runtime_manifest_hash.clone(),
            owner_executable_sha256: "b".repeat(64),
            controller_public_root: format!("ed25519:{}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [3u8; 32])),
            upload_sha256: "d".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap() + 60_000,
        };
        snapshot.operation_id = snapshot.derived_operation_id().unwrap();
        snapshot
    }

    #[test]
    fn snapshot_upload_and_source_termination_exclude_each_other() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (first_source, first_occurrence) = source(&db, false);
        let snapshot = ready_snapshot(&db, &first_source, &first_occurrence);
        db.reserve_runtime_snapshot(&snapshot).unwrap();
        assert!(
            db.reserve_bootstrap_termination(&termination(&first_source, &first_occurrence))
                .is_err()
        );

        let other = RuntimeDb::new_in_memory().unwrap();
        let (source, occurrence) = source(&other, false);
        let snapshot = ready_snapshot(&other, &source, &occurrence);
        other
            .reserve_bootstrap_termination(&termination(&source, &occurrence))
            .unwrap();
        assert!(other.reserve_runtime_snapshot(&snapshot).is_err());
    }

    #[test]
    fn materialized_parent_window_is_bounded_by_retained_source_expiry() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (source, occurrence) = source(&db, false);
        let mut snapshot = ready_snapshot(&db, &source, &occurrence);
        let created =
            chrono::DateTime::parse_from_rfc3339(snapshot.source_created_at.as_deref().unwrap())
                .unwrap()
                .timestamp_millis();
        let mut beyond_source = snapshot.clone();
        beyond_source.attempt_deadline_ms = created + 901_000;
        beyond_source.operation_id = beyond_source.derived_operation_id().unwrap();
        assert!(db.reserve_runtime_snapshot(&beyond_source).is_err());

        snapshot.attempt_deadline_ms = created + 600_000;
        snapshot.operation_id = snapshot.derived_operation_id().unwrap();
        db.reserve_runtime_snapshot(&snapshot).unwrap();
    }

    #[test]
    fn uncertain_upload_allows_only_read_only_cleanup_after_exact_lifetime() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let created_at = (chrono::Utc::now() - chrono::Duration::seconds(898)).to_rfc3339();
        let (source, occurrence) = source_with_created_at(&db, false, &created_at);
        let mut snapshot = ready_snapshot(&db, &source, &occurrence);
        snapshot.attempt_deadline_ms = chrono::DateTime::parse_from_rfc3339(&created_at)
            .unwrap()
            .timestamp_millis()
            + 900_000;
        snapshot.operation_id = snapshot.derived_operation_id().unwrap();
        db.reserve_runtime_snapshot(&snapshot).unwrap();
        db.claim_runtime_snapshot_attempt(&snapshot.operation_id, &snapshot.digest().unwrap())
            .unwrap();
        db.quarantine_runtime_snapshot_attempt(&snapshot.operation_id, &snapshot.digest().unwrap())
            .unwrap();
        let locator = ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotLocator {
            schema:
                ryeos_external_execution_contract::runtime_snapshot::RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: snapshot.operation_id.clone(),
            intent_digest: snapshot.digest().unwrap(),
            source_occurrence_id: snapshot.source_occurrence_id.clone(),
            provider_group_id: snapshot.provider_group_id.clone(),
            snapshot_id: "snp-late".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"schema":1}),
            adapter_observation_sha256: lillux::sha256_hex(br#"{"schema":1}"#),
        };
        assert_eq!(
            db.bind_runtime_snapshot_locator(&locator, true)
                .unwrap()
                .phase,
            super::runtime_snapshot::RuntimeSnapshotPhase::LateObserved
        );
        assert!(
            db.bootstrap_has_unsettled_snapshot(&source.operation_id)
                .unwrap()
        );

        let mut cleanup = termination(&source, &occurrence);
        assert!(db.reserve_bootstrap_termination(&cleanup).is_err());
        lillux::time::sleep(lillux::time::Duration::from_secs(3));
        cleanup.mode = BootstrapCleanupMode::ObserveOnly;
        let reserved = db.reserve_bootstrap_termination(&cleanup).unwrap();
        assert_eq!(reserved.intent.mode, BootstrapCleanupMode::ObserveOnly);
        let claim = db
            .claim_bootstrap_termination_attempt(&cleanup.operation_id)
            .unwrap();
        assert!(matches!(claim, BootstrapTerminationClaim::StartAttempt(_)));
        let mut mutation = cleanup.clone();
        mutation.mode = BootstrapCleanupMode::TerminateOnce;
        assert!(db.reserve_bootstrap_termination(&mutation).is_err());

        let other = RuntimeDb::new_in_memory().unwrap();
        let created_at = chrono::Utc::now().to_rfc3339();
        let (source, occurrence) = source_with_created_at(&other, false, &created_at);
        let mut early = termination(&source, &occurrence);
        early.mode = BootstrapCleanupMode::ObserveOnly;
        assert!(other.reserve_bootstrap_termination(&early).is_err());
    }

    #[test]
    fn snapshot_reopen_rejoins_the_exact_bootstrap_creation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let snapshot = {
            let db = RuntimeDb::open(&path).unwrap();
            let (source, occurrence) = source(&db, false);
            let snapshot = ready_snapshot(&db, &source, &occurrence);
            db.reserve_runtime_snapshot(&snapshot).unwrap();
            snapshot
        };
        let db = RuntimeDb::open(&path).unwrap();
        assert_eq!(
            db.runtime_snapshot_operation(&snapshot.operation_id)
                .unwrap()
                .unwrap()
                .intent,
            snapshot
        );
        assert!(db.conn.execute(
            "UPDATE runtime_snapshot_operation SET source_bootstrap_operation_id=?2 WHERE operation_id=?1",
            params![snapshot.operation_id, "0".repeat(64)],
        ).is_err());
    }

    #[test]
    fn late_source_can_be_terminated_but_mutation_is_one_shot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let (intent, occurrence) = {
            let db = RuntimeDb::open(&path).unwrap();
            source(&db, true)
        };
        let termination = termination(&intent, &occurrence);
        let db = RuntimeDb::open(&path).unwrap();
        db.reserve_bootstrap_termination(&termination).unwrap();
        assert!(matches!(
            db.claim_bootstrap_termination_attempt(&termination.operation_id)
                .unwrap(),
            BootstrapTerminationClaim::StartAttempt(_)
        ));
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert!(matches!(
            db.claim_bootstrap_termination_attempt(&termination.operation_id)
                .unwrap(),
            BootstrapTerminationClaim::Reconcile(_)
        ));
        let observation = RuntimeSnapshotBootstrapTerminalObservation {
            schema: 1,
            operation_id: termination.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id,
            provider_response_sha256: "c".repeat(64),
            terminated_at: "2026-09-29T00:00:00Z".into(),
            contact_deadline_exceeded: false,
        };
        let bound = db
            .bind_bootstrap_terminal_observation(&observation)
            .unwrap();
        assert_eq!(bound.phase, BootstrapTerminationPhase::Terminal);
        assert_eq!(
            db.bind_bootstrap_terminal_observation(&observation)
                .unwrap(),
            bound
        );
        assert!(db.conn.execute(
            "UPDATE runtime_snapshot_bootstrap_termination SET phase='reserved',observation_json=NULL WHERE operation_id=?1",
            [&termination.operation_id],
        ).is_err());
        assert!(matches!(
            db.claim_bootstrap_termination_attempt(&termination.operation_id)
                .unwrap(),
            BootstrapTerminationClaim::Terminal(_)
        ));
    }

    #[test]
    fn expired_uncontacted_deadline_renews_without_reminting_termination() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (source_intent, occurrence) = source(&db, false);
        let mut original = termination(&source_intent, &occurrence);
        original.attempt_deadline_ms =
            i64::try_from(lillux::time::timestamp_millis()).unwrap() + 200;
        db.reserve_bootstrap_termination(&original).unwrap();
        lillux::time::sleep(lillux::time::Duration::from_millis(220));
        assert!(
            db.claim_bootstrap_termination_attempt(&original.operation_id)
                .is_err()
        );
        let mut renewed = original.clone();
        renewed.attempt_deadline_ms =
            i64::try_from(lillux::time::timestamp_millis()).unwrap() + 60_000;
        assert_eq!(
            renewed.derived_operation_id().unwrap(),
            original.operation_id
        );
        let record = db.reserve_bootstrap_termination(&renewed).unwrap();
        assert_eq!(record.intent, renewed);
        assert!(db.conn.execute(
            "UPDATE runtime_snapshot_bootstrap_termination SET intent_json=json_set(intent_json,'$.provider_id','other') WHERE operation_id=?1",
            [&original.operation_id],
        ).is_err());
        assert!(matches!(
            db.claim_bootstrap_termination_attempt(&original.operation_id)
                .unwrap(),
            BootstrapTerminationClaim::StartAttempt(_)
        ));
        assert!(db.conn.execute(
            "UPDATE runtime_snapshot_bootstrap_termination SET intent_json=json_set(intent_json,'$.attempt_deadline_ms',9999999999999) WHERE operation_id=?1",
            [&original.operation_id],
        ).is_err());
        let mut after_contact = renewed;
        after_contact.attempt_deadline_ms += 1_000;
        assert!(db.reserve_bootstrap_termination(&after_contact).is_err());
        assert!(matches!(
            db.claim_bootstrap_termination_attempt(&original.operation_id)
                .unwrap(),
            BootstrapTerminationClaim::Reconcile(_)
        ));
    }
}
