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
    phase TEXT NOT NULL CHECK (phase IN ('reserved','created')),
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
 OR NOT (OLD.phase='reserved' AND NEW.phase='created')
BEGIN SELECT RAISE(ABORT, 'qualification guest scratch is one-use'); END;
CREATE TRIGGER external_qualification_guest_scratch_no_delete
BEFORE DELETE ON external_qualification_guest_scratch
BEGIN SELECT RAISE(ABORT, 'qualification guest scratch requires exact cleanup'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationGuestScratchPhase {
    Reserved,
    Created,
}

impl QualificationGuestScratchPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Created => "created",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "created" => Ok(Self::Created),
            _ => anyhow::bail!("Q guest scratch has an invalid phase"),
        }
    }
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
        read_scratch(conn, &placement)?.context("Q guest scratch disappeared")?;
    }
    Ok(())
}

impl RuntimeDb {
    pub(crate) fn qualification_guest_scratch(
        &self,
        placement: &str,
    ) -> Result<Option<QualificationGuestScratchRecord>> {
        read_scratch(&self.conn, placement)
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
}
