//! Subordinate external allocation ownership, attached to a dedicated session.
//!
//! This is not a second session scheduler. Its key is the existing placement
//! thread. The local worker remains an ordinary local process. Persist contact
//! intention before calling an external allocator; an ambiguous call consumes
//! capacity indefinitely until an independently qualified reconciliation path
//! settles it. No TTL, local process death or caller boolean is cleanup proof.

use super::*;

mod channel;

pub(super) const FIRST_EPOCH: u32 = 40;
pub(super) const GUARD_SQL: &str = r#"CREATE TABLE external_execution_guard (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version = 1),
    unsettled INTEGER NOT NULL CHECK (unsettled >= 0)
)"#;

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_execution_allocation (
    placement_thread_id TEXT PRIMARY KEY,
    capacity_owner TEXT NOT NULL,
    reservation_json TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN
        ('reserved','contact_pending','bound','quarantined','no_contact')),
    occurrence_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((phase IN ('reserved','contact_pending','no_contact') AND occurrence_json IS NULL)
        OR phase = 'quarantined'
        OR (phase = 'bound' AND occurrence_json IS NOT NULL))
);
CREATE INDEX idx_external_execution_capacity
    ON external_execution_allocation(capacity_owner, phase);
CREATE TABLE external_execution_import (
    binding_digest TEXT PRIMARY KEY,
    snapshot_hash TEXT NOT NULL,
    evidence_blob_hash TEXT NOT NULL,
    completion_request_digest TEXT NOT NULL,
    export_frame_digest TEXT NOT NULL
);


CREATE TRIGGER external_execution_import_no_update
BEFORE UPDATE ON external_execution_import
BEGIN SELECT RAISE(ABORT, 'retained external import is immutable'); END;
CREATE TRIGGER external_execution_import_no_delete
BEFORE DELETE ON external_execution_import
BEGIN SELECT RAISE(ABORT, 'external import requires explicit completion retention handoff'); END;

CREATE TRIGGER external_execution_channel_no_delete
BEFORE DELETE ON external_execution_channel
WHEN EXISTS(SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=OLD.placement_thread_id AND a.phase!='no_contact')
BEGIN SELECT RAISE(ABORT, 'external execution retains its channel'); END;

CREATE TRIGGER external_execution_frame_no_delete
BEFORE DELETE ON external_execution_frame
WHEN EXISTS(SELECT 1 FROM external_execution_channel c
    JOIN external_execution_allocation a ON a.placement_thread_id=c.placement_thread_id
    WHERE c.binding_digest=OLD.binding_digest AND a.phase!='no_contact')
BEGIN SELECT RAISE(ABORT, 'external execution retains its transcript'); END;
CREATE TRIGGER external_execution_insert_guard
AFTER INSERT ON external_execution_allocation WHEN NEW.phase != 'no_contact'
BEGIN
    UPDATE external_execution_guard SET unsettled=unsettled+1 WHERE singleton=1;
    SELECT CASE WHEN changes()!=1 THEN RAISE(ABORT, 'external execution guard absent') END;
END;
CREATE TRIGGER external_execution_update_guard
AFTER UPDATE OF phase ON external_execution_allocation
WHEN OLD.phase != 'no_contact' AND NEW.phase = 'no_contact'
BEGIN
    SELECT CASE WHEN OLD.phase != 'reserved'
        THEN RAISE(ABORT, 'external execution contact cannot become no-contact') END;
    UPDATE external_execution_guard SET unsettled=unsettled-1 WHERE singleton=1;
    SELECT CASE WHEN changes()!=1 THEN RAISE(ABORT, 'external execution guard absent') END;
END;
CREATE TRIGGER external_execution_no_reactivation
BEFORE UPDATE ON external_execution_allocation WHEN OLD.phase = 'no_contact'
BEGIN SELECT RAISE(ABORT, 'settled external allocation is immutable'); END;
CREATE TRIGGER external_execution_transition_guard
BEFORE UPDATE ON external_execution_allocation
WHEN NEW.placement_thread_id != OLD.placement_thread_id
 OR NEW.capacity_owner != OLD.capacity_owner
 OR NEW.reservation_json != OLD.reservation_json
 OR NOT (
    (OLD.phase='reserved' AND NEW.phase IN ('contact_pending','no_contact'))
    OR (OLD.phase='contact_pending' AND NEW.phase IN ('bound','quarantined'))
    OR (OLD.phase='bound' AND NEW.phase='quarantined')
    OR (OLD.phase='quarantined' AND NEW.phase='quarantined')
 )
BEGIN SELECT RAISE(ABORT, 'external allocation transition contradicts retained authority'); END;
CREATE TRIGGER external_execution_no_delete
BEFORE DELETE ON external_execution_allocation WHEN OLD.phase != 'no_contact'
BEGIN SELECT RAISE(ABORT, 'unsettled external execution cannot be deleted'); END;
CREATE TRIGGER external_execution_credential_release_guard
BEFORE UPDATE ON credential_profile
WHEN (NEW.lock_owner IS NOT OLD.lock_owner
      OR NEW.credential_generation != OLD.credential_generation
      OR NEW.profile_id IS NOT OLD.profile_id
      OR NEW.home_id IS NOT OLD.home_id
      OR NEW.owner_principal IS NOT OLD.owner_principal)
AND EXISTS (
    SELECT 1 FROM external_execution_allocation a
    JOIN dedicated_session s ON s.placement_thread_id=a.placement_thread_id
    WHERE s.credential_profile_id=OLD.profile_id AND a.phase!='no_contact'
)
BEGIN SELECT RAISE(ABORT, 'external execution cleanup retains credential ownership'); END;
CREATE TRIGGER external_execution_credential_delete_guard
BEFORE DELETE ON credential_profile
WHEN EXISTS (
    SELECT 1 FROM external_execution_allocation a
    JOIN dedicated_session s ON s.placement_thread_id=a.placement_thread_id
    WHERE s.credential_profile_id=OLD.profile_id AND a.phase!='no_contact'
)
BEGIN SELECT RAISE(ABORT, 'external execution retains its credential owner'); END;
CREATE TRIGGER external_execution_workspace_delete_guard
BEFORE DELETE ON execution_workspace
WHEN EXISTS (
    SELECT 1 FROM external_execution_allocation a
    JOIN dedicated_session s ON s.placement_thread_id=a.placement_thread_id
    WHERE s.workspace_id=OLD.workspace_id AND a.phase!='no_contact'
)
BEGIN SELECT RAISE(ABORT, 'external execution retains its workspace'); END;
CREATE TRIGGER external_execution_workspace_identity_guard
BEFORE UPDATE ON execution_workspace
WHEN EXISTS (
    SELECT 1 FROM external_execution_allocation a
    JOIN dedicated_session s ON s.placement_thread_id=a.placement_thread_id
    WHERE s.workspace_id=OLD.workspace_id AND a.phase!='no_contact'
)
AND (NEW.workspace_id IS NOT OLD.workspace_id
    OR NEW.thread_id IS NOT OLD.thread_id
    OR NEW.launch_owner IS NOT OLD.launch_owner
    OR NEW.base_snapshot IS NOT OLD.base_snapshot
    OR NEW.root_path IS NOT OLD.root_path
    OR NEW.frozen_snapshot_hash IS NOT OLD.frozen_snapshot_hash
    OR NEW.frozen_output_capture_hash IS NOT OLD.frozen_output_capture_hash
    OR NEW.state IN ('freezing','destroying','closing','closed'))
BEGIN SELECT RAISE(ABORT, 'external execution blocks local workspace settlement'); END;
CREATE TRIGGER external_execution_session_delete_guard
BEFORE DELETE ON dedicated_session
WHEN EXISTS (SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=OLD.placement_thread_id AND a.phase!='no_contact')
BEGIN SELECT RAISE(ABORT, 'external execution retains its session owner'); END;
CREATE TRIGGER external_execution_session_identity_guard
BEFORE UPDATE ON dedicated_session
WHEN EXISTS (SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=OLD.placement_thread_id AND a.phase!='no_contact')
AND (NEW.placement_thread_id IS NOT OLD.placement_thread_id
    OR NEW.admitted_capsule_hash IS NOT OLD.admitted_capsule_hash
    OR NEW.workspace_id IS NOT OLD.workspace_id
    OR NEW.worker_instance_id IS NOT OLD.worker_instance_id
    OR NEW.worker_boot_epoch IS NOT OLD.worker_boot_epoch
    OR NEW.credential_profile_id IS NOT OLD.credential_profile_id
    OR NEW.credential_generation IS NOT OLD.credential_generation
    OR NEW.candidate_snapshot_hash IS NOT OLD.candidate_snapshot_hash
    OR NEW.state IN ('freezing','frozen','verifying','qualifying','publish_ready',
        'publishing','discarding','terminal'))
BEGIN SELECT RAISE(ABORT, 'external execution blocks local completion and owner replacement'); END;
"#;

/// No secrets, URLs or arbitrary commands are accepted at this boundary.
/// The protected adapter binding supplies them, not a project/worker request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAllocationReservation {
    pub schema: u32,
    pub placement_thread_id: String,
    pub admitted_capsule_hash: String,
    pub workspace_id: String,
    pub worker_instance_id: String,
    pub worker_boot_epoch: u64,
    pub base_snapshot_hash: String,
    pub binding_hash: String,
    /// Stable protected capacity domain across binding/policy generations.
    pub capacity_owner: String,
    pub request_digest: String,
    pub max_active: u16,
    pub timeout_seconds: u32,
    pub contact_deadline_ms: i64,
}

impl ExternalAllocationReservation {
    pub fn validate(&self) -> Result<()> {
        if self.schema != 1
            || !(1..=64).contains(&self.max_active)
            || !(1..=3600).contains(&self.timeout_seconds)
            || self.contact_deadline_ms <= 0
            || self.worker_boot_epoch == 0
            || self.worker_boot_epoch > i64::MAX as u64
        {
            bail!("external allocation reservation is outside its versioned bounds");
        }
        validate_bounded_runtime_text("external placement", &self.placement_thread_id, 256)?;
        validate_bounded_runtime_text("external workspace", &self.workspace_id, 256)?;
        validate_bounded_runtime_text("external worker", &self.worker_instance_id, 256)?;
        for hash in [
            &self.admitted_capsule_hash,
            &self.base_snapshot_hash,
            &self.binding_hash,
            &self.capacity_owner,
            &self.request_digest,
        ] {
            validate_sha256("external allocation identity", hash)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAllocationOccurrence {
    pub schema: u32,
    pub binding_hash: String,
    pub request_digest: String,
    pub occurrence_id: String,
    pub provider_observation_digest: String,
}

impl ExternalAllocationOccurrence {
    fn validate(&self, reservation: &ExternalAllocationReservation) -> Result<()> {
        if self.schema != 1
            || self.binding_hash != reservation.binding_hash
            || self.request_digest != reservation.request_digest
        {
            bail!("external occurrence contradicts its reserved authority");
        }
        validate_bounded_runtime_text("external occurrence", &self.occurrence_id, 512)?;
        validate_sha256(
            "external occurrence observation",
            &self.provider_observation_digest,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAllocationPhase {
    Reserved,
    ContactPending,
    Bound,
    Quarantined,
    NoContact,
}

impl ExternalAllocationPhase {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "contact_pending" => Ok(Self::ContactPending),
            "bound" => Ok(Self::Bound),
            "quarantined" => Ok(Self::Quarantined),
            "no_contact" => Ok(Self::NoContact),
            _ => bail!("external allocation phase is not current"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAllocationRecord {
    pub reservation: ExternalAllocationReservation,
    pub phase: ExternalAllocationPhase,
    pub occurrence: Option<ExternalAllocationOccurrence>,
}

fn read(conn: &Connection, placement: &str) -> Result<Option<ExternalAllocationRecord>> {
    let raw: Option<(String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT capacity_owner,reservation_json,phase,occurrence_json
         FROM external_execution_allocation WHERE placement_thread_id=?1",
            [placement],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    raw.map(|(capacity, reservation_json, phase, occurrence_json)| {
        if reservation_json.len() > 8192
            || occurrence_json.as_ref().is_some_and(|raw| raw.len() > 8192)
        {
            bail!("external allocation record exceeds its bound");
        }
        let reservation: ExternalAllocationReservation = serde_json::from_str(&reservation_json)?;
        reservation.validate()?;
        if reservation.placement_thread_id != placement
            || reservation.capacity_owner != capacity
            || lillux::canonical_json(&serde_json::to_value(&reservation)?)? != reservation_json
        {
            bail!("external allocation row contradicts its canonical reservation");
        }
        let phase = ExternalAllocationPhase::parse(&phase)?;
        let occurrence = occurrence_json
            .map(|raw| {
                let occurrence: ExternalAllocationOccurrence = serde_json::from_str(&raw)?;
                occurrence.validate(&reservation)?;
                if lillux::canonical_json(&serde_json::to_value(&occurrence)?)? != raw {
                    bail!("external occurrence is not canonical");
                }
                Ok::<_, anyhow::Error>(occurrence)
            })
            .transpose()?;
        match (phase, occurrence.is_some()) {
            (ExternalAllocationPhase::Bound, false)
            | (
                ExternalAllocationPhase::Reserved
                | ExternalAllocationPhase::ContactPending
                | ExternalAllocationPhase::NoContact,
                true,
            ) => {
                bail!("external occurrence contradicts its phase");
            }
            _ => {}
        }
        Ok(ExternalAllocationRecord {
            reservation,
            phase,
            occurrence,
        })
    })
    .transpose()
}

/// Stable, independently readable reset guard. Never decode version-specific
/// allocation/session rows to infer that remote obligations disappeared.
pub(super) fn read_guard(conn: &Connection) -> Result<i64> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='external_execution_guard'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if sql.as_deref() != Some(GUARD_SQL) {
        bail!("external execution guard is absent or has an unsupported contract");
    }
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM external_execution_guard", [], |row| {
            row.get(0)
        })?;
    let (version, unsettled): (i64, i64) = conn.query_row(
        "SELECT schema_version, unsettled FROM external_execution_guard WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count != 1 || version != 1 || unsettled < 0 {
        bail!("external execution guard is malformed");
    }
    Ok(unsettled)
}

pub(super) fn ensure_resettable(conn: &Connection, epoch: u32) -> Result<()> {
    if epoch >= FIRST_EPOCH && read_guard(conn)? != 0 {
        bail!(
            "execution-history reset retains unsettled external execution; local host death and elapsed TTL are not remote cleanup proof"
        );
    }
    Ok(())
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let unsettled: i64 = conn.query_row(
        "SELECT COUNT(*) FROM external_execution_allocation WHERE phase!='no_contact'",
        [],
        |row| row.get(0),
    )?;
    if read_guard(conn)? != unsettled {
        bail!("external execution guard contradicts its allocation journal");
    }
    let mut statement =
        conn.prepare("SELECT placement_thread_id FROM external_execution_allocation")?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        let record = read(conn, &placement)?.context("external allocation disappeared")?;
        if record.phase != ExternalAllocationPhase::NoContact {
            require_session_owner(conn, &record.reservation)?;
        }
    }
    channel::validate_channels(conn)?;
    Ok(())
}

fn require_session_owner(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
) -> Result<()> {
    let matched: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM dedicated_session s JOIN credential_profile p
           ON p.profile_id=s.credential_profile_id
         JOIN execution_workspace w ON w.workspace_id=s.workspace_id
         WHERE s.placement_thread_id=?1 AND s.admitted_capsule_hash=?2 AND s.workspace_id=?3
           AND s.worker_instance_id=?4 AND s.worker_boot_epoch=?5
           AND w.thread_id=s.placement_thread_id AND w.base_snapshot=?6
           AND w.launch_owner='dedicated_worker_session'
           AND p.credential_generation=s.credential_generation
           AND p.lock_owner=s.worker_instance_id)",
        params![
            reservation.placement_thread_id,
            reservation.admitted_capsule_hash,
            reservation.workspace_id,
            reservation.worker_instance_id,
            i64::try_from(reservation.worker_boot_epoch)?,
            reservation.base_snapshot_hash
        ],
        |row| row.get(0),
    )?;
    if !matched {
        bail!("external allocation has no exact locked dedicated-session owner");
    }
    Ok(())
}

fn require_unreleased_session(conn: &Connection, placement: &str) -> Result<()> {
    let admitted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM dedicated_session WHERE placement_thread_id=?1
            AND state='admitted' AND send_boundary='none')",
        [placement],
        |row| row.get(0),
    )?;
    if !admitted {
        bail!("external allocation requires an unreleased admitted session");
    }
    Ok(())
}

impl RuntimeDb {
    pub fn external_allocation(&self, placement: &str) -> Result<Option<ExternalAllocationRecord>> {
        read(&self.conn, placement)
    }

    /// Idempotent reservation; no allocator may be contacted here. Unknown
    /// prior calls and quarantined occurrences retain capacity across restart.
    pub fn reserve_external_allocation(
        &self,
        reservation: &ExternalAllocationReservation,
    ) -> Result<ExternalAllocationRecord> {
        reservation.validate()?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        validate_current(&tx)?;
        if let Some(existing) = read(&tx, &reservation.placement_thread_id)? {
            if existing.reservation != *reservation {
                bail!("external allocation replay changed its exact reservation");
            }
            tx.commit()?;
            return Ok(existing);
        }
        require_session_owner(&tx, reservation)?;
        require_unreleased_session(&tx, &reservation.placement_thread_id)?;
        if read_guard(&tx)? >= 256 {
            bail!("node external allocation ceiling reached");
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        if now >= reservation.contact_deadline_ms
            || reservation.contact_deadline_ms.saturating_sub(now) > 300_000
        {
            bail!("external allocation contact deadline has expired");
        }
        let (count, prior_limit): (i64, Option<i64>) = tx.query_row(
            "SELECT COUNT(*),MIN(json_extract(reservation_json,'$.max_active'))
             FROM external_execution_allocation WHERE capacity_owner=?1 AND phase!='no_contact'",
            [&reservation.capacity_owner],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let limit = i64::from(reservation.max_active).min(prior_limit.unwrap_or(64));
        if count >= limit {
            bail!("external execution capacity remains reserved");
        }
        tx.execute(
            "INSERT INTO external_execution_allocation VALUES(?1,?2,?3,'reserved',NULL,?4,?4)",
            params![
                reservation.placement_thread_id,
                reservation.capacity_owner,
                lillux::canonical_json(&serde_json::to_value(reservation)?)?,
                now
            ],
        )?;
        let record =
            read(&tx, &reservation.placement_thread_id)?.context("reserved allocation missing")?;
        tx.commit()?;
        Ok(record)
    }

    /// Only the winner of this durable CAS may contact the allocator.
    /// false means no new call, including after an ambiguous response/crash.
    pub fn claim_external_allocation_contact(
        &self,
        placement: &str,
        request_digest: &str,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation was not reserved")?;
        if record.reservation.request_digest != request_digest {
            bail!("external allocation contact changed its request identity");
        }
        require_session_owner(&tx, &record.reservation)?;
        if record.phase != ExternalAllocationPhase::Reserved {
            return Ok(false);
        }
        require_unreleased_session(&tx, placement)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        if now >= record.reservation.contact_deadline_ms {
            bail!("external allocation contact deadline expired");
        }
        let changed = tx.execute(
            "UPDATE external_execution_allocation SET phase='contact_pending',updated_at_ms=?2
             WHERE placement_thread_id=?1 AND phase='reserved'",
            params![placement, now],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Bind the exact returned occurrence to the original pending contact.
    /// This is allocation observation only, not release or completion proof.
    pub fn bind_external_allocation(
        &self,
        placement: &str,
        occurrence: &ExternalAllocationOccurrence,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        occurrence.validate(&record.reservation)?;
        if let Some(prior) = &record.occurrence {
            if prior != occurrence {
                bail!("external allocation occurrence changed");
            }
            return Ok(());
        }
        if !matches!(
            record.phase,
            ExternalAllocationPhase::ContactPending | ExternalAllocationPhase::Quarantined
        ) {
            bail!("external occurrence arrived outside its durable contact");
        }
        let already_owned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_allocation
             WHERE placement_thread_id!=?1 AND capacity_owner=?2
               AND json_extract(occurrence_json,'$.occurrence_id')=?3)",
            params![
                placement,
                record.reservation.capacity_owner,
                occurrence.occurrence_id
            ],
            |row| row.get(0),
        )?;
        if already_owned {
            bail!("external occurrence already belongs to another placement");
        }
        // Late exact responses may identify cleanup after quarantine, but may
        // never reverse quarantine into execution permission.
        tx.execute(
            "UPDATE external_execution_allocation
             SET occurrence_json=?2,phase=CASE WHEN phase='quarantined' THEN phase ELSE 'bound' END,updated_at_ms=?3
             WHERE placement_thread_id=?1",
            params![placement, lillux::canonical_json(&serde_json::to_value(occurrence)?)?,
                i64::try_from(lillux::time::timestamp_millis())?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Cancel without external contact, or conservatively quarantine a call
    /// that may have been accepted. No contacted phase can become no-contact.
    pub fn cancel_external_allocation(&self, placement: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        if record.phase == ExternalAllocationPhase::NoContact {
            return Ok(());
        }
        tx.execute(
            "UPDATE external_execution_allocation SET phase=?2,updated_at_ms=?3 WHERE placement_thread_id=?1",
            params![placement, if record.phase == ExternalAllocationPhase::Reserved {"no_contact"} else {"quarantined"},
                i64::try_from(lillux::time::timestamp_millis())?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn external_execution_cas_roots(&self) -> Result<Vec<String>> {
        validate_current(&self.conn)?;
        let mut statement = self.conn.prepare(
            "SELECT placement_thread_id FROM external_execution_allocation WHERE phase!='no_contact'")?;
        let placements = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut roots = BTreeSet::new();
        for placement in placements {
            let record =
                read(&self.conn, &placement)?.context("external allocation root is absent")?;
            roots.insert(record.reservation.admitted_capsule_hash);
            roots.insert(record.reservation.base_snapshot_hash);
        }
        let mut statement = self
            .conn
            .prepare("SELECT snapshot_hash FROM external_execution_import")?;
        for root in statement.query_map([], |row| row.get::<_, String>(0))? {
            roots.insert(root?);
        }
        Ok(roots.into_iter().collect())
    }

    pub fn external_execution_blob_roots(&self) -> Result<Vec<String>> {
        validate_current(&self.conn)?;
        let mut statement = self
            .conn
            .prepare("SELECT evidence_blob_hash FROM external_execution_import")?;
        Ok(statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn reservation(db: &RuntimeDb, suffix: &str) -> ExternalAllocationReservation {
        let placement = format!("T-{suffix}");
        let workspace = format!("W-{suffix}");
        let worker = format!("worker-{suffix}");
        let profile = format!("P-{suffix}");
        db.create_credential_profile(NewCredentialProfile {
            profile_id: &profile,
            owner_principal: "fp:operator",
            home_id: &format!("home-{suffix}"),
        })
        .unwrap();
        db.acquire_credential_profile(&profile, "fp:operator", &worker)
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            placement_thread_id: &placement,
            chain_root_id: "T-root",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: &workspace,
            candidate_required: true,
            candidate_disposition: DedicatedCandidateDisposition::OwnerDecision,
            credential_profile_id: &profile,
            credential_generation: 1,
            credential_lock_owner: &worker,
        })
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO execution_workspace(workspace_id,thread_id,launch_owner,backend_id,
             base_snapshot,root_path,state,created_at_ms,updated_at_ms)
             VALUES(?1,?2,'dedicated_worker_session','fixture',?3,'/fixture','ready',1,1)",
                params![workspace, placement, "b".repeat(64)],
            )
            .unwrap();
        ExternalAllocationReservation {
            schema: 1,
            placement_thread_id: placement,
            admitted_capsule_hash: "a".repeat(64),
            workspace_id: workspace,
            worker_instance_id: worker,
            worker_boot_epoch: 1,
            base_snapshot_hash: "b".repeat(64),
            binding_hash: "c".repeat(64),
            capacity_owner: "d".repeat(64),
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            contact_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap() + 60_000,
        }
    }

    #[test]
    fn external_contact_is_claimed_once_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let reserved = reservation(&db, "one");
        assert_eq!(
            db.reserve_external_allocation(&reserved).unwrap().phase,
            ExternalAllocationPhase::Reserved
        );
        assert_eq!(
            db.reserve_external_allocation(&reserved).unwrap().phase,
            ExternalAllocationPhase::Reserved
        );
        assert!(
            db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap()
        );
        assert!(
            !db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap()
        );
        assert!(db.discard_all_thread_history(true).is_err());
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert!(
            !db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap()
        );
        db.cancel_external_allocation("T-one").unwrap();
        assert_eq!(
            db.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::Quarantined
        );
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
        assert!(
            db.release_credential_profile("P-one", "worker-one")
                .is_err()
        );
        assert_eq!(
            db.external_execution_cas_roots().unwrap(),
            vec!["a".repeat(64), "b".repeat(64)]
        );
        let second = reservation(&db, "two");
        assert!(db.reserve_external_allocation(&second).is_err());
    }

    #[test]
    fn external_no_contact_cancel_settles_without_reopening_the_intent() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reserved = reservation(&db, "one");
        db.reserve_external_allocation(&reserved).unwrap();
        db.cancel_external_allocation("T-one").unwrap();
        db.cancel_external_allocation("T-one").unwrap();
        assert!(
            !db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap()
        );
        assert_eq!(read_guard(&db.conn).unwrap(), 0);
        db.release_credential_profile("P-one", "worker-one")
            .unwrap();
        assert_eq!(
            db.reserve_external_allocation(&reserved).unwrap().phase,
            ExternalAllocationPhase::NoContact
        );
    }

    #[test]
    fn external_late_occurrence_identifies_cleanup_without_releasing_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reserved = reservation(&db, "one");
        db.reserve_external_allocation(&reserved).unwrap();
        db.claim_external_allocation_contact("T-one", &reserved.request_digest)
            .unwrap();
        db.cancel_external_allocation("T-one").unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reserved.binding_hash,
            request_digest: reserved.request_digest,
            occurrence_id: "exact-occurrence".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation("T-one", &occurrence).unwrap();
        db.bind_external_allocation("T-one", &occurrence).unwrap();
        let record = db.external_allocation("T-one").unwrap().unwrap();
        assert_eq!(record.phase, ExternalAllocationPhase::Quarantined);
        assert_eq!(record.occurrence, Some(occurrence.clone()));
        let mut wrong = occurrence;
        wrong.occurrence_id = "another-occurrence".into();
        assert!(db.bind_external_allocation("T-one", &wrong).is_err());
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
    }

    #[test]
    fn external_reset_guard_does_not_decode_predecessor_execution_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(GUARD_SQL).unwrap();
        conn.execute_batch(
            "INSERT INTO external_execution_guard VALUES(1,1,1);
            CREATE TABLE unknown_execution_authority(opaque BLOB);
            INSERT INTO unknown_execution_authority VALUES(X'FF00');",
        )
        .unwrap();
        assert!(ensure_resettable(&conn, FIRST_EPOCH).is_err());
        assert!(ensure_resettable(&conn, FIRST_EPOCH + 1).is_err());
        conn.execute("UPDATE external_execution_guard SET unsettled=0", [])
            .unwrap();
        ensure_resettable(&conn, FIRST_EPOCH).unwrap();
        conn.execute("DELETE FROM external_execution_guard", [])
            .unwrap();
        assert!(ensure_resettable(&conn, FIRST_EPOCH).is_err());
        conn.execute("DROP TABLE external_execution_guard", [])
            .unwrap();
        assert!(ensure_resettable(&conn, FIRST_EPOCH).is_err());
        ensure_resettable(&conn, FIRST_EPOCH - 1).unwrap();
    }

    #[test]
    fn external_reservation_rejects_changed_or_unowned_authority() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reserved = reservation(&db, "one");
        let mut wrong = reserved.clone();
        wrong.base_snapshot_hash = "f".repeat(64);
        assert!(db.reserve_external_allocation(&wrong).is_err());
        wrong = reserved.clone();
        wrong.worker_boot_epoch += 1;
        assert!(db.reserve_external_allocation(&wrong).is_err());
        db.reserve_external_allocation(&reserved).unwrap();
        wrong = reserved.clone();
        wrong.binding_hash = "f".repeat(64);
        assert!(db.reserve_external_allocation(&wrong).is_err());
        assert!(
            db.claim_external_allocation_contact("T-one", &"f".repeat(64))
                .is_err()
        );
        assert_eq!(
            db.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::Reserved
        );
    }
}
