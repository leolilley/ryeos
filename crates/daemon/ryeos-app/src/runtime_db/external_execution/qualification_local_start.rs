//! One local, pre-kernel start attempt for a bound Q allocation.
//!
//! This is a journal of intent and exact receipts. It cannot recreate the
//! original private workspace directory after a crash. A surviving row grants
//! cleanup/recovery only, never another directory borrow or Worker launch.

use super::*;
use ryeos_state::external_execution::journal::load_binding;

pub(in crate::runtime_db) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_qualification_local_start (
    placement_thread_id TEXT PRIMARY KEY REFERENCES external_execution_allocation(placement_thread_id),
    accepted_root_id TEXT NOT NULL,
    accepted_capsule_hash TEXT NOT NULL,
    qualification_operation_id TEXT NOT NULL,
    qualification_occurrence_id TEXT NOT NULL,
    channel_binding_digest TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN
        ('reserved','workspace_ready','worker_attached','cleanup_unproved',
         'cleanup_intent','workspace_removed','state_removed','closed')),
    record_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (updated_at_ms >= created_at_ms),
    CHECK (length(CAST(record_json AS BLOB)) BETWEEN 1 AND 131072)
);
CREATE TRIGGER external_qualification_local_start_insert_guard
BEFORE INSERT ON external_qualification_local_start
WHEN NEW.phase!='reserved'
 OR NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    JOIN external_execution_channel c ON c.placement_thread_id=a.placement_thread_id
    WHERE a.placement_thread_id=NEW.placement_thread_id AND a.phase='bound'
      AND json_extract(a.reservation_json,'$.owner.kind')='qualification_candidate'
      AND json_extract(a.reservation_json,'$.owner.coordinate.accepted_root_id')=NEW.accepted_root_id
      AND json_extract(a.reservation_json,'$.owner.coordinate.accepted_capsule_hash')=NEW.accepted_capsule_hash
      AND json_extract(a.reservation_json,'$.owner.qualification_operation_id')=NEW.qualification_operation_id
      AND json_extract(a.reservation_json,'$.owner.qualification_occurrence.occurrence_id')=NEW.qualification_occurrence_id
      AND c.binding_digest=NEW.channel_binding_digest AND c.state='running')
BEGIN SELECT RAISE(ABORT, 'qualification local start requires exact bound Q owner'); END;
CREATE TRIGGER external_qualification_local_start_transition_guard
BEFORE UPDATE ON external_qualification_local_start
WHEN NEW.placement_thread_id!=OLD.placement_thread_id
 OR NEW.accepted_root_id!=OLD.accepted_root_id
 OR NEW.accepted_capsule_hash!=OLD.accepted_capsule_hash
 OR NEW.qualification_operation_id!=OLD.qualification_operation_id
 OR NEW.qualification_occurrence_id!=OLD.qualification_occurrence_id
 OR NEW.channel_binding_digest!=OLD.channel_binding_digest
 OR NEW.created_at_ms!=OLD.created_at_ms
 OR NEW.updated_at_ms<OLD.updated_at_ms
 OR (NEW.phase IN ('cleanup_intent','workspace_removed','state_removed','closed')
     AND json_extract(NEW.record_json,'$.directory_borrow_intent') IS NOT NULL
     AND json_extract(NEW.record_json,'$.worker_incarnation_digest') IS NULL)
 OR json_remove(NEW.record_json,'$.phase','$.workspace_identity','$.directory_borrow_intent',
      '$.worker_incarnation_digest','$.cleanup_reason','$.cleanup','$.updated_at_ms')
    != json_remove(OLD.record_json,'$.phase','$.workspace_identity','$.directory_borrow_intent',
      '$.worker_incarnation_digest','$.cleanup_reason','$.cleanup','$.updated_at_ms')
 OR (json_extract(NEW.record_json,'$.workspace_identity') IS NOT json_extract(OLD.record_json,'$.workspace_identity')
     AND NOT (OLD.phase='reserved' AND NEW.phase='workspace_ready'
          AND json_extract(OLD.record_json,'$.workspace_identity') IS NULL
          AND json_extract(NEW.record_json,'$.workspace_identity') IS NOT NULL))
 OR (json_extract(NEW.record_json,'$.directory_borrow_intent') IS NOT json_extract(OLD.record_json,'$.directory_borrow_intent')
     AND NOT (OLD.phase='workspace_ready' AND NEW.phase='workspace_ready'
          AND json_extract(OLD.record_json,'$.directory_borrow_intent') IS NULL
          AND json_extract(NEW.record_json,'$.directory_borrow_intent') IS NOT NULL))
 OR (json_extract(NEW.record_json,'$.worker_incarnation_digest') IS NOT json_extract(OLD.record_json,'$.worker_incarnation_digest')
     AND NOT (OLD.phase='workspace_ready' AND NEW.phase='worker_attached'
          AND json_extract(OLD.record_json,'$.worker_incarnation_digest') IS NULL
          AND json_extract(NEW.record_json,'$.worker_incarnation_digest') IS NOT NULL))
 OR (NEW.phase=OLD.phase AND NOT (
      OLD.phase='workspace_ready'
      AND json_extract(OLD.record_json,'$.directory_borrow_intent') IS NULL
      AND json_extract(NEW.record_json,'$.directory_borrow_intent') IS NOT NULL
      AND json_remove(NEW.record_json,'$.directory_borrow_intent','$.updated_at_ms')
          = json_remove(OLD.record_json,'$.directory_borrow_intent','$.updated_at_ms')))
 OR NOT ((OLD.phase='reserved' AND NEW.phase='workspace_ready')
      OR (OLD.phase='workspace_ready' AND NEW.phase IN ('workspace_ready','worker_attached'))
      OR (OLD.phase IN ('reserved','workspace_ready','worker_attached') AND NEW.phase='cleanup_unproved')
      OR (OLD.phase IN ('reserved','workspace_ready','worker_attached','cleanup_unproved') AND NEW.phase='cleanup_intent')
      OR (OLD.phase='cleanup_intent' AND NEW.phase='workspace_removed')
      OR (OLD.phase='workspace_removed' AND NEW.phase='state_removed')
      OR (OLD.phase='state_removed' AND NEW.phase='closed'))
BEGIN SELECT RAISE(ABORT, 'qualification local start transition changed its one-use owner'); END;
CREATE TRIGGER external_qualification_local_start_no_delete
BEFORE DELETE ON external_qualification_local_start
BEGIN SELECT RAISE(ABORT, 'qualification local start cleanup requires exact proof'); END;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationLocalStartPhase {
    Reserved,
    WorkspaceReady,
    WorkerAttached,
    CleanupUnproved,
    CleanupIntent,
    WorkspaceRemoved,
    StateRemoved,
    Closed,
}

impl QualificationLocalStartPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::WorkspaceReady => "workspace_ready",
            Self::WorkerAttached => "worker_attached",
            Self::CleanupUnproved => "cleanup_unproved",
            Self::CleanupIntent => "cleanup_intent",
            Self::WorkspaceRemoved => "workspace_removed",
            Self::StateRemoved => "state_removed",
            Self::Closed => "closed",
        }
    }
}

/// One durable intent to lend the original pinned project directory to a held
/// Worker. The intent precedes descriptor cloning and cannot be replayed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkspaceBorrowIntent {
    pub workspace_id: String,
    pub directory_identity: lillux::PinnedDirectoryIdentity,
    pub reserved_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationLocalCleanupEvidence {
    pub prior_failure_reason: Option<String>,
    pub intent_at_ms: i64,
    pub workspace_removed_at_ms: Option<i64>,
    pub state_removed_at_ms: Option<i64>,
    pub closed_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationLocalStartRecord {
    pub placement_thread_id: String,
    pub accepted_root_id: String,
    pub accepted_capsule_hash: String,
    pub qualification_operation_id: String,
    pub qualification_occurrence_id: String,
    pub channel_binding_digest: String,
    pub daemon_generation_id: String,
    pub base_snapshot_hash: String,
    pub workspace_id: String,
    /// Diagnostic path only; the original pinned directory is workspace authority.
    pub workspace_root: String,
    /// Diagnostic coordinate for a separate one-use pinned child under cache.
    /// The original pinned descriptor is the Worker state-root authority.
    pub state_root: String,
    pub launch_owner: String,
    pub phase: QualificationLocalStartPhase,
    pub workspace_identity: Option<lillux::PinnedDirectoryIdentity>,
    pub directory_borrow_intent: Option<QualificationWorkspaceBorrowIntent>,
    pub worker_incarnation_digest: Option<String>,
    pub cleanup_reason: Option<String>,
    cleanup: Option<QualificationLocalCleanupEvidence>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

fn local_workspace_digest(
    placement: &str,
    root: &str,
    capsule: &str,
    operation: &str,
    occurrence: &str,
    channel: &str,
    daemon: &str,
) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "placement": placement,
        "root": root,
        "capsule": capsule,
        "operation": operation,
        "occurrence": occurrence,
        "channel": channel,
        "daemon": daemon,
    }))
}

impl QualificationLocalStartRecord {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.placement_thread_id
                .strip_prefix("Q-")
                .is_some_and(lillux::valid_hash),
            "qualification local start has invalid placement"
        );
        for (label, value) in [
            ("capsule", &self.accepted_capsule_hash),
            ("operation", &self.qualification_operation_id),
            ("channel", &self.channel_binding_digest),
            ("base", &self.base_snapshot_hash),
        ] {
            validate_sha256(label, value)?;
        }
        for (label, value) in [
            ("accepted root", &self.accepted_root_id),
            ("occurrence", &self.qualification_occurrence_id),
            ("daemon generation", &self.daemon_generation_id),
            ("workspace root", &self.workspace_root),
            ("state root", &self.state_root),
        ] {
            validate_bounded_runtime_text(label, value, 4096)?;
        }
        ryeos_engine::execution_workspace::validate_workspace_id(&self.workspace_id)?;
        let digest = local_workspace_digest(
            &self.placement_thread_id,
            &self.accepted_root_id,
            &self.accepted_capsule_hash,
            &self.qualification_operation_id,
            &self.qualification_occurrence_id,
            &self.channel_binding_digest,
            &self.daemon_generation_id,
        )?;
        ensure!(
            self.workspace_id == format!("QW-{digest}")
                && self.launch_owner == format!("Q-local-{digest}")
                && self.workspace_root.starts_with('/')
                && std::path::Path::new(&self.workspace_root)
                    .file_name()
                    .and_then(|name| name.to_str())
                    == Some(self.workspace_id.as_str())
                && self.created_at_ms > 0
                && self.updated_at_ms >= self.created_at_ms,
            "qualification local start has invalid workspace coordinate or time"
        );
        let workspace_root = std::path::Path::new(&self.workspace_root);
        let cache_root = workspace_root
            .parent()
            .and_then(std::path::Path::parent)
            .context("qualification workspace has no cache parent")?;
        ensure!(
            workspace_root.parent() == Some(cache_root.join("executions").as_path())
                && std::path::Path::new(&self.state_root)
                    == cache_root
                        .join("qualification-local")
                        .join(&self.workspace_id),
            "qualification state root is not the separate exact cache child"
        );
        validate_bounded_runtime_text("local launch owner", &self.launch_owner, 256)?;
        if let Some(intent) = &self.directory_borrow_intent {
            ensure!(
                intent.workspace_id == self.workspace_id
                    && self.workspace_identity.as_ref() == Some(&intent.directory_identity)
                    && intent.reserved_at_ms >= self.created_at_ms
                    && intent.reserved_at_ms <= self.updated_at_ms,
                "qualification directory borrow changed its original identity or time"
            );
        }
        if let Some(digest) = &self.worker_incarnation_digest {
            validate_sha256("local Worker incarnation", digest)?;
            ensure!(
                self.directory_borrow_intent.is_some(),
                "qualification Worker has no original directory borrow"
            );
        }
        if let Some(reason) = &self.cleanup_reason {
            validate_bounded_runtime_text("local cleanup reason", reason, 1024)?;
        }
        ensure!(
            match self.phase {
                QualificationLocalStartPhase::Reserved =>
                    self.workspace_identity.is_none()
                        && self.directory_borrow_intent.is_none()
                        && self.worker_incarnation_digest.is_none()
                        && self.cleanup.is_none(),
                QualificationLocalStartPhase::WorkspaceReady =>
                    self.workspace_identity.is_some()
                        && self.worker_incarnation_digest.is_none()
                        && self.cleanup.is_none(),
                QualificationLocalStartPhase::WorkerAttached =>
                    self.workspace_identity.is_some()
                        && self.directory_borrow_intent.is_some()
                        && self.worker_incarnation_digest.is_some()
                        && self.cleanup.is_none(),
                QualificationLocalStartPhase::CleanupUnproved => self.cleanup.is_none(),
                QualificationLocalStartPhase::CleanupIntent
                | QualificationLocalStartPhase::WorkspaceRemoved
                | QualificationLocalStartPhase::StateRemoved
                | QualificationLocalStartPhase::Closed => self.cleanup.is_some(),
            } && (self.phase == QualificationLocalStartPhase::CleanupUnproved)
                == self.cleanup_reason.is_some(),
            "qualification local start has impossible phase evidence"
        );
        if let Some(cleanup) = &self.cleanup {
            self.validate_cleanup(cleanup)?;
        }
        Ok(())
    }

    fn validate_cleanup(&self, cleanup: &QualificationLocalCleanupEvidence) -> Result<()> {
        ensure!(
            self.directory_borrow_intent.is_none() || self.worker_incarnation_digest.is_some(),
            "qualification cleanup cannot prove an unattached borrowed Worker dead"
        );
        if let Some(reason) = &cleanup.prior_failure_reason {
            validate_bounded_runtime_text("prior local cleanup failure", reason, 1024)?;
        }
        let stage = match self.phase {
            QualificationLocalStartPhase::CleanupIntent => 0,
            QualificationLocalStartPhase::WorkspaceRemoved => 1,
            QualificationLocalStartPhase::StateRemoved => 2,
            QualificationLocalStartPhase::Closed => 3,
            _ => bail!("qualification cleanup evidence has no cleanup phase"),
        };
        ensure!(
            cleanup.intent_at_ms >= self.created_at_ms
                && cleanup.intent_at_ms <= self.updated_at_ms
                && cleanup.workspace_removed_at_ms.is_some() == (stage >= 1)
                && cleanup.state_removed_at_ms.is_some() == (stage >= 2)
                && cleanup.closed_at_ms.is_some() == (stage >= 3),
            "qualification cleanup phase lacks its exact ordered evidence"
        );
        let mut previous = cleanup.intent_at_ms;
        for instant in [
            cleanup.workspace_removed_at_ms,
            cleanup.state_removed_at_ms,
            cleanup.closed_at_ms,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                instant >= previous && instant <= self.updated_at_ms,
                "qualification cleanup evidence time regressed"
            );
            previous = instant;
        }
        Ok(())
    }
}

fn canonical(record: &QualificationLocalStartRecord) -> Result<String> {
    record.validate()?;
    let json = lillux::canonical_json(&serde_json::to_value(record)?)?;
    ensure!(
        json.len() <= 131072,
        "qualification local start exceeds journal bound"
    );
    Ok(json)
}

pub(super) fn read_local_start(
    conn: &Connection,
    placement: &str,
) -> Result<Option<QualificationLocalStartRecord>> {
    let row: Option<(String, String, String, String, String, String, String, String, i64, i64)> = conn
        .query_row(
            "SELECT accepted_root_id,accepted_capsule_hash,qualification_operation_id,qualification_occurrence_id,channel_binding_digest,phase,record_json,placement_thread_id,created_at_ms,updated_at_ms
             FROM external_qualification_local_start WHERE placement_thread_id=?1",
            [placement],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?)),
        )
        .optional()?;
    let Some((root, capsule, operation, occurrence, channel, phase, json, key, created, updated)) =
        row
    else {
        return Ok(None);
    };
    let record: QualificationLocalStartRecord = serde_json::from_str(&json)?;
    ensure!(
        record.placement_thread_id == key
            && key == placement
            && record.accepted_root_id == root
            && record.accepted_capsule_hash == capsule
            && record.qualification_operation_id == operation
            && record.qualification_occurrence_id == occurrence
            && record.channel_binding_digest == channel
            && record.phase.as_str() == phase
            && record.created_at_ms == created
            && record.updated_at_ms == updated
            && canonical(&record)? == json,
        "qualification local start row contradicts its canonical record"
    );
    require_retained_local_owner(conn, &record)?;
    Ok(Some(record))
}

fn require_retained_local_owner(
    conn: &Connection,
    record: &QualificationLocalStartRecord,
) -> Result<()> {
    let allocation = read(conn, &record.placement_thread_id)?
        .context("qualification local start lost its allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("qualification local start lost its Q owner");
    };
    ensure!(
        owner.coordinate.accepted_root_id == record.accepted_root_id
            && owner.coordinate.accepted_capsule_hash == record.accepted_capsule_hash
            && owner.qualification_operation_id == record.qualification_operation_id
            && owner.qualification_occurrence.occurrence_id == record.qualification_occurrence_id
            && allocation.reservation.base_snapshot_hash == record.base_snapshot_hash,
        "qualification local start changed accepted owner or base"
    );
    let channel = load_binding(conn, &record.placement_thread_id)?;
    ensure!(
        channel.digest()? == record.channel_binding_digest
            && channel.execution_binding_hash == allocation.reservation.binding_hash
            && matches!(channel.execution_mode, ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {})
            && channel.candidate_export_max_bytes == 0,
        "qualification local start changed its Q channel"
    );
    Ok(())
}

fn require_current_start(conn: &Connection, record: &QualificationLocalStartRecord) -> Result<()> {
    ensure!(
        record.daemon_generation_id == daemon_generation_id(),
        "qualification local start is from another daemon generation; cleanup required"
    );
    let allocation = read(conn, &record.placement_thread_id)?
        .context("qualification local start lost its allocation")?;
    ensure!(
        allocation.phase == ExternalAllocationPhase::Bound,
        "qualification local start lost bound allocation"
    );
    require_contactable_qualification_owner(conn, &allocation.reservation)?;
    let channel = load_binding(conn, &record.placement_thread_id)?;
    ensure!(
        i64::try_from(lillux::time::timestamp_millis())? < channel.execution_deadline_ms,
        "qualification local start exceeded its released channel deadline"
    );
    let state: String = conn.query_row(
        "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
        [&record.placement_thread_id],
        |row| row.get(0),
    )?;
    ensure!(
        state == "running",
        "qualification local start lost released channel"
    );
    let released: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_frame WHERE binding_digest=?1
         AND direction='owner_to_supervisor' AND json_extract(frame_json,'$.frame.payload.kind')='release')",
        [&record.channel_binding_digest], |row| row.get(0),
    )?;
    ensure!(
        released,
        "qualification local start has no released channel"
    );
    Ok(())
}

fn transition(
    conn: &Connection,
    current: &QualificationLocalStartRecord,
    next: &QualificationLocalStartRecord,
) -> Result<()> {
    let prior = canonical(current)?;
    let next_json = canonical(next)?;
    ensure!(
        conn.execute(
            "UPDATE external_qualification_local_start SET phase=?3,record_json=?4,updated_at_ms=?5
             WHERE placement_thread_id=?1 AND record_json=?2 AND phase=?6",
            params![
                current.placement_thread_id,
                prior,
                next.phase.as_str(),
                next_json,
                next.updated_at_ms,
                current.phase.as_str()
            ],
        )? == 1,
        "qualification local start lost its exact one-use transition"
    );
    Ok(())
}

fn now_after(current: &QualificationLocalStartRecord) -> Result<i64> {
    Ok(i64::try_from(lillux::time::timestamp_millis())?.max(current.updated_at_ms))
}

fn require_local_processes_settled(
    conn: &Connection,
    record: &QualificationLocalStartRecord,
) -> Result<()> {
    ensure!(
        record.directory_borrow_intent.is_none() || record.worker_incarnation_digest.is_some(),
        "qualification directory borrow has unproved Worker attachment or death"
    );
    let worker: Option<(String, String)> = conn
        .query_row(
            "SELECT phase,process_identity_json FROM external_qualification_worker_process WHERE placement_thread_id=?1",
            [&record.placement_thread_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match worker {
        Some((phase, json)) => {
            let identity: crate::process::ExecutionProcessIdentity = serde_json::from_str(&json)?;
            ensure!(
                phase == "terminal"
                    && identity.process_scope.is_none()
                    && record.worker_incarnation_digest.as_deref()
                        == Some(identity.owner_incarnation_digest()?.as_str()),
                "qualification local cleanup lost exact terminal Worker evidence"
            );
        }
        None => ensure!(
            record.worker_incarnation_digest.is_none(),
            "qualification local cleanup lost its attached Worker"
        ),
    }
    let connector: Option<String> = conn
        .query_row(
            "SELECT state FROM external_execution_connector WHERE placement_thread_id=?1",
            [&record.placement_thread_id],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        connector.as_deref().is_none_or(|state| state == "closed"),
        "qualification local cleanup retains a live connector"
    );
    Ok(())
}

fn cleanup_cas(
    conn: &Connection,
    expected: &QualificationLocalStartRecord,
    from: QualificationLocalStartPhase,
    to: QualificationLocalStartPhase,
    mutate: impl FnOnce(&Connection, &mut QualificationLocalStartRecord, i64) -> Result<()>,
) -> Result<QualificationLocalStartRecord> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let current = read_local_start(&tx, &expected.placement_thread_id)?
        .context("qualification local cleanup lost its original row")?;
    ensure!(
        current == *expected && current.phase == from,
        "qualification local cleanup lost its exact phase CAS"
    );
    require_local_processes_settled(&tx, &current)?;
    let mut next = current.clone();
    let now = now_after(&current)?;
    next.phase = to;
    next.updated_at_ms = now;
    mutate(&tx, &mut next, now)?;
    transition(&tx, &current, &next)?;
    tx.commit()?;
    Ok(next)
}

pub(super) fn validate_local_starts(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare("SELECT placement_thread_id FROM external_qualification_local_start ORDER BY placement_thread_id")?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        let record =
            read_local_start(conn, &placement)?.context("qualification local start disappeared")?;
        if matches!(
            record.phase,
            QualificationLocalStartPhase::CleanupIntent
                | QualificationLocalStartPhase::WorkspaceRemoved
                | QualificationLocalStartPhase::StateRemoved
                | QualificationLocalStartPhase::Closed
        ) {
            require_local_processes_settled(conn, &record)?;
        }
        let process: Option<String> = conn.query_row(
            "SELECT process_identity_json FROM external_qualification_worker_process WHERE placement_thread_id=?1",
            [&placement], |row| row.get(0),
        ).optional()?;
        match process {
            Some(json) => {
                let identity: crate::process::ExecutionProcessIdentity =
                    serde_json::from_str(&json)?;
                ensure!(
                    matches!(
                        record.phase,
                        QualificationLocalStartPhase::WorkerAttached
                            | QualificationLocalStartPhase::CleanupUnproved
                            | QualificationLocalStartPhase::CleanupIntent
                            | QualificationLocalStartPhase::WorkspaceRemoved
                            | QualificationLocalStartPhase::StateRemoved
                            | QualificationLocalStartPhase::Closed
                    ) && record.worker_incarnation_digest.as_deref()
                        == Some(identity.owner_incarnation_digest()?.as_str())
                        && identity.process_scope.is_none(),
                    "qualification local start lost its exact Worker attachment"
                );
            }
            None => ensure!(
                record.phase != QualificationLocalStartPhase::WorkerAttached
                    && record.worker_incarnation_digest.is_none(),
                "qualification local start has no attached Worker"
            ),
        }
    }
    Ok(())
}

pub(super) fn attach_local_worker_tx(
    tx: &Connection,
    placement: &str,
    identity: &crate::process::ExecutionProcessIdentity,
    daemon: &str,
) -> Result<()> {
    let current =
        read_local_start(tx, placement)?.context("qualification Worker has no Q local start")?;
    ensure!(
        current.phase == QualificationLocalStartPhase::WorkspaceReady
            && current.directory_borrow_intent.is_some()
            && current.daemon_generation_id == daemon
            && identity.process_scope.is_none(),
        "qualification Worker changed its one-use original directory"
    );
    require_current_start(tx, &current)?;
    let mut next = current.clone();
    next.worker_incarnation_digest = Some(identity.owner_incarnation_digest()?);
    next.phase = QualificationLocalStartPhase::WorkerAttached;
    next.updated_at_ms = now_after(&current)?;
    transition(tx, &current, &next)
}

impl RuntimeDb {
    pub(crate) fn qualification_local_start(
        &self,
        placement: &str,
    ) -> Result<Option<QualificationLocalStartRecord>> {
        read_local_start(&self.conn, placement)
    }

    /// Read-only final pre-spawn gate. The app caller holds the Q occurrence
    /// contact gate from this check through held attachment; this projection
    /// itself grants neither a descriptor nor process contact.
    pub(crate) fn authorize_qualification_local_spawn(
        &self,
        placement: &str,
    ) -> Result<QualificationLocalStartRecord> {
        let record = read_local_start(&self.conn, placement)?
            .context("qualification local spawn has no original start")?;
        ensure!(
            record.phase == QualificationLocalStartPhase::WorkspaceReady
                && record.directory_borrow_intent.is_some(),
            "qualification local spawn requires its one-use original directory borrow"
        );
        require_current_start(&self.conn, &record)?;
        Ok(record)
    }

    /// Commit one possible descriptor borrow before cloning the original
    /// workspace directory. A failed clone consumes this attempt and requires
    /// cleanup; it must never be retried with a replacement directory or process.
    pub(crate) fn reserve_qualification_workspace_directory_borrow(
        &self,
        placement: &str,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, placement)?
            .context("qualification workspace borrow lost its start")?;
        ensure!(
            current.phase == QualificationLocalStartPhase::WorkspaceReady
                && current.directory_borrow_intent.is_none(),
            "qualification workspace directory borrow is one-use"
        );
        require_current_start(&tx, &current)?;
        let identity = current
            .workspace_identity
            .context("qualification workspace borrow has no original directory identity")?;
        let mut next = current.clone();
        next.updated_at_ms = now_after(&current)?;
        next.directory_borrow_intent = Some(QualificationWorkspaceBorrowIntent {
            workspace_id: current.workspace_id.clone(),
            directory_identity: identity,
            reserved_at_ms: next.updated_at_ms,
        });
        let prior = canonical(&current)?;
        let next_json = canonical(&next)?;
        ensure!(
            tx.execute(
                "UPDATE external_qualification_local_start SET record_json=?3,updated_at_ms=?4
                 WHERE placement_thread_id=?1 AND record_json=?2 AND phase='workspace_ready'",
                params![placement, prior, next_json, next.updated_at_ms],
            )? == 1,
            "qualification workspace borrow lost its one-use CAS"
        );
        tx.commit()?;
        Ok(next)
    }

    /// Reserve before creating a directory. Exact replay is refused even when
    /// the first attempt may have stopped before its first filesystem effect.
    pub(crate) fn reserve_qualification_local_start(
        &self,
        placement: &str,
        runtime_cache_root: &std::path::Path,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        ensure!(
            read_local_start(&tx, placement)?.is_none(),
            "qualification local start already reserved"
        );
        let allocation =
            read(&tx, placement)?.context("qualification local start has no allocation")?;
        ensure!(
            allocation.phase == ExternalAllocationPhase::Bound,
            "qualification local start requires bound allocation"
        );
        require_contactable_qualification_owner(&tx, &allocation.reservation)?;
        let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
        else {
            bail!("qualification local start requires Q owner");
        };
        let channel = load_binding(&tx, placement)?;
        let channel_digest = channel.digest()?;
        let released: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_frame WHERE binding_digest=?1
             AND direction='owner_to_supervisor' AND json_extract(frame_json,'$.frame.payload.kind')='release')",
            [&channel_digest], |row| row.get(0),
        )?;
        let channel_state: String = tx.query_row(
            "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
            [placement],
            |row| row.get(0),
        )?;
        ensure!(released && channel_state == "running"
            && channel.execution_binding_hash == allocation.reservation.binding_hash
            && matches!(channel.execution_mode, ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {})
            && channel.candidate_export_max_bytes == 0
            && i64::try_from(lillux::time::timestamp_millis())? < channel.execution_deadline_ms,
            "qualification local start requires released zero-export Q channel");
        ensure!(
            tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM external_execution_connector WHERE placement_thread_id=?1)",
                [placement], |row| row.get::<_, bool>(0))?
                && tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM external_qualification_worker_process WHERE placement_thread_id=?1)",
                    [placement], |row| row.get::<_, bool>(0))?,
            "qualification local start cannot replace connector or Worker"
        );
        ensure!(
            runtime_cache_root.is_absolute(),
            "qualification local workspace root must be absolute"
        );
        let digest = local_workspace_digest(
            placement,
            &owner.coordinate.accepted_root_id,
            &owner.coordinate.accepted_capsule_hash,
            &owner.qualification_operation_id,
            &owner.qualification_occurrence.occurrence_id,
            &channel_digest,
            daemon_generation_id(),
        )?;
        let workspace_id = format!("QW-{digest}");
        let launch_owner = format!("Q-local-{digest}");
        let workspace_root = runtime_cache_root.join("executions").join(&workspace_id);
        let state_root = runtime_cache_root
            .join("qualification-local")
            .join(&workspace_id);
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let record = QualificationLocalStartRecord {
            placement_thread_id: placement.to_owned(),
            accepted_root_id: owner.coordinate.accepted_root_id.clone(),
            accepted_capsule_hash: owner.coordinate.accepted_capsule_hash.clone(),
            qualification_operation_id: owner.qualification_operation_id.clone(),
            qualification_occurrence_id: owner.qualification_occurrence.occurrence_id.clone(),
            channel_binding_digest: channel_digest,
            daemon_generation_id: daemon_generation_id().to_owned(),
            base_snapshot_hash: allocation.reservation.base_snapshot_hash.clone(),
            workspace_id,
            workspace_root: workspace_root.to_string_lossy().into_owned(),
            state_root: state_root.to_string_lossy().into_owned(),
            launch_owner,
            phase: QualificationLocalStartPhase::Reserved,
            workspace_identity: None,
            directory_borrow_intent: None,
            worker_incarnation_digest: None,
            cleanup_reason: None,
            cleanup: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let json = canonical(&record)?;
        tx.execute(
            "INSERT INTO external_qualification_local_start VALUES (?1,?2,?3,?4,?5,?6,'reserved',?7,?8,?8)",
            params![record.placement_thread_id, record.accepted_root_id, record.accepted_capsule_hash,
                record.qualification_operation_id, record.qualification_occurrence_id,
                record.channel_binding_digest, json, now],
        )?;
        tx.commit()?;
        Ok(record)
    }

    /// Retain the original populated project child identity before any Worker
    /// can borrow it. A surviving reservation never grants replacement creation.
    pub(crate) fn complete_qualification_private_workspace(
        &self,
        placement: &str,
        identity: lillux::PinnedDirectoryIdentity,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, placement)?
            .context("qualification private workspace lost its reservation")?;
        ensure!(
            current.phase == QualificationLocalStartPhase::Reserved,
            "qualification private workspace completion is one-use"
        );
        require_current_start(&tx, &current)?;
        let mut next = current.clone();
        next.workspace_identity = Some(identity);
        next.phase = QualificationLocalStartPhase::WorkspaceReady;
        next.updated_at_ms = now_after(&current)?;
        transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    /// Fence every unresolved side effect. This transition proves no cleanup.
    pub(crate) fn mark_qualification_local_cleanup_unproved(
        &self,
        placement: &str,
        reason: &str,
    ) -> Result<QualificationLocalStartRecord> {
        validate_bounded_runtime_text("Q local cleanup reason", reason, 1024)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, placement)?
            .context("qualification local cleanup has no start")?;
        ensure!(
            matches!(
                current.phase,
                QualificationLocalStartPhase::Reserved
                    | QualificationLocalStartPhase::WorkspaceReady
                    | QualificationLocalStartPhase::WorkerAttached
            ),
            "qualification local cleanup fence is already retained or positive cleanup began"
        );
        let mut next = current.clone();
        next.phase = QualificationLocalStartPhase::CleanupUnproved;
        next.cleanup_reason = Some(reason.to_owned());
        next.updated_at_ms = now_after(&current)?;
        transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    /// Fence all new local contact before any physical cleanup. The caller's
    /// StateStore wrapper authenticates process-local original custody. Worker
    /// and connector settlement must precede this permission for removal.
    pub(crate) fn begin_qualification_local_cleanup(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        self.begin_qualification_local_cleanup_inner(expected, false)
    }

    /// The pre-registration rollback owns both original roots but has never
    /// authorized a directory borrow, local Worker, or connector. This strict
    /// absence check is atomic with the no-contact cleanup intent.
    pub(crate) fn begin_unregistered_qualification_precreate_rollback(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        self.begin_qualification_local_cleanup_inner(expected, true)
    }

    fn begin_qualification_local_cleanup_inner(
        &self,
        expected: &QualificationLocalStartRecord,
        require_unregistered_precreate: bool,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, &expected.placement_thread_id)?
            .context("qualification cleanup lost its retained start")?;
        ensure!(
            current == *expected,
            "qualification cleanup changed its original custody row"
        );
        if require_unregistered_precreate {
            ensure!(
                current.phase == QualificationLocalStartPhase::Reserved,
                "unregistered Q rollback requires its original directory reservation"
            );
            for table in [
                "external_qualification_worker_process",
                "external_execution_connector",
            ] {
                let exists: bool = tx.query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE placement_thread_id=?1)"),
                    [&current.placement_thread_id],
                    |row| row.get(0),
                )?;
                ensure!(
                    !exists,
                    "unregistered Q rollback has local process or connector contact"
                );
            }
        }
        ensure!(
            matches!(
                current.phase,
                QualificationLocalStartPhase::Reserved
                    | QualificationLocalStartPhase::WorkspaceReady
                    | QualificationLocalStartPhase::WorkerAttached
                    | QualificationLocalStartPhase::CleanupUnproved
            ),
            "qualification cleanup already began or closed"
        );
        require_local_processes_settled(&tx, &current)?;
        let now = now_after(&current)?;
        let mut next = current.clone();
        next.phase = QualificationLocalStartPhase::CleanupIntent;
        next.cleanup_reason = None;
        next.cleanup = Some(QualificationLocalCleanupEvidence {
            prior_failure_reason: current.cleanup_reason.clone(),
            intent_at_ms: now,
            workspace_removed_at_ms: None,
            state_removed_at_ms: None,
            closed_at_ms: None,
        });
        next.updated_at_ms = now;
        transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    /// Recheck exact retained custody and settled local processes immediately
    /// before the wrapper removes its original pinned workspace directory.
    pub(crate) fn authorize_qualification_workspace_removal(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, &expected.placement_thread_id)?
            .context("qualification workspace removal lost its original row")?;
        ensure!(
            current == *expected && current.phase == QualificationLocalStartPhase::CleanupIntent,
            "qualification workspace removal requires its exact cleanup intent"
        );
        require_local_processes_settled(&tx, &current)?;
        tx.commit()?;
        Ok(())
    }

    /// The StateStore wrapper removes the original pinned workspace directory
    /// before recording this exact receipt. CleanupIntent already fences spawn.
    pub(crate) fn record_qualification_workspace_removed(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        cleanup_cas(
            &self.conn,
            expected,
            QualificationLocalStartPhase::CleanupIntent,
            QualificationLocalStartPhase::WorkspaceRemoved,
            |_, next, now| {
                next.cleanup.as_mut().unwrap().workspace_removed_at_ms = Some(now);
                Ok(())
            },
        )
    }

    pub(crate) fn record_qualification_state_root_removed(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        cleanup_cas(
            &self.conn,
            expected,
            QualificationLocalStartPhase::WorkspaceRemoved,
            QualificationLocalStartPhase::StateRemoved,
            |_, next, now| {
                next.cleanup.as_mut().unwrap().state_removed_at_ms = Some(now);
                Ok(())
            },
        )
    }

    pub(crate) fn close_qualification_local_start(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        cleanup_cas(
            &self.conn,
            expected,
            QualificationLocalStartPhase::StateRemoved,
            QualificationLocalStartPhase::Closed,
            |tx, next, now| {
                require_local_processes_settled(tx, next)?;
                next.cleanup.as_mut().unwrap().closed_at_ms = Some(now);
                Ok(())
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_record() -> QualificationLocalStartRecord {
        let placement = format!("Q-{}", "1".repeat(64));
        let root = "T-qualification-root";
        let capsule = "2".repeat(64);
        let operation = "3".repeat(64);
        let channel = "4".repeat(64);
        let occurrence = "qualification-occurrence";
        let daemon = "original-daemon";
        let digest = local_workspace_digest(
            &placement, root, &capsule, &operation, occurrence, &channel, daemon,
        )
        .unwrap();
        let workspace_id = format!("QW-{digest}");
        QualificationLocalStartRecord {
            placement_thread_id: placement,
            accepted_root_id: root.to_owned(),
            accepted_capsule_hash: capsule,
            qualification_operation_id: operation,
            qualification_occurrence_id: occurrence.to_owned(),
            channel_binding_digest: channel,
            daemon_generation_id: daemon.to_owned(),
            base_snapshot_hash: "5".repeat(64),
            workspace_root: format!("/tmp/qualification-cache/executions/{workspace_id}"),
            state_root: format!("/tmp/qualification-cache/qualification-local/{workspace_id}"),
            workspace_id,
            launch_owner: format!("Q-local-{digest}"),
            phase: QualificationLocalStartPhase::Reserved,
            workspace_identity: None,
            directory_borrow_intent: None,
            worker_incarnation_digest: None,
            cleanup_reason: None,
            cleanup: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn directory_identity(inode: u64) -> lillux::PinnedDirectoryIdentity {
        serde_json::from_value(serde_json::json!({
            "containing_device": 1, "inode": inode,
        }))
        .unwrap()
    }

    #[test]
    fn borrowed_directory_without_attached_worker_cannot_authorize_cleanup() {
        let mut record = fixture_record();
        record.phase = QualificationLocalStartPhase::WorkspaceReady;
        record.workspace_identity = Some(directory_identity(2));
        record.directory_borrow_intent = Some(QualificationWorkspaceBorrowIntent {
            workspace_id: record.workspace_id.clone(),
            directory_identity: record.workspace_identity.unwrap(),
            reserved_at_ms: 1,
        });
        record.validate().unwrap();
        // Even a database with no Worker table cannot turn possible held spawn
        // into evidence of no process: the durable borrow fences cleanup first.
        let conn = Connection::open_in_memory().unwrap();
        let error = require_local_processes_settled(&conn, &record).unwrap_err();
        assert!(error.to_string().contains("unproved Worker"));
        record.phase = QualificationLocalStartPhase::CleanupIntent;
        record.cleanup = Some(QualificationLocalCleanupEvidence {
            prior_failure_reason: None,
            intent_at_ms: 1,
            workspace_removed_at_ms: None,
            state_removed_at_ms: None,
            closed_at_ms: None,
        });
        assert!(record.validate().is_err());
    }

    #[test]
    fn private_directory_journal_pins_identity_and_allows_one_borrow() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE external_execution_allocation(placement_thread_id TEXT PRIMARY KEY, phase TEXT, reservation_json TEXT);
             CREATE TABLE external_execution_channel(placement_thread_id TEXT PRIMARY KEY, binding_digest TEXT, state TEXT);",
        ).unwrap();
        conn.execute_batch(JOURNAL_SQL).unwrap();
        let mut current = fixture_record();
        let reservation = serde_json::json!({"owner": {
            "kind": "qualification_candidate",
            "coordinate": {"accepted_root_id": current.accepted_root_id,
                "accepted_capsule_hash": current.accepted_capsule_hash},
            "qualification_operation_id": current.qualification_operation_id,
            "qualification_occurrence": {"occurrence_id": current.qualification_occurrence_id},
        }});
        conn.execute(
            "INSERT INTO external_execution_allocation VALUES(?1,'bound',?2)",
            params![current.placement_thread_id, reservation.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO external_execution_channel VALUES(?1,?2,'running')",
            params![current.placement_thread_id, current.channel_binding_digest],
        )
        .unwrap();
        conn.execute("INSERT INTO external_qualification_local_start VALUES(?1,?2,?3,?4,?5,?6,'reserved',?7,1,1)",
            params![current.placement_thread_id, current.accepted_root_id, current.accepted_capsule_hash,
                current.qualification_operation_id, current.qualification_occurrence_id,
                current.channel_binding_digest, canonical(&current).unwrap()]).unwrap();
        let mut ready = current.clone();
        ready.phase = QualificationLocalStartPhase::WorkspaceReady;
        ready.workspace_identity = Some(directory_identity(2));
        transition(&conn, &current, &ready).unwrap();
        current = ready;
        let mut borrowed = current.clone();
        borrowed.directory_borrow_intent = Some(QualificationWorkspaceBorrowIntent {
            workspace_id: current.workspace_id.clone(),
            directory_identity: current.workspace_identity.unwrap(),
            reserved_at_ms: 1,
        });
        transition(&conn, &current, &borrowed).unwrap();
        assert!(transition(&conn, &borrowed, &borrowed).is_err());
        let mut replaced = borrowed.clone();
        replaced.workspace_identity = Some(directory_identity(3));
        replaced
            .directory_borrow_intent
            .as_mut()
            .unwrap()
            .directory_identity = directory_identity(3);
        assert!(transition(&conn, &borrowed, &replaced).is_err());
        let mut skipped = borrowed.clone();
        skipped.phase = QualificationLocalStartPhase::WorkspaceRemoved;
        assert!(transition(&conn, &borrowed, &skipped).is_err());
    }
}
