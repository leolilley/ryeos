//! One-use, precontact custody intent for Q guest-input scratch.
//!
//! The row precedes creation of its private child. It records why an original
//! descriptor may still exist after a failed preparation or daemon restart;
//! neither a row nor its diagnostic path can recreate that descriptor or
//! authorize provider contact. Cleanup transitions are deliberately separate.

use super::*;
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_qualification_guest_scratch (
    placement_thread_id TEXT PRIMARY KEY,
    qualification_operation_id TEXT NOT NULL REFERENCES runtime_snapshot_qualification(operation_id),
    phase TEXT NOT NULL CHECK (phase IN ('reserved','created','cleanup_intent','removed','closed')),
    record_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (updated_at_ms >= created_at_ms),
    CHECK (length(CAST(record_json AS BLOB)) BETWEEN 1 AND 32768)
);
CREATE TRIGGER external_qualification_guest_scratch_transition_guard
BEFORE UPDATE ON external_qualification_guest_scratch
WHEN NEW.placement_thread_id!=OLD.placement_thread_id
 OR NEW.qualification_operation_id!=OLD.qualification_operation_id
 OR NEW.created_at_ms!=OLD.created_at_ms
 OR json_remove(NEW.record_json,'$.phase','$.cleanup_basis','$.removed_at_ms','$.closed_at_ms','$.updated_at_ms')
    != json_remove(OLD.record_json,'$.phase','$.cleanup_basis','$.removed_at_ms','$.closed_at_ms','$.updated_at_ms')
 OR NOT ((OLD.phase='reserved' AND NEW.phase='created')
      OR (OLD.phase IN ('reserved','created') AND NEW.phase='cleanup_intent')
      OR (OLD.phase='cleanup_intent' AND NEW.phase='removed')
      OR (OLD.phase='removed' AND NEW.phase='closed'))
BEGIN SELECT RAISE(ABORT, 'qualification guest scratch is one-use'); END;
CREATE TRIGGER external_qualification_guest_scratch_no_delete
BEFORE DELETE ON external_qualification_guest_scratch
BEGIN SELECT RAISE(ABORT, 'qualification guest scratch requires exact cleanup'); END;
CREATE TRIGGER external_qualification_guest_scratch_allocation_guard
BEFORE INSERT ON external_execution_allocation
WHEN json_extract(NEW.reservation_json,'$.owner.kind')='qualification_candidate'
 AND EXISTS(SELECT 1 FROM external_qualification_guest_scratch s
    WHERE s.placement_thread_id=NEW.placement_thread_id AND s.phase!='created')
BEGIN SELECT RAISE(ABORT, 'qualification guest scratch cleanup blocks allocation'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationGuestScratchPhase {
    Reserved,
    Created,
    CleanupIntent,
    Removed,
    Closed,
}

impl QualificationGuestScratchPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Created => "created",
            Self::CleanupIntent => "cleanup_intent",
            Self::Removed => "removed",
            Self::Closed => "closed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "created" => Ok(Self::Created),
            "cleanup_intent" => Ok(Self::CleanupIntent),
            "removed" => Ok(Self::Removed),
            "closed" => Ok(Self::Closed),
            _ => anyhow::bail!("Q guest scratch has an invalid phase"),
        }
    }
}

/// A local cleanup basis, retained before deleting any guest-input bytes.
/// It is not a product claim. Every transition rechecks the immutable Q
/// allocation and the settled provider/local owners in the same transaction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QualificationGuestScratchCleanupBasis {
    NoAllocation,
    SettledAllocation {
        allocation_request_digest: String,
        settled_phase: ExternalAllocationPhase,
        occurrence_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationGuestScratchRecord {
    pub placement_thread_id: String,
    pub qualification_operation_id: String,
    pub coordinate: ConsumerRuntimeVerificationCoordinate,
    pub qualification_occurrence_id: String,
    pub owner_principal: String,
    pub daemon_generation_id: String,
    pub program_digest: String,
    /// Diagnostic only. Physical custody is the original pinned parent/root.
    pub scratch_root: String,
    pub phase: QualificationGuestScratchPhase,
    pub cleanup_basis: Option<QualificationGuestScratchCleanupBasis>,
    pub removed_at_ms: Option<i64>,
    pub closed_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl QualificationGuestScratchRecord {
    pub fn validate(&self) -> Result<()> {
        self.coordinate.validate()?;
        ensure!(
            self.placement_thread_id
                == ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
                    &self.coordinate,
                    &self.qualification_operation_id,
                )?,
            "Q guest scratch changed its placement key"
        );
        for (label, value) in [
            ("operation", &self.qualification_operation_id),
            ("program", &self.program_digest),
        ] {
            validate_sha256(label, value)?;
        }
        for (label, value) in [
            ("occurrence", &self.qualification_occurrence_id),
            ("owner", &self.owner_principal),
            ("daemon", &self.daemon_generation_id),
            ("scratch", &self.scratch_root),
        ] {
            validate_bounded_runtime_text(label, value, 4096)?;
        }
        let path = std::path::Path::new(&self.scratch_root);
        ensure!(
            path.is_absolute()
                && path.file_name().and_then(|name| name.to_str())
                    == Some(self.placement_thread_id.as_str())
                && path.parent().and_then(std::path::Path::file_name)
                    == Some(std::ffi::OsStr::new("qualification-guest-inputs")),
            "Q guest scratch changed its private child coordinate"
        );
        ensure!(
            self.created_at_ms > 0 && self.updated_at_ms >= self.created_at_ms,
            "Q guest scratch has invalid journal time"
        );
        if let Some(QualificationGuestScratchCleanupBasis::SettledAllocation {
            allocation_request_digest,
            settled_phase,
            occurrence_id,
        }) = &self.cleanup_basis
        {
            validate_sha256("Q scratch allocation request", allocation_request_digest)?;
            ensure!(
                settled_phase.is_settled(),
                "Q scratch cleanup basis has an unsettled allocation"
            );
            if let Some(occurrence_id) = occurrence_id {
                validate_bounded_runtime_text("Q scratch provider occurrence", occurrence_id, 256)?;
            }
        }
        match self.phase {
            QualificationGuestScratchPhase::Reserved | QualificationGuestScratchPhase::Created => {
                ensure!(
                    self.cleanup_basis.is_none()
                        && self.removed_at_ms.is_none()
                        && self.closed_at_ms.is_none(),
                    "Q guest scratch has premature cleanup evidence"
                );
            }
            QualificationGuestScratchPhase::CleanupIntent => {
                ensure!(
                    self.cleanup_basis.is_some()
                        && self.removed_at_ms.is_none()
                        && self.closed_at_ms.is_none(),
                    "Q guest scratch has no exact cleanup intent"
                );
            }
            QualificationGuestScratchPhase::Removed => {
                ensure!(
                    self.cleanup_basis.is_some()
                        && self
                            .removed_at_ms
                            .is_some_and(|time| time >= self.created_at_ms)
                        && self.closed_at_ms.is_none(),
                    "Q guest scratch has no physical removal receipt"
                );
            }
            QualificationGuestScratchPhase::Closed => {
                ensure!(
                    self.cleanup_basis.is_some()
                        && self
                            .removed_at_ms
                            .is_some_and(|time| time >= self.created_at_ms)
                        && self.closed_at_ms.is_some_and(|time| {
                            self.removed_at_ms.is_some_and(|removed| {
                                time >= removed && time <= self.updated_at_ms
                            })
                        }),
                    "Q guest scratch has no closed receipt"
                );
            }
        }
        Ok(())
    }
}

fn canonical(record: &QualificationGuestScratchRecord) -> Result<String> {
    record.validate()?;
    Ok(String::from_utf8(
        ryeos_external_execution_contract::canonical_json(record)?,
    )?)
}

fn read_scratch(
    conn: &Connection,
    placement: &str,
) -> Result<Option<QualificationGuestScratchRecord>> {
    let row: Option<(String, String, String, i64, i64)> = conn
        .query_row(
            "SELECT qualification_operation_id,phase,record_json,created_at_ms,updated_at_ms
             FROM external_qualification_guest_scratch WHERE placement_thread_id=?1",
            [placement],
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
    let Some((operation, phase, raw, created, updated)) = row else {
        return Ok(None);
    };
    let record: QualificationGuestScratchRecord = serde_json::from_str(&raw)?;
    record.validate()?;
    ensure!(
        record.placement_thread_id == placement
            && record.qualification_operation_id == operation
            && record.phase == QualificationGuestScratchPhase::parse(&phase)?
            && record.created_at_ms == created
            && record.updated_at_ms == updated
            && canonical(&record)? == raw,
        "Q guest scratch row changed its canonical owner"
    );
    Ok(Some(record))
}

pub(super) fn validate_scratches(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT placement_thread_id FROM external_qualification_guest_scratch ORDER BY placement_thread_id",
    )?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        let record = read_scratch(conn, &placement)?.context("Q guest scratch disappeared")?;
        if record.cleanup_basis.is_some() {
            require_cleanup_basis(conn, &record)?;
        }
    }
    Ok(())
}

/// A fresh Q allocation may use only the one Created input tree prepared for
/// its exact accepted root and live occurrence. This runs in the allocation
/// transaction, so cleanup cannot move the scratch row between this join and
/// the allocation INSERT. Process-local descriptor custody is checked by
/// StateStore before entering the transaction.
pub(super) fn require_created_for_allocation(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
) -> Result<()> {
    let ExternalAllocationOwner::QualificationCandidate(owner) = &reservation.owner else {
        anyhow::bail!("Q scratch allocation join requires a qualification owner");
    };
    let scratch = read_scratch(conn, &reservation.placement_thread_id)?
        .context("Q allocation has no original guest scratch intent")?;
    ensure!(
        scratch.phase == QualificationGuestScratchPhase::Created
            && scratch.coordinate == owner.coordinate
            && scratch.qualification_operation_id == owner.qualification_operation_id
            && scratch.qualification_occurrence_id == owner.qualification_occurrence.occurrence_id
            && scratch.owner_principal == owner.operator_principal
            && scratch.program_digest == owner.program.digest()?
            && reservation.admitted_capsule_hash == scratch.coordinate.accepted_capsule_hash,
        "Q allocation changed its Created original guest scratch owner"
    );
    Ok(())
}

fn require_cleanup_basis(
    conn: &Connection,
    record: &QualificationGuestScratchRecord,
) -> Result<()> {
    let basis = record
        .cleanup_basis
        .as_ref()
        .context("Q guest scratch has no cleanup basis")?;
    let allocation = read(conn, &record.placement_thread_id)?;
    match (basis, allocation.as_ref()) {
        (QualificationGuestScratchCleanupBasis::NoAllocation, None) => {
            for table in [
                "external_qualification_local_start",
                "external_qualification_worker_process",
                "external_execution_connector",
            ] {
                let sql =
                    format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE placement_thread_id=?1)");
                let exists: bool =
                    conn.query_row(&sql, [&record.placement_thread_id], |row| row.get(0))?;
                ensure!(!exists, "Q no-allocation scratch has a later local owner");
            }
        }
        (
            QualificationGuestScratchCleanupBasis::SettledAllocation {
                allocation_request_digest,
                settled_phase,
                occurrence_id,
            },
            Some(allocation),
        ) => {
            let ExternalAllocationOwner::QualificationCandidate(owner) =
                &allocation.reservation.owner
            else {
                anyhow::bail!("Q scratch cleanup changed its allocation kind");
            };
            ensure!(
                allocation.phase.is_settled()
                    && allocation.phase == *settled_phase
                    && allocation.reservation.request_digest == *allocation_request_digest
                    && allocation.reservation.admitted_capsule_hash
                        == record.coordinate.accepted_capsule_hash
                    && allocation
                        .occurrence
                        .as_ref()
                        .map(|item| item.occurrence_id.as_str())
                        == occurrence_id.as_deref()
                    && owner.coordinate == record.coordinate
                    && owner.qualification_operation_id == record.qualification_operation_id
                    && owner.qualification_occurrence.occurrence_id
                        == record.qualification_occurrence_id
                    && owner.operator_principal == record.owner_principal
                    && owner.program.digest()? == record.program_digest,
                "Q scratch cleanup changed its exact settled allocation"
            );
            validate_lifecycle_evidence(conn, allocation)?;
            let local =
                qualification_local_start::read_local_start(conn, &record.placement_thread_id)?;
            ensure!(
                local.as_ref().is_none_or(|item| {
                    item.phase == QualificationLocalStartPhase::Closed
                        && item.accepted_root_id == record.coordinate.accepted_root_id
                        && item.accepted_capsule_hash == record.coordinate.accepted_capsule_hash
                        && item.qualification_operation_id == record.qualification_operation_id
                        && item.qualification_occurrence_id == record.qualification_occurrence_id
                }),
                "Q scratch cleanup retains a live local Worker workspace"
            );
            let process_exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_qualification_worker_process
                 WHERE placement_thread_id=?1)",
                [&record.placement_thread_id],
                |row| row.get(0),
            )?;
            if process_exists {
                ensure!(
                    local.is_some(),
                    "Q scratch cleanup lost its local Worker start"
                );
                qualification_process::require_terminal_qualification_worker_process(
                    conn,
                    &allocation.reservation,
                )?;
            }
            let connector = connector::read_connector(conn, &record.placement_thread_id)?;
            ensure!(
                connector
                    .is_none_or(|item| { item.phase == connector::ExternalConnectorPhase::Closed }),
                "Q scratch cleanup retains a live local connector"
            );
        }
        _ => anyhow::bail!("Q scratch cleanup basis changed after its durable intent"),
    }
    Ok(())
}

fn transition(
    conn: &Connection,
    current: &QualificationGuestScratchRecord,
    next: &QualificationGuestScratchRecord,
) -> Result<QualificationGuestScratchRecord> {
    let before = canonical(current)?;
    let after = canonical(next)?;
    ensure!(
        conn.execute(
            "UPDATE external_qualification_guest_scratch
             SET phase=?3,record_json=?4,updated_at_ms=?5
             WHERE placement_thread_id=?1 AND record_json=?2 AND phase=?6",
            params![
                current.placement_thread_id,
                before,
                next.phase.as_str(),
                after,
                next.updated_at_ms,
                current.phase.as_str(),
            ],
        )? == 1,
        "Q guest scratch cleanup lost its one-use CAS"
    );
    read_scratch(conn, &current.placement_thread_id)?
        .context("Q guest scratch disappeared after cleanup CAS")
}

impl RuntimeDb {
    pub(crate) fn qualification_guest_scratch(
        &self,
        placement: &str,
    ) -> Result<Option<QualificationGuestScratchRecord>> {
        read_scratch(&self.conn, placement)
    }

    /// A retry may report transport completion only for the same one-use Q
    /// attempt after its original scratch is Closed and its normal provider,
    /// local and native terminal evidence still rejoins. Cleanup of a failed
    /// or cancelled attempt cannot be promoted to a completed transport.
    pub(crate) fn completed_qualification_guest_scratch_transport(
        &self,
        placement: &str,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
        qualification_operation_id: &str,
        owner_principal: &str,
        program_digest: &str,
    ) -> Result<bool> {
        let Some(record) = read_scratch(&self.conn, placement)? else {
            return Ok(false);
        };
        if record.phase != QualificationGuestScratchPhase::Closed {
            return Ok(false);
        }
        ensure!(
            record.coordinate == *coordinate
                && record.qualification_operation_id == qualification_operation_id
                && record.owner_principal == owner_principal
                && record.program_digest == program_digest
                && matches!(
                    record.cleanup_basis.as_ref(),
                    Some(QualificationGuestScratchCleanupBasis::SettledAllocation {
                        settled_phase: ExternalAllocationPhase::Terminated,
                        ..
                    })
                ),
            "closed Q scratch does not prove this exact completed transport"
        );
        require_cleanup_basis(&self.conn, &record)?;
        self.ensure_external_qualification_normal_terminal(placement)?;
        Ok(true)
    }

    /// Refuse every replay, including a prior Reserved row whose child may
    /// never have been created. No provider/allocation contact occurs here.
    pub(crate) fn reserve_qualification_guest_scratch(
        &self,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
        qualification_operation_id: &str,
        owner_principal: &str,
        program_digest: &str,
        scratch_root: &std::path::Path,
    ) -> Result<QualificationGuestScratchRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let placement = ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
            coordinate,
            qualification_operation_id,
        )?;
        ensure!(
            read_scratch(&tx, &placement)?.is_none() && read(&tx, &placement)?.is_none(),
            "Q guest scratch cannot replace an existing attempt"
        );
        super::super::restored_verifier_attempt::read_consumer_prerequisite(
            &tx,
            qualification_operation_id,
            owner_principal,
            coordinate,
        )?;
        let qualified =
            super::super::runtime_snapshot_qualification::read(&tx, qualification_operation_id)?
                .context("Q guest scratch lost its restored operation")?;
        ensure!(
            qualified.phase
                == super::super::runtime_snapshot_qualification::SnapshotQualificationPhase::OccurrenceBound
                && qualified.intent.owner_principal == owner_principal,
            "Q guest scratch has no operator-owned bound occurrence"
        );
        let occurrence = qualified
            .occurrence
            .context("Q guest scratch lost its bound occurrence")?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let record = QualificationGuestScratchRecord {
            placement_thread_id: placement.clone(),
            qualification_operation_id: qualification_operation_id.to_owned(),
            coordinate: coordinate.clone(),
            qualification_occurrence_id: occurrence.occurrence_id,
            owner_principal: owner_principal.to_owned(),
            daemon_generation_id: daemon_generation_id().to_owned(),
            program_digest: program_digest.to_owned(),
            scratch_root: scratch_root
                .to_str()
                .context("Q guest scratch path is not UTF-8")?
                .to_owned(),
            phase: QualificationGuestScratchPhase::Reserved,
            cleanup_basis: None,
            removed_at_ms: None,
            closed_at_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let raw = canonical(&record)?;
        ensure!(
            tx.execute(
                "INSERT INTO external_qualification_guest_scratch VALUES (?1,?2,'reserved',?3,?4,?4)",
                params![placement, qualification_operation_id, raw, now],
            )? == 1,
            "Q guest scratch lost its one-use reservation"
        );
        tx.commit()?;
        Ok(record)
    }

    pub(crate) fn mark_qualification_guest_scratch_created(
        &self,
        expected: &QualificationGuestScratchRecord,
    ) -> Result<QualificationGuestScratchRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_scratch(&tx, &expected.placement_thread_id)?
            .context("Q guest scratch lost its reservation")?;
        ensure!(
            current == *expected
                && current.phase == QualificationGuestScratchPhase::Reserved
                && current.daemon_generation_id == daemon_generation_id(),
            "Q guest scratch changed its original reservation"
        );
        let mut next = current.clone();
        next.phase = QualificationGuestScratchPhase::Created;
        next.updated_at_ms =
            i64::try_from(lillux::time::timestamp_millis())?.max(current.updated_at_ms);
        let raw = canonical(&next)?;
        ensure!(
            tx.execute(
                "UPDATE external_qualification_guest_scratch SET phase='created',record_json=?3,updated_at_ms=?4
                 WHERE placement_thread_id=?1 AND record_json=?2 AND phase='reserved'",
                params![current.placement_thread_id, canonical(&current)?, raw, next.updated_at_ms],
            )? == 1,
            "Q guest scratch creation lost its one-use CAS"
        );
        tx.commit()?;
        Ok(next)
    }

    /// Freeze exact no-allocation or settled-allocation evidence before any
    /// physical removal. A running local Worker or connector blocks cleanup.
    pub(crate) fn begin_qualification_guest_scratch_cleanup(
        &self,
        expected: &QualificationGuestScratchRecord,
    ) -> Result<QualificationGuestScratchRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_scratch(&tx, &expected.placement_thread_id)?
            .context("Q guest scratch cleanup lost its original row")?;
        ensure!(
            current == *expected
                && matches!(
                    current.phase,
                    QualificationGuestScratchPhase::Reserved
                        | QualificationGuestScratchPhase::Created
                )
                && current.daemon_generation_id == daemon_generation_id(),
            "Q guest scratch cleanup changed its current original custody"
        );
        let basis = match read(&tx, &current.placement_thread_id)? {
            None => QualificationGuestScratchCleanupBasis::NoAllocation,
            Some(allocation) => QualificationGuestScratchCleanupBasis::SettledAllocation {
                allocation_request_digest: allocation.reservation.request_digest,
                settled_phase: allocation.phase,
                occurrence_id: allocation.occurrence.map(|item| item.occurrence_id),
            },
        };
        let mut next = current.clone();
        next.phase = QualificationGuestScratchPhase::CleanupIntent;
        next.cleanup_basis = Some(basis);
        next.updated_at_ms =
            i64::try_from(lillux::time::timestamp_millis())?.max(current.updated_at_ms);
        require_cleanup_basis(&tx, &next)?;
        let next = transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    /// Record physical removal only after the same daemon proves absence
    /// through its original pinned parent. The DB transition rechecks the
    /// settled allocation/local owners; a path is never this proof.
    pub(crate) fn record_qualification_guest_scratch_removed(
        &self,
        expected: &QualificationGuestScratchRecord,
    ) -> Result<QualificationGuestScratchRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_scratch(&tx, &expected.placement_thread_id)?
            .context("Q guest scratch removal lost its original row")?;
        ensure!(
            current == *expected
                && current.phase == QualificationGuestScratchPhase::CleanupIntent
                && current.daemon_generation_id == daemon_generation_id(),
            "Q guest scratch removal changed its current cleanup intent"
        );
        require_cleanup_basis(&tx, &current)?;
        let mut next = current.clone();
        next.phase = QualificationGuestScratchPhase::Removed;
        next.updated_at_ms =
            i64::try_from(lillux::time::timestamp_millis())?.max(current.updated_at_ms);
        next.removed_at_ms = Some(next.updated_at_ms);
        let next = transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    pub(crate) fn close_qualification_guest_scratch(
        &self,
        expected: &QualificationGuestScratchRecord,
    ) -> Result<QualificationGuestScratchRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_scratch(&tx, &expected.placement_thread_id)?
            .context("Q guest scratch close lost its original row")?;
        ensure!(
            current == *expected
                && current.phase == QualificationGuestScratchPhase::Removed
                && current.daemon_generation_id == daemon_generation_id(),
            "Q guest scratch close changed its physical removal receipt"
        );
        require_cleanup_basis(&tx, &current)?;
        let mut next = current.clone();
        next.phase = QualificationGuestScratchPhase::Closed;
        next.updated_at_ms =
            i64::try_from(lillux::time::timestamp_millis())?.max(current.updated_at_ms);
        next.closed_at_ms = Some(next.updated_at_ms);
        let next = transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::sync::Arc;

    use crate::qualification_workspace_custody::{
        QualificationGuestScratchCustody, QualificationWorkspaceCustodyRegistry,
    };
    use crate::temp_dir_guard::TempDirGuard;

    fn fixture(
        path: &std::path::Path,
    ) -> Result<(
        Connection,
        lillux::PinnedDirectory,
        QualificationGuestScratchRecord,
    )> {
        let private =
            lillux::PinnedDirectory::open_or_create(&path.join("qualification-guest-inputs"))?;
        private.require_owner_private_directory()?;
        let operation = "1".repeat(64);
        let coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-accepted".into(),
            accepted_capsule_hash: "a".repeat(64),
            qualification_purpose_digest: "b".repeat(64),
            scenario_id: "hosted_worker".into(),
            scenario_source_digest: "c".repeat(64),
            subject_digest: "d".repeat(64),
            use_digest: "e".repeat(64),
            prerequisite_measurement_attempt_id: "f".repeat(64),
            prerequisite_measurement_observation_digest: "0".repeat(64),
        };
        let placement = ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
            &coordinate,
            &operation,
        )?;
        let record = QualificationGuestScratchRecord {
            placement_thread_id: placement.clone(),
            qualification_operation_id: operation.clone(),
            coordinate,
            qualification_occurrence_id: "original-occurrence".into(),
            owner_principal: "fp:original-operator".into(),
            daemon_generation_id: daemon_generation_id().into(),
            program_digest: "9".repeat(64),
            scratch_root: private.path().join(&placement).to_str().unwrap().into(),
            phase: QualificationGuestScratchPhase::Reserved,
            cleanup_basis: None,
            removed_at_ms: None,
            closed_at_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let conn = Connection::open(path.join("scratch.sqlite3"))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE runtime_snapshot_qualification(operation_id TEXT PRIMARY KEY);
             CREATE TABLE external_execution_allocation(
               placement_thread_id TEXT PRIMARY KEY,
               capacity_owner TEXT,
               reservation_json TEXT,
               phase TEXT,
               occurrence_json TEXT);
             CREATE TABLE external_qualification_local_start(placement_thread_id TEXT PRIMARY KEY);
             CREATE TABLE external_qualification_worker_process(placement_thread_id TEXT PRIMARY KEY);
             CREATE TABLE external_execution_connector(placement_thread_id TEXT PRIMARY KEY);",
        )?;
        conn.execute_batch(JOURNAL_SQL)?;
        conn.execute(
            "INSERT INTO runtime_snapshot_qualification VALUES (?1)",
            [&operation],
        )?;
        conn.execute(
            "INSERT INTO external_qualification_guest_scratch VALUES (?1,?2,'reserved',?3,1,1)",
            params![placement, operation, canonical(&record)?],
        )?;
        Ok((conn, private, record))
    }

    #[test]
    fn reserved_before_child_survives_restart_without_descriptor_adoption() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (conn, parent, record) = fixture(root.path())?;
        ensure!(
            parent
                .open_child_directory(OsStr::new(&record.placement_thread_id))?
                .is_none(),
            "Reserved intent unexpectedly created a child"
        );
        drop(conn);
        let reopened = Connection::open(root.path().join("scratch.sqlite3"))?;
        ensure!(
            read_scratch(&reopened, &record.placement_thread_id)? == Some(record.clone()),
            "Reserved intent disappeared across restart"
        );
        ensure!(
            QualificationWorkspaceCustodyRegistry::new()
                .borrow_guest_scratch(&record)
                .is_err(),
            "restart adopted a path as original descriptor custody"
        );
        Ok(())
    }

    #[test]
    fn created_before_allocation_and_lost_remove_ack_keep_original_custody() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (conn, parent, reserved) = fixture(root.path())?;
        let child = parent.create_child(OsStr::new(&reserved.placement_thread_id), 0o700)?;
        let guard = Arc::new(TempDirGuard::new_pinned(
            parent,
            reserved.placement_thread_id.clone().into(),
            child,
        ));
        let custody = Arc::new(QualificationGuestScratchCustody::new(&reserved, guard)?);
        let registry = QualificationWorkspaceCustodyRegistry::new();
        registry.retain_guest_scratch(Arc::clone(&custody))?;
        let mut created = reserved.clone();
        created.phase = QualificationGuestScratchPhase::Created;
        created.updated_at_ms = 2;
        transition(&conn, &reserved, &created)?;
        ensure!(
            read(&conn, &created.placement_thread_id)?.is_none()
                && Arc::ptr_eq(&registry.borrow_guest_scratch(&created)?, &custody),
            "Created scratch lost its original pre-allocation custody"
        );
        let mut intent = created.clone();
        intent.phase = QualificationGuestScratchPhase::CleanupIntent;
        intent.cleanup_basis = Some(QualificationGuestScratchCleanupBasis::NoAllocation);
        intent.updated_at_ms = 3;
        require_cleanup_basis(&conn, &intent)?;
        transition(&conn, &created, &intent)?;
        ensure!(
            conn.execute(
                "INSERT INTO external_execution_allocation(placement_thread_id,reservation_json)
                 VALUES (?1,'{\"owner\":{\"kind\":\"qualification_candidate\"}}')",
                [&intent.placement_thread_id],
            )
            .is_err(),
            "cleanup intent admitted a late Q allocation"
        );
        custody.guard().remove_now()?;
        custody.guard().prove_owned_scratch_removed()?;
        // The unlink committed but the journal update did not. Only this
        // daemon's retained original guard can retry the physical proof.
        ensure!(
            read_scratch(&conn, &intent.placement_thread_id)? == Some(intent.clone()),
            "lost removal ACK advanced the journal"
        );
        ensure!(
            QualificationWorkspaceCustodyRegistry::new()
                .borrow_guest_scratch(&intent)
                .is_err(),
            "restart adopted removed scratch from its diagnostic path"
        );
        let mut removed = intent.clone();
        removed.phase = QualificationGuestScratchPhase::Removed;
        removed.removed_at_ms = Some(4);
        removed.updated_at_ms = 4;
        transition(&conn, &intent, &removed)?;
        let mut closed = removed.clone();
        closed.phase = QualificationGuestScratchPhase::Closed;
        closed.closed_at_ms = Some(5);
        closed.updated_at_ms = 5;
        transition(&conn, &removed, &closed)?;
        registry.release_closed_guest_scratch(&closed)?;
        ensure!(
            registry.borrow_guest_scratch(&closed).is_err(),
            "Closed scratch retained active descriptor custody"
        );
        Ok(())
    }
}
