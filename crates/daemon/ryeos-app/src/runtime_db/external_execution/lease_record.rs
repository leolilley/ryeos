//! Lease record: the durable standing of one remote lease's readiness
//! handshake, from reservation to admission or refusal.
//!
//! Workload: a placed session whose lifecycle adapter created a remote lease.
//! The record is inserted in the same transaction that reserves the
//! allocation and its lease budget, so a lease never exists without it. It
//! then moves `reserved` -> `activated` (the supervisor was activated and the
//! lease's identity and ready deadline are bound) -> `admitted` (the adapter
//! accepted the guest's first two frames). Any state may move to `refused`,
//! which is terminal and carries its reason; a refused lease is never
//! admitted later. The first two guest frames are held here, un-journalled,
//! until the adapter's verdict decides whether they enter the channel journal.
//!
//! The row is never deleted: a refusal is durable evidence that outlives the
//! allocation, and a restart reads it to refuse a stream of a lease it has
//! already condemned.

use super::*;

/// The largest frame a record holds, in bytes.
pub(crate) const MAX_HELD_LEASE_FRAME_BYTES: usize = 64 * 1024;
/// The longest refusal reason a record stores, in bytes.
#[cfg(test)]
const MAX_REFUSAL_REASON_BYTES: usize = 1024;
/// The longest lease identifier a record stores, in bytes.
const MAX_LEASE_ID_BYTES: usize = 256;
/// The largest resume state a record stores, in bytes.
const MAX_RESUME_STATE_BYTES: usize = 64 * 1024;

pub(in crate::runtime_db) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_execution_lease (
    placement_thread_id TEXT PRIMARY KEY,
    admission_request_sha256 TEXT NOT NULL CHECK (length(admission_request_sha256) = 64),
    activation_span_ms INTEGER NOT NULL CHECK (activation_span_ms > 0),
    state TEXT NOT NULL CHECK (state IN ('reserved','activated','admitted','refused')),
    lease_id TEXT CHECK (lease_id IS NULL OR (length(lease_id) BETWEEN 1 AND 256)),
    activation_request_digest TEXT
        CHECK (activation_request_digest IS NULL OR length(activation_request_digest) = 64),
    signed_import_sha256 TEXT
        CHECK (signed_import_sha256 IS NULL OR length(signed_import_sha256) = 64),
    guest_package_sha256 TEXT
        CHECK (guest_package_sha256 IS NULL OR length(guest_package_sha256) = 64),
    ready_deadline_ms INTEGER CHECK (ready_deadline_ms IS NULL OR ready_deadline_ms > 0),
    held_ready_frame BLOB
        CHECK (held_ready_frame IS NULL OR length(held_ready_frame) BETWEEN 1 AND 65536),
    held_report_frame BLOB
        CHECK (held_report_frame IS NULL OR length(held_report_frame) BETWEEN 1 AND 65536),
    ready_frame_digest TEXT
        CHECK (ready_frame_digest IS NULL OR length(ready_frame_digest) = 64),
    report_frame_digest TEXT
        CHECK (report_frame_digest IS NULL OR length(report_frame_digest) = 64),
    resume_state_json TEXT
        CHECK (resume_state_json IS NULL OR length(CAST(resume_state_json AS BLOB)) BETWEEN 1 AND 65536),
    refusal_reason TEXT
        CHECK (refusal_reason IS NULL OR length(CAST(refusal_reason AS BLOB)) BETWEEN 1 AND 1024),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    CHECK (held_report_frame IS NULL OR held_ready_frame IS NOT NULL),
    CHECK (
        (state = 'reserved'
            AND lease_id IS NULL AND activation_request_digest IS NULL
            AND signed_import_sha256 IS NULL AND guest_package_sha256 IS NULL
            AND ready_deadline_ms IS NULL AND held_ready_frame IS NULL
            AND held_report_frame IS NULL AND ready_frame_digest IS NULL
            AND report_frame_digest IS NULL AND resume_state_json IS NULL
            AND refusal_reason IS NULL)
     OR (state = 'activated'
            AND lease_id IS NOT NULL AND activation_request_digest IS NOT NULL
            AND signed_import_sha256 IS NOT NULL AND guest_package_sha256 IS NOT NULL
            AND ready_deadline_ms IS NOT NULL AND ready_frame_digest IS NULL
            AND report_frame_digest IS NULL AND resume_state_json IS NULL
            AND refusal_reason IS NULL)
     OR (state = 'admitted'
            AND lease_id IS NOT NULL AND activation_request_digest IS NOT NULL
            AND signed_import_sha256 IS NOT NULL AND guest_package_sha256 IS NOT NULL
            AND ready_deadline_ms IS NOT NULL AND ready_frame_digest IS NOT NULL
            AND report_frame_digest IS NOT NULL AND resume_state_json IS NOT NULL
            AND refusal_reason IS NULL)
     OR (state = 'refused' AND refusal_reason IS NOT NULL
            AND (lease_id IS NULL) = (activation_request_digest IS NULL)
            AND (lease_id IS NULL) = (signed_import_sha256 IS NULL)
            AND (lease_id IS NULL) = (guest_package_sha256 IS NULL)
            AND (lease_id IS NULL) = (ready_deadline_ms IS NULL))
    )
);
CREATE INDEX idx_external_execution_lease_state_deadline
    ON external_execution_lease(state, ready_deadline_ms);
CREATE TRIGGER external_execution_lease_insert_guard
BEFORE INSERT ON external_execution_lease
WHEN NEW.state != 'reserved'
 OR NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=NEW.placement_thread_id AND a.phase='reserved')
BEGIN SELECT RAISE(ABORT, 'a lease record is inserted only reserved, with a fresh allocation'); END;
CREATE TRIGGER external_execution_lease_identity_guard
BEFORE UPDATE ON external_execution_lease
WHEN NEW.placement_thread_id != OLD.placement_thread_id
 OR NEW.admission_request_sha256 != OLD.admission_request_sha256
 OR NEW.activation_span_ms != OLD.activation_span_ms
 OR NEW.created_at_ms != OLD.created_at_ms
BEGIN SELECT RAISE(ABORT, 'a lease record keeps the identity it was reserved with'); END;
CREATE TRIGGER external_execution_lease_transition_guard
BEFORE UPDATE ON external_execution_lease
WHEN NOT (
    (OLD.state = 'reserved' AND NEW.state = 'activated')
 OR (OLD.state = 'activated' AND NEW.state = 'activated')
 OR (OLD.state = 'activated' AND NEW.state = 'admitted')
 OR (OLD.state IN ('reserved','activated','admitted') AND NEW.state = 'refused')
)
BEGIN SELECT RAISE(ABORT, 'a lease record moves reserved, activated, admitted, or to refused, and refused is terminal'); END;
CREATE TRIGGER external_execution_lease_binding_guard
BEFORE UPDATE ON external_execution_lease
WHEN OLD.state IN ('activated','admitted') AND NEW.state != 'refused'
 AND (NEW.lease_id IS NOT OLD.lease_id
   OR NEW.activation_request_digest IS NOT OLD.activation_request_digest
   OR NEW.signed_import_sha256 IS NOT OLD.signed_import_sha256
   OR NEW.guest_package_sha256 IS NOT OLD.guest_package_sha256
   OR NEW.ready_deadline_ms IS NOT OLD.ready_deadline_ms)
BEGIN SELECT RAISE(ABORT, 'an activated lease record keeps the identity it was activated with'); END;
CREATE TRIGGER external_execution_lease_held_frame_guard
BEFORE UPDATE ON external_execution_lease
WHEN (OLD.held_ready_frame IS NOT NULL AND NEW.held_ready_frame IS NOT OLD.held_ready_frame)
 OR (OLD.held_report_frame IS NOT NULL AND NEW.held_report_frame IS NOT OLD.held_report_frame)
 OR (OLD.state = 'reserved' AND NEW.state = 'activated'
     AND (NEW.held_ready_frame IS NOT NULL OR NEW.held_report_frame IS NOT NULL))
BEGIN SELECT RAISE(ABORT, 'a held lease frame is written once'); END;
CREATE TRIGGER external_execution_lease_refusal_guard
BEFORE UPDATE ON external_execution_lease
WHEN NEW.state = 'refused'
 AND (NEW.lease_id IS NOT OLD.lease_id
   OR NEW.activation_request_digest IS NOT OLD.activation_request_digest
   OR NEW.signed_import_sha256 IS NOT OLD.signed_import_sha256
   OR NEW.guest_package_sha256 IS NOT OLD.guest_package_sha256
   OR NEW.ready_deadline_ms IS NOT OLD.ready_deadline_ms
   OR NEW.held_ready_frame IS NOT OLD.held_ready_frame
   OR NEW.held_report_frame IS NOT OLD.held_report_frame
   OR NEW.ready_frame_digest IS NOT OLD.ready_frame_digest
   OR NEW.report_frame_digest IS NOT OLD.report_frame_digest
   OR NEW.resume_state_json IS NOT OLD.resume_state_json)
BEGIN SELECT RAISE(ABORT, 'a refusal changes only the record state and its reason'); END;
CREATE TRIGGER external_execution_lease_no_delete
BEFORE DELETE ON external_execution_lease
BEGIN SELECT RAISE(ABORT, 'a lease record is retained'); END;
"#;

/// Where a lease stands in its readiness handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalLeaseRecordState {
    Reserved,
    Activated,
    Admitted,
    Refused,
}

impl ExternalLeaseRecordState {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "activated" => Ok(Self::Activated),
            "admitted" => Ok(Self::Admitted),
            "refused" => Ok(Self::Refused),
            other => bail!("lease record holds unknown state {other:?}"),
        }
    }
}

/// What activation binds to a lease record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalLeaseActivation {
    pub(crate) lease_id: String,
    pub(crate) activation_request_digest: String,
    pub(crate) signed_import_sha256: String,
    pub(crate) guest_package_sha256: String,
    pub(crate) ready_deadline_ms: i64,
}

/// What admission records: the digests of the two frames the adapter judged
/// and the resume state it returned, opaque to this table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalLeaseAdmission {
    pub(crate) ready_frame_digest: String,
    pub(crate) report_frame_digest: String,
    pub(crate) resume_state_json: String,
}

/// One stored lease record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalLeaseRecord {
    pub(crate) placement_thread_id: String,
    pub(crate) admission_request_sha256: String,
    pub(crate) activation_span_ms: i64,
    pub(crate) state: ExternalLeaseRecordState,
    pub(crate) activation: Option<ExternalLeaseActivation>,
    pub(crate) held_ready_frame: Option<Vec<u8>>,
    pub(crate) held_report_frame: Option<Vec<u8>>,
    pub(crate) admission: Option<ExternalLeaseAdmission>,
    pub(crate) refusal_reason: Option<String>,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
}

const COLUMNS: &str = "placement_thread_id,admission_request_sha256,activation_span_ms,state,
    lease_id,activation_request_digest,signed_import_sha256,guest_package_sha256,
    ready_deadline_ms,held_ready_frame,held_report_frame,ready_frame_digest,
    report_frame_digest,resume_state_json,refusal_reason,created_at_ms,updated_at_ms";

fn record_from_row(row: &rusqlite::Row<'_>) -> Result<ExternalLeaseRecord> {
    let lease_id: Option<String> = row.get(4)?;
    let activation = match lease_id {
        None => None,
        Some(lease_id) => Some(ExternalLeaseActivation {
            lease_id,
            activation_request_digest: row.get(5)?,
            signed_import_sha256: row.get(6)?,
            guest_package_sha256: row.get(7)?,
            ready_deadline_ms: row.get(8)?,
        }),
    };
    let ready_frame_digest: Option<String> = row.get(11)?;
    let admission = match ready_frame_digest {
        None => None,
        Some(ready_frame_digest) => Some(ExternalLeaseAdmission {
            ready_frame_digest,
            report_frame_digest: row.get(12)?,
            resume_state_json: row.get(13)?,
        }),
    };
    Ok(ExternalLeaseRecord {
        placement_thread_id: row.get(0)?,
        admission_request_sha256: row.get(1)?,
        activation_span_ms: row.get(2)?,
        state: ExternalLeaseRecordState::parse(&row.get::<_, String>(3)?)?,
        activation,
        held_ready_frame: row.get(9)?,
        held_report_frame: row.get(10)?,
        admission,
        refusal_reason: row.get(14)?,
        created_at_ms: row.get(15)?,
        updated_at_ms: row.get(16)?,
    })
}

fn read(conn: &Connection, placement: &str) -> Result<Option<ExternalLeaseRecord>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM external_execution_lease WHERE placement_thread_id=?1"
    ))?;
    let mut rows = statement.query([placement])?;
    rows.next()?.map(record_from_row).transpose()
}

fn read_many(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<ExternalLeaseRecord>> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query(params)?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        records.push(record_from_row(row)?);
    }
    Ok(records)
}

/// Insert the record of the allocation just inserted, in its transaction.
/// The admitted request digest and the readiness window come from the
/// reservation itself, so the record is the reservation's, not the caller's.
pub(super) fn reserve(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
    now_ms: i64,
) -> Result<()> {
    let request = reservation
        .admitted_adapter_request()?
        .context("a lease record needs the reservation's admitted adapter request")?;
    let span = reservation
        .startup_deadline_ms
        .checked_sub(reservation.startup_started_at_ms)
        .context("lease record activation span overflows")?;
    conn.execute(
        "INSERT INTO external_execution_lease(placement_thread_id,admission_request_sha256,
            activation_span_ms,state,created_at_ms,updated_at_ms)
         VALUES(?1,?2,?3,'reserved',?4,?4)",
        params![
            reservation.placement_thread_id,
            lillux::sha256_hex(&request),
            span,
            now_ms
        ],
    )?;
    Ok(())
}

/// Move one record from `from` with `sql`, which must name `state=?2` in its
/// WHERE clause; refuses when the record is not in `from`.
fn transition(
    conn: &Connection,
    placement: &str,
    from: &str,
    sql: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<()> {
    let changed = conn.execute(sql, values)?;
    if changed == 1 {
        return Ok(());
    }
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM external_execution_lease WHERE placement_thread_id=?1",
            [placement],
            |row| row.get(0),
        )
        .optional()?;
    match state {
        None => bail!("lease record of {placement} does not exist"),
        Some(state) => bail!("lease record of {placement} is {state}, not {from}"),
    }
}

fn validate_frame(label: &str, frame: &[u8]) -> Result<()> {
    ensure!(
        !frame.is_empty() && frame.len() <= MAX_HELD_LEASE_FRAME_BYTES,
        "held lease {label} frame is empty or exceeds {MAX_HELD_LEASE_FRAME_BYTES} bytes"
    );
    Ok(())
}

impl ExternalLeaseActivation {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.lease_id.is_empty() && self.lease_id.len() <= MAX_LEASE_ID_BYTES,
            "lease id is empty or exceeds {MAX_LEASE_ID_BYTES} bytes"
        );
        validate_sha256("lease activation request", &self.activation_request_digest)?;
        validate_sha256("lease signed import", &self.signed_import_sha256)?;
        validate_sha256("lease guest package", &self.guest_package_sha256)?;
        ensure!(self.ready_deadline_ms > 0, "lease ready deadline must be positive");
        Ok(())
    }
}

impl ExternalLeaseAdmission {
    pub(in crate::runtime_db) fn validate(&self) -> Result<()> {
        validate_sha256("lease ready frame", &self.ready_frame_digest)?;
        validate_sha256("lease report frame", &self.report_frame_digest)?;
        ensure!(
            !self.resume_state_json.is_empty()
                && self.resume_state_json.len() <= MAX_RESUME_STATE_BYTES,
            "lease resume state is empty or exceeds {MAX_RESUME_STATE_BYTES} bytes"
        );
        Ok(())
    }
}

impl RuntimeDb {
    /// The lease record of `placement`, if it has one.
    pub(crate) fn external_lease_record(&self, placement: &str) -> Result<Option<ExternalLeaseRecord>> {
        self.conn.with_sql(|conn| read(conn, placement))
    }

    /// Reserved to activated: bind the lease's identity and its ready
    /// deadline.
    pub(crate) fn activate_external_lease_record(
        &self,
        placement: &str,
        activation: &ExternalLeaseActivation,
        now_ms: i64,
    ) -> Result<()> {
        activation.validate()?;
        self.conn.with_sql(|conn| {
            transition(
                conn,
                placement,
                "reserved",
                "UPDATE external_execution_lease
                    SET state='activated',lease_id=?3,activation_request_digest=?4,
                        signed_import_sha256=?5,guest_package_sha256=?6,ready_deadline_ms=?7,
                        updated_at_ms=MAX(?2,created_at_ms)
                  WHERE placement_thread_id=?1 AND state='reserved'",
                &[
                    &placement,
                    &now_ms,
                    &activation.lease_id,
                    &activation.activation_request_digest,
                    &activation.signed_import_sha256,
                    &activation.guest_package_sha256,
                    &activation.ready_deadline_ms,
                ],
            )
        })
    }

    /// Hold the guest's first frame on an activated record. A frame is held
    /// once; holding the identical frame again succeeds, a different one is
    /// refused.
    pub(crate) fn hold_external_lease_ready_frame(
        &self,
        placement: &str,
        frame: &[u8],
        now_ms: i64,
    ) -> Result<()> {
        validate_frame("ready", frame)?;
        self.hold_frame(placement, "held_ready_frame", frame, now_ms)
    }

    /// Hold the guest's second frame on an activated record that already
    /// holds the first.
    pub(crate) fn hold_external_lease_report_frame(
        &self,
        placement: &str,
        frame: &[u8],
        now_ms: i64,
    ) -> Result<()> {
        validate_frame("report", frame)?;
        self.hold_frame(placement, "held_report_frame", frame, now_ms)
    }

    fn hold_frame(&self, placement: &str, column: &str, frame: &[u8], now_ms: i64) -> Result<()> {
        self.conn.with_sql(|conn| {
            let record = read(conn, placement)?
                .with_context(|| format!("lease record of {placement} does not exist"))?;
            let held = if column == "held_ready_frame" {
                &record.held_ready_frame
            } else {
                &record.held_report_frame
            };
            if let Some(held) = held {
                ensure!(
                    held == frame,
                    "lease record of {placement} already holds a different frame"
                );
                return Ok(());
            }
            transition(
                conn,
                placement,
                "activated",
                &format!(
                    "UPDATE external_execution_lease
                        SET {column}=?3,updated_at_ms=MAX(?2,created_at_ms)
                      WHERE placement_thread_id=?1 AND state='activated'"
                ),
                &[&placement, &now_ms, &frame],
            )
        })
    }

    /// Activated to admitted: the adapter accepted the held frames.
    #[cfg(test)]
    pub(crate) fn admit_external_lease_record(
        &self,
        placement: &str,
        admission: &ExternalLeaseAdmission,
        now_ms: i64,
    ) -> Result<()> {
        admission.validate()?;
        self.conn.with_sql(|conn| {
            transition(
                conn,
                placement,
                "activated",
                "UPDATE external_execution_lease
                    SET state='admitted',ready_frame_digest=?3,report_frame_digest=?4,
                        resume_state_json=?5,updated_at_ms=MAX(?2,created_at_ms)
                  WHERE placement_thread_id=?1 AND state='activated'",
                &[
                    &placement,
                    &now_ms,
                    &admission.ready_frame_digest,
                    &admission.report_frame_digest,
                    &admission.resume_state_json,
                ],
            )
        })
    }

    /// Any state to refused, terminally. Refusing a refused record keeps its
    /// first reason and succeeds.
    #[cfg(test)]
    pub(crate) fn refuse_external_lease_record(
        &self,
        placement: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<()> {
        ensure!(
            !reason.is_empty() && reason.len() <= MAX_REFUSAL_REASON_BYTES,
            "lease refusal reason is empty or exceeds {MAX_REFUSAL_REASON_BYTES} bytes"
        );
        self.conn.with_sql(|conn| {
            if read(conn, placement)?
                .is_some_and(|record| record.state == ExternalLeaseRecordState::Refused)
            {
                return Ok(());
            }
            transition(
                conn,
                placement,
                "reserved, activated or admitted",
                "UPDATE external_execution_lease
                    SET state='refused',refusal_reason=?3,updated_at_ms=MAX(?2,created_at_ms)
                  WHERE placement_thread_id=?1 AND state!='refused'",
                &[&placement, &now_ms, &reason],
            )
        })
    }

    /// The unrefused records of leases still held whose session worker is not
    /// a worker of `current_generation`: the leases of an earlier daemon
    /// incarnation. Call this only at startup, before the node is ready, for
    /// the reason [`Self::predecessor_external_lease_placements`] gives.
    pub(crate) fn predecessor_external_lease_records(
        &self,
        current_generation: &str,
    ) -> Result<Vec<ExternalLeaseRecord>> {
        self.conn.with_sql(|conn| {
            read_many(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM external_execution_lease
                      WHERE state != 'refused' AND placement_thread_id IN (
                        SELECT a.placement_thread_id
                          FROM external_execution_lease_budget b
                          JOIN external_execution_allocation a
                            ON a.placement_thread_id=b.placement_thread_id
                         WHERE b.released_at_ms IS NULL
                           AND NOT EXISTS(SELECT 1 FROM worker_process w
                             WHERE w.placement_thread_id=a.placement_thread_id
                               AND w.worker_instance_id=
                                   json_extract(a.reservation_json,'$.owner.worker_instance_id')
                               AND w.boot_epoch=
                                   json_extract(a.reservation_json,'$.owner.worker_boot_epoch')
                               AND w.daemon_generation_id=?1))
                      ORDER BY placement_thread_id"
                ),
                [current_generation],
            )
        })
    }

    /// The activated records whose ready deadline is at or before `now_ms`.
    pub(crate) fn overdue_external_lease_records(
        &self,
        now_ms: i64,
    ) -> Result<Vec<ExternalLeaseRecord>> {
        self.conn.with_sql(|conn| {
            read_many(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM external_execution_lease
                      WHERE state='activated' AND ready_deadline_ms <= ?1
                      ORDER BY ready_deadline_ms,placement_thread_id"
                ),
                [now_ms],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{reservation, reserve as reserve_session};
    use super::*;

    fn open() -> (tempfile::TempDir, crate::runtime_db::test_native::TestRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let db =
            crate::runtime_db::test_native::TestRuntime::open(&dir.path().join("runtime.sqlite3"));
        (dir, db)
    }

    fn lease_charge() -> ExternalLeaseCharge {
        ExternalLeaseCharge {
            lifetime_seconds: 60,
            budget_weight: 1,
            max_concurrent_leases: 8,
            daily_lease_seconds: 8 * 3600,
        }
    }

    /// Reserve a leased allocation through the production transaction and
    /// return its placement thread.
    fn leased_real(db: &RuntimeDb, suffix: &str) -> (String, ExternalAllocationReservation) {
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let mut held = reservation(db, suffix);
        held.admitted_adapter_request_base64 = Some(
            ExternalAllocationReservation::encode_admitted_adapter_request(b"{\"lease\":1}")
                .unwrap(),
        );
        db.reserve_external_lease_allocation(&held, &binding, &lease_charge())
            .unwrap();
        (held.placement_thread_id.clone(), held)
    }

    /// A storage-level allocation, lease budget and reserved record, standing
    /// in for the reservation transaction so several leases can coexist under
    /// the fixture's capacity of one.
    fn leased(db: &RuntimeDb, suffix: &str) -> (String, ()) {
        let placement = format!("T-{suffix}");
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        db.conn
            .with_sql(|connection| {
                connection.execute(
                    "INSERT OR IGNORE INTO external_execution_binding_generation
                     VALUES(?1,?2,?3,1)",
                    params![
                        binding.digest(),
                        binding.capacity_owner(),
                        binding.canonical_json()?
                    ],
                )?;
                connection.execute(
                    "INSERT INTO external_execution_allocation
                     VALUES(?1,?2,?3,'reserved',NULL,1,1)",
                    params![
                        placement,
                        binding.capacity_owner(),
                        format!(
                            "{{\"owner\":{{\"worker_instance_id\":\"worker-{suffix}\",\"worker_boot_epoch\":1}}}}"
                        )
                    ],
                )?;
                super::lease_budget::reserve(connection, &placement, 1, &lease_charge())?;
                connection.execute(
                    "INSERT INTO external_execution_lease(placement_thread_id,
                        admission_request_sha256,activation_span_ms,state,created_at_ms,
                        updated_at_ms)
                     VALUES(?1,printf('%064d',1),1000,'reserved',1,1)",
                    [&placement],
                )?;
                Ok(())
            })
            .unwrap();
        (placement, ())
    }

    fn activation(tag: char) -> ExternalLeaseActivation {
        ExternalLeaseActivation {
            lease_id: format!("lease-{tag}"),
            activation_request_digest: tag.to_string().repeat(64),
            signed_import_sha256: "b".repeat(64),
            guest_package_sha256: "c".repeat(64),
            ready_deadline_ms: 5_000,
        }
    }

    fn admission() -> ExternalLeaseAdmission {
        ExternalLeaseAdmission {
            ready_frame_digest: "d".repeat(64),
            report_frame_digest: "e".repeat(64),
            resume_state_json: "{\"resume\":true}".into(),
        }
    }

    fn sql(db: &RuntimeDb, statement: &str) -> Result<usize> {
        db.conn
            .with_sql(|connection| Ok(connection.execute(statement, [])?))
    }

    fn state_of(db: &RuntimeDb, placement: &str) -> ExternalLeaseRecordState {
        db.external_lease_record(placement).unwrap().unwrap().state
    }

    fn activated(db: &RuntimeDb, suffix: &str) -> String {
        let (placement, _) = leased(db, suffix);
        db.activate_external_lease_record(&placement, &activation('a'), 10)
            .unwrap();
        placement
    }

    #[test]
    fn the_reservation_transaction_inserts_a_reserved_record_once() {
        let (_dir, db) = open();
        let (placement, held) = leased_real(&db, "one");
        let record = db.external_lease_record(&placement).unwrap().unwrap();
        assert_eq!(record.state, ExternalLeaseRecordState::Reserved);
        assert_eq!(
            record.admission_request_sha256,
            lillux::sha256_hex(b"{\"lease\":1}")
        );
        assert_eq!(
            record.activation_span_ms,
            held.startup_deadline_ms - held.startup_started_at_ms
        );
        assert!(record.activation.is_none() && record.admission.is_none());
        assert!(record.held_ready_frame.is_none() && record.refusal_reason.is_none());

        // Replay reserves nothing twice, and a session without a lease has
        // no record.
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        db.reserve_external_lease_allocation(&held, &binding, &lease_charge())
            .unwrap();
        db.cancel_external_allocation(&placement).unwrap();
        let plain = reservation(&db, "plain");
        reserve_session(&db, &plain).unwrap();
        assert!(db
            .external_lease_record(&plain.placement_thread_id)
            .unwrap()
            .is_none());
        assert_eq!(
            sql(&db, "DELETE FROM external_execution_lease WHERE 0").unwrap(),
            0
        );
    }

    #[test]
    fn a_record_walks_reserved_activated_admitted_with_its_frames_held() {
        let (_dir, db) = open();
        let (placement, _) = leased(&db, "walk");
        let bound = activation('a');
        db.activate_external_lease_record(&placement, &bound, 10).unwrap();
        db.hold_external_lease_ready_frame(&placement, b"ready", 11).unwrap();
        // Holding the same frame again passes; a different one does not.
        db.hold_external_lease_ready_frame(&placement, b"ready", 12).unwrap();
        assert!(db
            .hold_external_lease_ready_frame(&placement, b"other", 12)
            .is_err());
        db.hold_external_lease_report_frame(&placement, b"report", 13).unwrap();
        let record = db.external_lease_record(&placement).unwrap().unwrap();
        assert_eq!(record.state, ExternalLeaseRecordState::Activated);
        assert_eq!(record.activation, Some(bound.clone()));
        assert_eq!(record.held_ready_frame.as_deref(), Some(&b"ready"[..]));
        assert_eq!(record.held_report_frame.as_deref(), Some(&b"report"[..]));

        db.admit_external_lease_record(&placement, &admission(), 14).unwrap();
        let record = db.external_lease_record(&placement).unwrap().unwrap();
        assert_eq!(record.state, ExternalLeaseRecordState::Admitted);
        assert_eq!(record.admission, Some(admission()));
        assert_eq!(record.activation, Some(bound));
        assert_eq!(record.held_report_frame.as_deref(), Some(&b"report"[..]));
        assert!(record.updated_at_ms >= record.created_at_ms);
    }

    #[test]
    fn a_report_frame_needs_the_ready_frame_and_frames_are_bounded() {
        let (_dir, db) = open();
        let placement = activated(&db, "frames");
        assert!(db
            .hold_external_lease_report_frame(&placement, b"report", 11)
            .is_err());
        assert!(db.hold_external_lease_ready_frame(&placement, b"", 11).is_err());
        let big = vec![0u8; MAX_HELD_LEASE_FRAME_BYTES + 1];
        assert!(db.hold_external_lease_ready_frame(&placement, &big, 11).is_err());
        db.hold_external_lease_ready_frame(&placement, &big[..MAX_HELD_LEASE_FRAME_BYTES], 11)
            .unwrap();
    }

    #[test]
    fn transitions_off_the_allowed_path_are_refused() {
        let (_dir, db) = open();
        let (placement, _) = leased(&db, "path");
        // Reserved cannot be admitted, hold frames, or be admitted by raw SQL.
        assert!(db.admit_external_lease_record(&placement, &admission(), 10).is_err());
        assert!(db.hold_external_lease_ready_frame(&placement, b"r", 10).is_err());
        assert!(sql(
            &db,
            "UPDATE external_execution_lease SET state='admitted',lease_id='x',
                activation_request_digest=printf('%064d',1),signed_import_sha256=printf('%064d',1),
                guest_package_sha256=printf('%064d',1),ready_deadline_ms=1,
                ready_frame_digest=printf('%064d',1),report_frame_digest=printf('%064d',1),
                resume_state_json='{}'"
        )
        .is_err());
        // Reserved to reserved is no transition.
        assert!(sql(&db, "UPDATE external_execution_lease SET updated_at_ms=updated_at_ms+1").is_err());
        assert_eq!(state_of(&db, &placement), ExternalLeaseRecordState::Reserved);

        db.activate_external_lease_record(&placement, &activation('a'), 10).unwrap();
        // Activated cannot be activated again, even with the same inputs.
        assert!(db
            .activate_external_lease_record(&placement, &activation('a'), 11)
            .is_err());
        // Activated cannot go back to reserved.
        assert!(sql(&db, "UPDATE external_execution_lease SET state='reserved'").is_err());

        db.admit_external_lease_record(&placement, &admission(), 12).unwrap();
        // Admitted is not admitted twice, activated again, or held to.
        assert!(db.admit_external_lease_record(&placement, &admission(), 13).is_err());
        assert!(db.hold_external_lease_report_frame(&placement, b"r", 13).is_err());
        assert!(sql(&db, "UPDATE external_execution_lease SET state='activated'").is_err());
        assert!(sql(&db, "UPDATE external_execution_lease SET state='reserved'").is_err());
        assert!(sql(&db, "UPDATE external_execution_lease SET updated_at_ms=updated_at_ms+1").is_err());
        assert_eq!(state_of(&db, &placement), ExternalLeaseRecordState::Admitted);
    }

    #[test]
    fn a_refusal_comes_from_any_state_and_is_terminal() {
        let (_dir, db) = open();
        let (reserved, _) = leased(&db, "r");
        db.refuse_external_lease_record(&reserved, "never activated", 20).unwrap();
        let record = db.external_lease_record(&reserved).unwrap().unwrap();
        assert_eq!(record.state, ExternalLeaseRecordState::Refused);
        assert_eq!(record.refusal_reason.as_deref(), Some("never activated"));
        assert!(record.activation.is_none());

        let active = activated(&db, "s");
        db.hold_external_lease_ready_frame(&active, b"ready", 11).unwrap();
        db.refuse_external_lease_record(&active, "bad report", 21).unwrap();
        let record = db.external_lease_record(&active).unwrap().unwrap();
        assert_eq!(record.state, ExternalLeaseRecordState::Refused);
        assert_eq!(record.activation, Some(activation('a')));
        assert_eq!(record.held_ready_frame.as_deref(), Some(&b"ready"[..]));

        let (admitted, _) = leased(&db, "t");
        db.activate_external_lease_record(&admitted, &activation('a'), 10).unwrap();
        db.admit_external_lease_record(&admitted, &admission(), 12).unwrap();
        db.refuse_external_lease_record(&admitted, "controller restarted", 22).unwrap();
        let record = db.external_lease_record(&admitted).unwrap().unwrap();
        assert_eq!(record.admission, Some(admission()));

        // Terminal: nothing moves a refused record, and a second refusal
        // keeps the first reason.
        for placement in [&reserved, &active, &admitted] {
            db.refuse_external_lease_record(placement, "again", 30).unwrap();
            assert_eq!(
                db.external_lease_record(placement).unwrap().unwrap().refusal_reason.as_deref(),
                Some(match placement.as_str() {
                    p if p == reserved => "never activated",
                    p if p == active => "bad report",
                    _ => "controller restarted",
                })
            );
            assert!(db.activate_external_lease_record(placement, &activation('a'), 30).is_err());
            assert!(db.admit_external_lease_record(placement, &admission(), 30).is_err());
        }
        for target in ["reserved", "activated", "admitted"] {
            assert!(sql(
                &db,
                &format!("UPDATE external_execution_lease SET state='{target}'")
            )
            .is_err());
        }
        assert!(sql(&db, "UPDATE external_execution_lease SET refusal_reason='changed'").is_err());
        assert!(db.refuse_external_lease_record(&reserved, "", 30).is_err());
    }

    #[test]
    fn identity_and_written_values_are_immutable() {
        let (_dir, db) = open();
        let placement = activated(&db, "imm");
        db.hold_external_lease_ready_frame(&placement, b"ready", 11).unwrap();
        for change in [
            "placement_thread_id='T-other'",
            "admission_request_sha256=printf('%064d',9)",
            "activation_span_ms=activation_span_ms+1",
            "created_at_ms=created_at_ms+1",
            "lease_id='lease-z'",
            "activation_request_digest=printf('%064d',9)",
            "signed_import_sha256=printf('%064d',9)",
            "guest_package_sha256=printf('%064d',9)",
            "ready_deadline_ms=ready_deadline_ms+1",
            "held_ready_frame=x'00'",
            "held_ready_frame=NULL",
        ] {
            assert!(
                sql(&db, &format!("UPDATE external_execution_lease SET {change}")).is_err(),
                "{change}"
            );
        }
        db.admit_external_lease_record(&placement, &admission(), 12).unwrap();
        for change in [
            "lease_id='lease-z'",
            "ready_deadline_ms=1",
            "ready_frame_digest=printf('%064d',9)",
        ] {
            assert!(
                sql(&db, &format!("UPDATE external_execution_lease SET {change}")).is_err(),
                "{change}"
            );
        }
        // A refusal rewrites nothing else.
        assert!(sql(
            &db,
            "UPDATE external_execution_lease SET state='refused',refusal_reason='x',lease_id='q'"
        )
        .is_err());
    }

    #[test]
    fn a_record_is_never_deleted() {
        let (_dir, db) = open();
        let (placement, _) = leased(&db, "del");
        assert!(sql(&db, "DELETE FROM external_execution_lease").is_err());
        db.refuse_external_lease_record(&placement, "done", 20).unwrap();
        assert!(sql(&db, "DELETE FROM external_execution_lease").is_err());
        sql(
            &db,
            "UPDATE external_execution_allocation SET phase='no_contact',updated_at_ms=100",
        )
        .unwrap();
        assert!(sql(&db, "DELETE FROM external_execution_lease").is_err());
        assert!(db.external_lease_record(&placement).unwrap().is_some());
    }

    #[test]
    fn a_record_is_inserted_only_reserved_and_only_with_a_reserved_allocation() {
        let (_dir, db) = open();
        // No allocation at all.
        let insert = |placement: &str, state: &str| {
            sql(
                &db,
                &format!(
                    "INSERT INTO external_execution_lease(placement_thread_id,
                        admission_request_sha256,activation_span_ms,state,created_at_ms,
                        updated_at_ms)
                     VALUES('{placement}',printf('%064d',1),1000,'{state}',1,1)"
                ),
            )
        };
        assert!(insert("T-missing", "reserved").is_err());

        // An allocation that has left the reserved phase.
        let plain = reservation(&db, "plain");
        reserve_session(&db, &plain).unwrap();
        db.cancel_external_allocation(&plain.placement_thread_id).unwrap();
        assert!(insert(&plain.placement_thread_id, "reserved").is_err());

        // A fresh allocation takes a reserved record only.
        let fresh = reservation(&db, "fresh");
        reserve_session(&db, &fresh).unwrap();
        assert!(insert(&fresh.placement_thread_id, "refused").is_err());
        assert!(insert(&fresh.placement_thread_id, "activated").is_err());
        insert(&fresh.placement_thread_id, "reserved").unwrap();
        assert!(insert(&fresh.placement_thread_id, "reserved").is_err());
        assert!(db
            .external_lease_record(&fresh.placement_thread_id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn the_states_hold_their_columns() {
        let (_dir, db) = open();
        let (placement, _) = leased(&db, "cols");
        // A reserved row carries nothing, and an activated one needs all of
        // its identity.
        assert!(sql(&db, "UPDATE external_execution_lease SET state='activated',lease_id='x'").is_err());
        assert!(sql(&db, "UPDATE external_execution_lease SET refusal_reason='x'").is_err());
        assert!(sql(&db, "UPDATE external_execution_lease SET lease_id=''").is_err());
        // Out-of-bound activation inputs are refused before the row.
        let mut bad = activation('a');
        bad.signed_import_sha256 = "short".into();
        assert!(db.activate_external_lease_record(&placement, &bad, 10).is_err());
        bad = activation('a');
        bad.lease_id = String::new();
        assert!(db.activate_external_lease_record(&placement, &bad, 10).is_err());
        bad = activation('a');
        bad.ready_deadline_ms = 0;
        assert!(db.activate_external_lease_record(&placement, &bad, 10).is_err());
        assert_eq!(state_of(&db, &placement), ExternalLeaseRecordState::Reserved);
        assert!(db
            .activate_external_lease_record("T-missing", &activation('a'), 10)
            .is_err());
        let mut bad_admission = admission();
        bad_admission.resume_state_json = String::new();
        db.activate_external_lease_record(&placement, &activation('a'), 10).unwrap();
        assert!(db.admit_external_lease_record(&placement, &bad_admission, 11).is_err());
        assert_eq!(state_of(&db, &placement), ExternalLeaseRecordState::Activated);
    }

    #[test]
    fn overdue_activated_records_are_listed_until_admitted_or_refused() {
        let (_dir, db) = open();
        let first = activated(&db, "late-a");
        let (second, _) = leased(&db, "late-b");
        let mut later = activation('b');
        later.ready_deadline_ms = 9_000;
        db.activate_external_lease_record(&second, &later, 10).unwrap();
        let (_reserved, _) = leased(&db, "late-c");

        assert!(db.overdue_external_lease_records(4_999).unwrap().is_empty());
        let overdue = db.overdue_external_lease_records(5_000).unwrap();
        assert_eq!(
            overdue.iter().map(|r| r.placement_thread_id.clone()).collect::<Vec<_>>(),
            vec![first.clone()]
        );
        assert_eq!(db.overdue_external_lease_records(9_000).unwrap().len(), 2);
        db.refuse_external_lease_record(&first, "late", 9_000).unwrap();
        db.admit_external_lease_record(&second, &admission(), 9_000).unwrap();
        assert!(db.overdue_external_lease_records(99_000).unwrap().is_empty());
    }

    fn worker_row(db: &RuntimeDb, placement: &str, worker: &str, generation: &str) {
        db.conn
            .with_sql(|connection| {
                Ok(connection.execute(
                    "INSERT INTO worker_process (
                        worker_instance_id, boot_identity_hash, session_capsule_hash,
                        boot_epoch, lifecycle_generation, process_identity,
                        control_channel_identity, state, daemon_generation_id,
                        placement_thread_id, cleanup_state, created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, 1, 1, '{}', 'fd:fixture', 'live', ?4, ?5,
                               'owned', 1, 1)",
                    params![worker, "b".repeat(64), "a".repeat(64), generation, placement],
                )?)
            })
            .unwrap();
    }

    #[test]
    fn records_of_an_earlier_incarnation_are_listed_until_refused() {
        let (_dir, db) = open();
        let (kept, _) = leased(&db, "kept");
        let (lost, _) = leased(&db, "lost");
        worker_row(&db, &kept, "worker-kept", "daemon-first");
        let listed = |generation: &str| -> Vec<String> {
            db.predecessor_external_lease_records(generation)
                .unwrap()
                .into_iter()
                .map(|record| record.placement_thread_id)
                .collect()
        };
        // Under the incarnation that holds a worker, only the worker-less
        // lease is a predecessor's; under a later one, both are.
        assert_eq!(listed("daemon-first"), vec![lost.clone()]);
        assert_eq!(listed("daemon-second"), vec![kept.clone(), lost.clone()]);
        db.refuse_external_lease_record(&lost, "controller restarted", 50).unwrap();
        assert_eq!(listed("daemon-second"), vec![kept.clone()]);
        // A settled allocation no longer holds its lease.
        sql(
            &db,
            "UPDATE external_execution_allocation SET phase='no_contact',updated_at_ms=100
              WHERE placement_thread_id='T-kept'",
        )
        .unwrap();
        assert!(listed("daemon-second").is_empty());
    }
}
