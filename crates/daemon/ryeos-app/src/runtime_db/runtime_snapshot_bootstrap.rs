//! One-attempt source-Sandbox bootstrap journal, separate from Worker
//! allocation, snapshot production, and restored-runtime qualification.
//! Pending or quarantined provider contact is never blindly retried.

use super::*;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::runtime_snapshot_bootstrap::{
    BOOTSTRAP_READINESS_PROTOCOL, RuntimeSnapshotBootstrapIntent,
    RuntimeSnapshotBootstrapOccurrence, RuntimeSnapshotBootstrapReadinessObservation,
    RuntimeSnapshotBootstrapReadinessRequest,
};

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE runtime_snapshot_bootstrap (
    operation_id TEXT PRIMARY KEY,
    intent_json TEXT NOT NULL,
    intent_digest TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    provider_group_id TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined','occurrence_bound','late_occurrence_bound','rejected_occurrence_bound')),
    occurrence_id TEXT,
    occurrence_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((occurrence_json IS NULL) = (occurrence_id IS NULL)),
    CHECK ((phase IN ('occurrence_bound','late_occurrence_bound','rejected_occurrence_bound')) =
           (occurrence_json IS NOT NULL AND occurrence_id IS NOT NULL)),
    -- Render does not attest a group in its Sandbox create response. One
    -- actual provider occurrence cannot acquire two intended group owners.
    UNIQUE(provider_id,occurrence_id)
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
    (OLD.phase='attempt_pending' AND NEW.phase IN ('quarantined','occurrence_bound','late_occurrence_bound','rejected_occurrence_bound')) OR
    (OLD.phase='quarantined' AND NEW.phase IN ('late_occurrence_bound','rejected_occurrence_bound'))
)
BEGIN SELECT RAISE(ABORT, 'snapshot bootstrap phase cannot move backward'); END;
CREATE TABLE runtime_snapshot_bootstrap_authority (
    operation_id TEXT PRIMARY KEY REFERENCES runtime_snapshot_bootstrap(operation_id),
    binding_json TEXT NOT NULL
);
CREATE TRIGGER runtime_snapshot_bootstrap_authority_immutable
BEFORE UPDATE ON runtime_snapshot_bootstrap_authority
BEGIN SELECT RAISE(ABORT, 'bootstrap cleanup authority is immutable'); END;
CREATE TRIGGER runtime_snapshot_bootstrap_authority_no_delete
BEFORE DELETE ON runtime_snapshot_bootstrap_authority
BEGIN SELECT RAISE(ABORT, 'bootstrap cleanup authority is retained'); END;
CREATE TABLE runtime_snapshot_bootstrap_artifacts (
    operation_id TEXT PRIMARY KEY REFERENCES runtime_snapshot_bootstrap(operation_id),
    root_hash TEXT NOT NULL CHECK (length(root_hash)=64)
);
CREATE TRIGGER runtime_snapshot_bootstrap_artifacts_immutable
BEFORE UPDATE ON runtime_snapshot_bootstrap_artifacts
BEGIN SELECT RAISE(ABORT, 'bootstrap lifecycle artifact root is immutable'); END;
CREATE TRIGGER runtime_snapshot_bootstrap_artifacts_no_delete
BEFORE DELETE ON runtime_snapshot_bootstrap_artifacts
BEGIN SELECT RAISE(ABORT, 'bootstrap lifecycle artifact root is retained'); END;
CREATE TABLE runtime_snapshot_bootstrap_readiness (
    operation_id TEXT PRIMARY KEY REFERENCES runtime_snapshot_bootstrap(operation_id),
    observation_json TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL
);
CREATE TRIGGER runtime_snapshot_bootstrap_readiness_immutable
BEFORE UPDATE ON runtime_snapshot_bootstrap_readiness
BEGIN SELECT RAISE(ABORT, 'bootstrap readiness is immutable'); END;
CREATE TRIGGER runtime_snapshot_bootstrap_readiness_no_delete
BEFORE DELETE ON runtime_snapshot_bootstrap_readiness
BEGIN SELECT RAISE(ABORT, 'bootstrap readiness is retained'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotBootstrapPhase {
    Reserved,
    AttemptPending,
    Quarantined,
    OccurrenceBound,
    LateOccurrenceBound,
    RejectedOccurrenceBound,
}

impl SnapshotBootstrapPhase {
    fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            "occurrence_bound" => Self::OccurrenceBound,
            "late_occurrence_bound" => Self::LateOccurrenceBound,
            "rejected_occurrence_bound" => Self::RejectedOccurrenceBound,
            _ => anyhow::bail!("snapshot bootstrap has an invalid phase"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotBootstrapRecord {
    pub intent: RuntimeSnapshotBootstrapIntent,
    pub phase: SnapshotBootstrapPhase,
    /// The exact protected historical lifecycle closure. Its absence permits
    /// reservation but never an attempt or provider contact.
    pub lifecycle_artifacts_root: Option<String>,
    pub occurrence: Option<RuntimeSnapshotBootstrapOccurrence>,
    pub readiness: Option<RuntimeSnapshotBootstrapReadinessObservation>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotBootstrapAttemptClaim {
    StartAttempt(SnapshotBootstrapRecord),
    Reconcile(SnapshotBootstrapRecord),
    OccurrenceBound(SnapshotBootstrapRecord),
    LateOccurrenceBound(SnapshotBootstrapRecord),
    RejectedOccurrenceBound(SnapshotBootstrapRecord),
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
            SnapshotBootstrapPhase::OccurrenceBound
                | SnapshotBootstrapPhase::LateOccurrenceBound
                | SnapshotBootstrapPhase::RejectedOccurrenceBound
        ) == occurrence.is_some()
            && occurrence
                .as_ref()
                .map(|value| value.occurrence_id.as_str())
                == occurrence_id.as_deref()
            && created > 0
            && updated >= created
            && (phase != SnapshotBootstrapPhase::OccurrenceBound
                || occurrence.as_ref().is_some_and(|value| {
                    value.creation_attributes_verified
                        && !value.contact_deadline_exceeded
                        && updated < intent.attempt_deadline_ms
                }))
            && (phase != SnapshotBootstrapPhase::RejectedOccurrenceBound
                || occurrence
                    .as_ref()
                    .is_some_and(|value| !value.creation_attributes_verified)),
        "bootstrap phase or timestamps contradict retained evidence"
    );
    let readiness_raw: Option<(String, i64)> = conn
        .query_row(
            "SELECT observation_json,observed_at_ms FROM runtime_snapshot_bootstrap_readiness WHERE operation_id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let readiness = readiness_raw
        .map(|(raw, observed_at_ms)| {
            ensure!(
                raw.len() <= 1024
                    && observed_at_ms >= created
                    && matches!(phase, SnapshotBootstrapPhase::OccurrenceBound),
                "bootstrap readiness lacks a timely bound source"
            );
            let request = RuntimeSnapshotBootstrapReadinessRequest {
                protocol: BOOTSTRAP_READINESS_PROTOCOL.into(),
                intent: intent.clone(),
                occurrence: occurrence
                    .clone()
                    .context("ready source has no occurrence")?,
                provider_spec_digest: intent.provider_spec_digest.clone(),
            };
            let value: RuntimeSnapshotBootstrapReadinessObservation = serde_json::from_str(&raw)?;
            value.validate_for(&request)?;
            ensure!(
                canonical(&value)? == raw
                    && value.observed_at_ms <= observed_at_ms
                    && observed_at_ms.saturating_sub(value.observed_at_ms) <= 300_000
                    && occurrence
                        .as_ref()
                        .and_then(
                            |source| source.provider_creation_observation["created_at"].as_str()
                        )
                        == Some(value.observed_created_at.as_str()),
                "bootstrap readiness changed retained creation evidence"
            );
            Ok(value)
        })
        .transpose()?;
    let lifecycle_artifacts_root: Option<String> = conn
        .query_row(
            "SELECT root_hash FROM runtime_snapshot_bootstrap_artifacts WHERE operation_id=?1",
            [operation_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(root) = &lifecycle_artifacts_root {
        require_lifecycle_root(root)?;
    }
    ensure!(
        phase == SnapshotBootstrapPhase::Reserved || lifecycle_artifacts_root.is_some(),
        "contacted bootstrap lost its retained lifecycle closure"
    );
    Ok(Some(SnapshotBootstrapRecord {
        intent,
        phase,
        lifecycle_artifacts_root,
        occurrence,
        readiness,
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
    let mut statement = conn.prepare(
        "SELECT operation_id,binding_json FROM runtime_snapshot_bootstrap_authority ORDER BY operation_id",
    )?;
    let bindings = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (operation_id, raw) in bindings {
        validate_retained_binding(conn, &operation_id, &raw)?;
    }
    let mut statement = conn.prepare(
        "SELECT operation_id,root_hash FROM runtime_snapshot_bootstrap_artifacts ORDER BY operation_id",
    )?;
    let roots = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (operation_id, root) in roots {
        require_lifecycle_root(&root)?;
        ensure!(
            read(conn, &operation_id)?
                .as_ref()
                .and_then(|record| record.lifecycle_artifacts_root.as_deref())
                == Some(root.as_str()),
            "retained lifecycle root lost bootstrap operation"
        );
    }
    Ok(())
}

fn require_lifecycle_root(root: &str) -> Result<()> {
    ensure!(
        root.len() == 64
            && root
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "bootstrap lifecycle root is not a lowercase SHA-256 digest"
    );
    Ok(())
}

fn validate_retained_binding(
    conn: &Connection,
    operation_id: &str,
    raw: &str,
) -> Result<crate::node_config::sections::runtime_snapshot_production::RetainedRuntimeSnapshotProductionBinding>{
    ensure!(
        raw.len() <= crate::node_document::MAX_ITEM_BYTES as usize + 16 * 1024,
        "retained bootstrap authority exceeds bound"
    );
    let binding: crate::node_config::sections::runtime_snapshot_production::RetainedRuntimeSnapshotProductionBinding =
        serde_json::from_str(raw)?;
    binding.validate()?;
    let record =
        read(conn, operation_id)?.context("retained bootstrap authority lost operation")?;
    ensure!(
        binding.digest() == record.intent.production_binding_digest && canonical(&binding)? == raw,
        "retained bootstrap authority changed signed generation"
    );
    Ok(binding)
}

impl RuntimeDb {
    /// Operational CAS roots for every retained bootstrap. A completed
    /// durable upload drops its staging roots, so SQLite must keep the exact
    /// lifecycle closure reachable through provider contact and cleanup.
    pub(crate) fn snapshot_bootstrap_lifecycle_roots(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT operation_id,root_hash FROM runtime_snapshot_bootstrap_artifacts ORDER BY operation_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut roots = Vec::with_capacity(rows.len());
        for (operation_id, root) in rows {
            require_lifecycle_root(&root)?;
            ensure!(
                read(&self.conn, &operation_id)?
                    .as_ref()
                    .and_then(|record| record.lifecycle_artifacts_root.as_deref())
                    == Some(root.as_str()),
                "bootstrap lifecycle CAS root lost its authoritative journal"
            );
            roots.push(root);
        }
        Ok(roots)
    }

    pub(crate) fn retained_snapshot_bootstrap_binding(
        &self,
        operation_id: &str,
    ) -> Result<Option<crate::node_config::sections::runtime_snapshot_production::RetainedRuntimeSnapshotProductionBinding>>{
        let raw: Option<String> = self.conn.query_row(
            "SELECT binding_json FROM runtime_snapshot_bootstrap_authority WHERE operation_id=?1",
            [operation_id], |row| row.get(0),
        ).optional()?;
        raw.map(|raw| validate_retained_binding(&self.conn, operation_id, &raw))
            .transpose()
    }

    pub fn bind_snapshot_bootstrap_readiness(
        &self,
        observation: &RuntimeSnapshotBootstrapReadinessObservation,
    ) -> Result<SnapshotBootstrapRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &observation.operation_id)?
            .context("bootstrap readiness has no retained source")?;
        ensure!(
            record.phase == SnapshotBootstrapPhase::OccurrenceBound,
            "bootstrap readiness cannot promote cleanup-only source"
        );
        let request = RuntimeSnapshotBootstrapReadinessRequest {
            protocol: BOOTSTRAP_READINESS_PROTOCOL.into(),
            intent: record.intent.clone(),
            occurrence: record
                .occurrence
                .clone()
                .context("bound source has no occurrence")?,
            provider_spec_digest: record.intent.provider_spec_digest.clone(),
        };
        observation.validate_for(&request)?;
        ensure!(
            request.occurrence.provider_creation_observation["created_at"]
                == observation.observed_created_at,
            "bootstrap readiness changed original creation timestamp"
        );
        if let Some(existing) = &record.readiness {
            ensure!(
                existing == observation,
                "bootstrap readiness replay changed"
            );
            tx.commit()?;
            return Ok(record);
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            observation.observed_at_ms <= now
                && now.saturating_sub(observation.observed_at_ms) <= 300_000,
            "bootstrap readiness observation is stale or future-dated"
        );
        tx.execute(
            "INSERT INTO runtime_snapshot_bootstrap_readiness(operation_id,observation_json,observed_at_ms) VALUES(?1,?2,?3)",
            params![observation.operation_id, canonical(observation)?, now],
        )?;
        let current =
            read(&tx, &observation.operation_id)?.context("bootstrap readiness disappeared")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn snapshot_bootstrap_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<SnapshotBootstrapRecord>> {
        read(&self.conn, operation_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn reserve_snapshot_bootstrap(
        &self,
        intent: &RuntimeSnapshotBootstrapIntent,
    ) -> Result<SnapshotBootstrapRecord> {
        // Journal-only fixtures use a synthetic root. Production can attach
        // only a verified protected CAS root through the owner boundary.
        self.reserve_snapshot_bootstrap_inner(intent, None)?;
        self.attach_snapshot_bootstrap_artifacts(&intent.operation_id, &"a".repeat(64))
    }

    pub(crate) fn attach_snapshot_bootstrap_artifacts(
        &self,
        operation_id: &str,
        root_hash: &str,
    ) -> Result<SnapshotBootstrapRecord> {
        require_lifecycle_root(root_hash)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("bootstrap was not reserved")?;
        if let Some(existing) = &record.lifecycle_artifacts_root {
            ensure!(
                existing == root_hash,
                "bootstrap replay changed retained lifecycle closure"
            );
        } else {
            ensure!(
                record.phase == SnapshotBootstrapPhase::Reserved,
                "bootstrap lifecycle root cannot be attached after contact"
            );
            tx.execute(
                "INSERT INTO runtime_snapshot_bootstrap_artifacts(operation_id,root_hash) VALUES(?1,?2)",
                params![operation_id, root_hash],
            )?;
        }
        let current = read(&tx, operation_id)?.context("bootstrap artifact link vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub(crate) fn reserve_snapshot_bootstrap_with_binding(
        &self,
        intent: &RuntimeSnapshotBootstrapIntent,
        binding: &crate::node_config::sections::runtime_snapshot_production::RetainedRuntimeSnapshotProductionBinding,
    ) -> Result<SnapshotBootstrapRecord> {
        binding.validate()?;
        ensure!(
            binding.digest() == intent.production_binding_digest,
            "bootstrap source differs from retained producer binding"
        );
        self.reserve_snapshot_bootstrap_inner(intent, Some(binding))
    }

    fn reserve_snapshot_bootstrap_inner(
        &self,
        intent: &RuntimeSnapshotBootstrapIntent,
        binding: Option<&crate::node_config::sections::runtime_snapshot_production::RetainedRuntimeSnapshotProductionBinding>,
    ) -> Result<SnapshotBootstrapRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            ensure!(
                existing.intent == *intent,
                "bootstrap reservation replay changed"
            );
            if let Some(binding) = binding {
                let raw: String = tx.query_row(
                    "SELECT binding_json FROM runtime_snapshot_bootstrap_authority WHERE operation_id=?1",
                    [&intent.operation_id], |row| row.get(0),
                ).context("bootstrap replay lost retained cleanup authority")?;
                ensure!(
                    raw == canonical(binding)?,
                    "bootstrap replay changed retained cleanup authority"
                );
            }
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
        if let Some(binding) = binding {
            tx.execute(
                "INSERT INTO runtime_snapshot_bootstrap_authority(operation_id,binding_json) VALUES(?1,?2)",
                params![intent.operation_id, canonical(binding)?],
            )?;
        }
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
        ensure!(
            record.lifecycle_artifacts_root.is_some(),
            "bootstrap cannot contact provider without retained lifecycle artifacts"
        );
        match record.phase {
            SnapshotBootstrapPhase::OccurrenceBound => {
                tx.commit()?;
                return Ok(SnapshotBootstrapAttemptClaim::OccurrenceBound(record));
            }
            SnapshotBootstrapPhase::LateOccurrenceBound => {
                tx.commit()?;
                return Ok(SnapshotBootstrapAttemptClaim::LateOccurrenceBound(record));
            }
            SnapshotBootstrapPhase::RejectedOccurrenceBound => {
                tx.commit()?;
                return Ok(SnapshotBootstrapAttemptClaim::RejectedOccurrenceBound(
                    record,
                ));
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
            SnapshotBootstrapPhase::OccurrenceBound
                | SnapshotBootstrapPhase::LateOccurrenceBound
                | SnapshotBootstrapPhase::RejectedOccurrenceBound
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
        let phase = if !occurrence.creation_attributes_verified {
            "rejected_occurrence_bound"
        } else if late {
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
            creation_attributes_verified: true,
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
            creation_attributes_verified: true,
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
    fn phase_cannot_reopen_contact_and_occurrence_is_unique_across_intended_groups() {
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
            creation_attributes_verified: true,
            contact_deadline_exceeded: false,
        };
        db.bind_snapshot_bootstrap_occurrence(&occurrence).unwrap();
        let mut second = intent();
        second.source = RuntimeSnapshotSource::BundleMaterialization {
            materialization_attestation_hash: "b".repeat(64),
            source_coordinate_digest: "7".repeat(64),
            materialization_binding_digest: "8".repeat(64),
        };
        second.provider_group_id = "sbg-other".into();
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
            creation_attributes_verified: true,
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
    fn mismatched_creation_attributes_retain_cleanup_target_without_upload_authority() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let intent = intent();
        db.reserve_snapshot_bootstrap(&intent).unwrap();
        db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
            .unwrap();
        let occurrence = RuntimeSnapshotBootstrapOccurrence {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: "sbx-wrong-plan".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"plan":"wrong"}),
            creation_attributes_verified: false,
            contact_deadline_exceeded: false,
        };
        let record = db.bind_snapshot_bootstrap_occurrence(&occurrence).unwrap();
        assert_eq!(
            record.phase,
            SnapshotBootstrapPhase::RejectedOccurrenceBound
        );
        assert_eq!(record.occurrence, Some(occurrence));
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::RejectedOccurrenceBound(_)
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

    #[test]
    fn signed_cleanup_authority_survives_reopen_and_cannot_be_changed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let binding = crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding::test_fixture();
        let retained = binding.retained_generation().unwrap();
        let mut intent = intent();
        intent.production_binding_digest = retained.digest().to_owned();
        intent.operation_id = intent.derived_operation_id().unwrap();
        {
            let db = RuntimeDb::open(&path).unwrap();
            db.reserve_snapshot_bootstrap_with_binding(&intent, &retained)
                .unwrap();
            assert_eq!(
                canonical(
                    &db.retained_snapshot_bootstrap_binding(&intent.operation_id)
                        .unwrap()
                        .unwrap()
                )
                .unwrap(),
                canonical(&retained).unwrap()
            );
            assert!(db.conn.execute(
                "UPDATE runtime_snapshot_bootstrap_authority SET binding_json='{}' WHERE operation_id=?1",
                [&intent.operation_id],
            ).is_err());
            assert!(
                db.conn
                    .execute(
                        "DELETE FROM runtime_snapshot_bootstrap_authority WHERE operation_id=?1",
                        [&intent.operation_id],
                    )
                    .is_err()
            );
        }
        let db = RuntimeDb::open(&path).unwrap();
        let reopened = db
            .retained_snapshot_bootstrap_binding(&intent.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(canonical(&reopened).unwrap(), canonical(&retained).unwrap());
        assert_eq!(
            reopened.recovered_binding().unwrap().digest(),
            binding.digest()
        );
        db.reserve_snapshot_bootstrap_with_binding(&intent, &retained)
            .unwrap();
        let mut changed = intent;
        changed.production_binding_digest = "0".repeat(64);
        changed.operation_id = changed.derived_operation_id().unwrap();
        assert!(
            db.reserve_snapshot_bootstrap_with_binding(&changed, &retained)
                .is_err()
        );
    }

    #[test]
    fn provider_contact_requires_immutable_lifecycle_root_across_reopen() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let binding = crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding::test_fixture();
        let retained = binding.retained_generation().unwrap();
        let mut intent = intent();
        intent.production_binding_digest = retained.digest().to_owned();
        intent.operation_id = intent.derived_operation_id().unwrap();
        let root = "b".repeat(64);
        {
            let db = RuntimeDb::open(&path).unwrap();
            let reserved = db
                .reserve_snapshot_bootstrap_with_binding(&intent, &retained)
                .unwrap();
            assert_eq!(reserved.lifecycle_artifacts_root, None);
            assert!(
                db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                    .is_err()
            );
            assert_eq!(
                db.snapshot_bootstrap_operation(&intent.operation_id)
                    .unwrap()
                    .unwrap()
                    .phase,
                SnapshotBootstrapPhase::Reserved
            );
            let linked = db
                .attach_snapshot_bootstrap_artifacts(&intent.operation_id, &root)
                .unwrap();
            assert_eq!(
                linked.lifecycle_artifacts_root.as_deref(),
                Some(root.as_str())
            );
            assert_eq!(
                db.snapshot_bootstrap_lifecycle_roots().unwrap(),
                vec![root.clone()]
            );
            assert!(
                db.attach_snapshot_bootstrap_artifacts(&intent.operation_id, &"c".repeat(64))
                    .is_err()
            );
            assert!(db.conn.execute(
                "UPDATE runtime_snapshot_bootstrap_artifacts SET root_hash=?2 WHERE operation_id=?1",
                params![intent.operation_id, "c".repeat(64)],
            ).is_err());
        }
        let db = RuntimeDb::open(&path).unwrap();
        assert_eq!(
            db.snapshot_bootstrap_lifecycle_roots().unwrap(),
            vec![root.clone()]
        );
        assert_eq!(
            db.snapshot_bootstrap_operation(&intent.operation_id)
                .unwrap()
                .unwrap()
                .lifecycle_artifacts_root
                .as_deref(),
            Some(root.as_str())
        );
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::StartAttempt(_)
        ));
        assert_eq!(
            db.attach_snapshot_bootstrap_artifacts(&intent.operation_id, &root)
                .unwrap()
                .lifecycle_artifacts_root
                .as_deref(),
            Some(root.as_str())
        );
    }

    #[test]
    fn running_readiness_is_immutable_and_survives_reopen() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let intent = intent();
        let occurrence = RuntimeSnapshotBootstrapOccurrence {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: "sbx-exact".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"created_at":"2026-09-29T00:00:00Z"}),
            creation_attributes_verified: true,
            contact_deadline_exceeded: false,
        };
        let observation = RuntimeSnapshotBootstrapReadinessObservation {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            provider_response_sha256: "b".repeat(64),
            observed_created_at: "2026-09-29T00:00:00Z".into(),
            observed_at_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap(),
        };
        {
            let db = RuntimeDb::open(&path).unwrap();
            db.reserve_snapshot_bootstrap(&intent).unwrap();
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap();
            db.bind_snapshot_bootstrap_occurrence(&occurrence).unwrap();
            let ready = db.bind_snapshot_bootstrap_readiness(&observation).unwrap();
            assert_eq!(ready.readiness, Some(observation.clone()));
            assert_eq!(
                db.bind_snapshot_bootstrap_readiness(&observation).unwrap(),
                ready
            );
            let mut changed = observation.clone();
            changed.provider_response_sha256 = "c".repeat(64);
            assert!(db.bind_snapshot_bootstrap_readiness(&changed).is_err());
            assert!(
                db.conn
                    .execute(
                        "DELETE FROM runtime_snapshot_bootstrap_readiness WHERE operation_id=?1",
                        [&intent.operation_id],
                    )
                    .is_err()
            );
        }
        let db = RuntimeDb::open(&path).unwrap();
        let retained = db
            .snapshot_bootstrap_operation(&intent.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(retained.readiness, Some(observation));
        assert!(matches!(
            db.claim_snapshot_bootstrap_attempt(&intent.operation_id)
                .unwrap(),
            SnapshotBootstrapAttemptClaim::OccurrenceBound(_)
        ));
    }
}
