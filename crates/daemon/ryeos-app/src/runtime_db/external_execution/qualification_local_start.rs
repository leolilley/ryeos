//! One local, pre-kernel start attempt for a bound Q allocation.
//!
//! This is a journal of intent and exact receipts. It cannot recreate the
//! original workspace view or a launchable process scope after a crash. A
//! surviving row therefore grants cleanup/recovery only, never another Create,
//! allocation, or Worker launch.

use super::*;
use ryeos_state::external_execution::journal::load_binding;

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_qualification_local_start (
    placement_thread_id TEXT PRIMARY KEY REFERENCES external_execution_allocation(placement_thread_id),
    accepted_root_id TEXT NOT NULL,
    accepted_capsule_hash TEXT NOT NULL,
    qualification_operation_id TEXT NOT NULL,
    qualification_occurrence_id TEXT NOT NULL,
    channel_binding_digest TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN
        ('reserved','workspace_creating','workspace_ready','scope_allocating',
         'scope_bound','worker_attached','cleanup_unproved','cleanup_intent',
         'scope_retirement_pending','scope_retired','view_closed',
         'destroy_pending','destroyed','workspace_removed','state_removed','closed')),
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
 OR (NEW.phase=OLD.phase AND NOT (
      (OLD.phase='workspace_creating'
       AND json_extract(OLD.record_json,'$.creator_identity') IS NULL
       AND json_extract(NEW.record_json,'$.creator_identity') IS NOT NULL
       AND json_remove(NEW.record_json,'$.creator_identity','$.updated_at_ms')
           = json_remove(OLD.record_json,'$.creator_identity','$.updated_at_ms'))
      OR (OLD.phase='scope_bound'
       AND json_extract(OLD.record_json,'$.view_borrow_intent') IS NULL
       AND json_extract(NEW.record_json,'$.view_borrow_intent') IS NOT NULL
       AND json_remove(NEW.record_json,'$.view_borrow_intent','$.updated_at_ms')
           = json_remove(OLD.record_json,'$.view_borrow_intent','$.updated_at_ms'))))
 OR NOT ((OLD.phase='reserved' AND NEW.phase='workspace_creating')
      OR (OLD.phase='workspace_creating' AND NEW.phase='workspace_ready')
      OR (OLD.phase='workspace_ready' AND NEW.phase='scope_allocating')
      OR (OLD.phase='scope_allocating' AND NEW.phase='scope_bound')
      OR (OLD.phase='scope_bound' AND NEW.phase='worker_attached')
      OR (OLD.phase='workspace_creating' AND NEW.phase='workspace_creating')
      OR (OLD.phase='scope_bound' AND NEW.phase='scope_bound')
      OR (OLD.phase IN ('reserved','workspace_creating','workspace_ready',
                       'scope_allocating','scope_bound','worker_attached')
          AND NEW.phase='cleanup_unproved')
      OR (OLD.phase IN ('reserved','workspace_ready','scope_allocating',
                       'scope_bound','worker_attached','cleanup_unproved')
          AND NEW.phase='cleanup_intent')
      OR (OLD.phase='cleanup_intent' AND NEW.phase IN ('scope_retirement_pending','workspace_removed'))
      OR (OLD.phase='scope_retirement_pending' AND NEW.phase='scope_retired')
      OR (OLD.phase='scope_retired' AND NEW.phase='view_closed')
      OR (OLD.phase='view_closed' AND NEW.phase='destroy_pending')
      OR (OLD.phase='destroy_pending' AND NEW.phase='destroyed')
      OR (OLD.phase='destroyed' AND NEW.phase='workspace_removed')
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
    WorkspaceCreating,
    WorkspaceReady,
    ScopeAllocating,
    ScopeBound,
    WorkerAttached,
    CleanupUnproved,
    CleanupIntent,
    ScopeRetirementPending,
    ScopeRetired,
    ViewClosed,
    DestroyPending,
    Destroyed,
    WorkspaceRemoved,
    StateRemoved,
    Closed,
}

impl QualificationLocalStartPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::WorkspaceCreating => "workspace_creating",
            Self::WorkspaceReady => "workspace_ready",
            Self::ScopeAllocating => "scope_allocating",
            Self::ScopeBound => "scope_bound",
            Self::WorkerAttached => "worker_attached",
            Self::CleanupUnproved => "cleanup_unproved",
            Self::CleanupIntent => "cleanup_intent",
            Self::ScopeRetirementPending => "scope_retirement_pending",
            Self::ScopeRetired => "scope_retired",
            Self::ViewClosed => "view_closed",
            Self::DestroyPending => "destroy_pending",
            Self::Destroyed => "destroyed",
            Self::WorkspaceRemoved => "workspace_removed",
            Self::StateRemoved => "state_removed",
            Self::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkspaceReceipt {
    pub backend_id: String,
    pub backend_version: String,
    pub pinned_root_identities: BTreeMap<String, String>,
    pub mount_identity: String,
    pub view_identity: String,
}

/// One durable intent to lend the original, exact Create view to a held
/// Worker. The intent precedes descriptor cloning and cannot be replayed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkspaceBorrowIntent {
    pub workspace_id: String,
    pub view_identity: String,
    pub reserved_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum QualificationLocalCleanupRoute {
    NoCreate,
    Created,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationWorkspaceDestroyReceipt {
    pub backend_id: String,
    pub backend_version: String,
    pub pinned_root_identities: BTreeMap<String, String>,
    pub mount_identity: String,
    pub destroyed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationLocalCleanupEvidence {
    pub route: QualificationLocalCleanupRoute,
    pub prior_failure_reason: Option<String>,
    pub intent_at_ms: i64,
    pub scope_retirement_intent_at_ms: Option<i64>,
    pub scope_retired_at_ms: Option<i64>,
    pub view_closed_at_ms: Option<i64>,
    pub destroy_intent_at_ms: Option<i64>,
    pub destroy_receipt: Option<QualificationWorkspaceDestroyReceipt>,
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
    /// Diagnostic path only; the original TempDirGuard is the view authority.
    pub workspace_root: String,
    /// Diagnostic coordinate for a separate one-use pinned child under cache.
    /// The original pinned descriptor is the Worker state-root authority.
    pub state_root: String,
    pub launch_owner: String,
    pub phase: QualificationLocalStartPhase,
    pub backend_id: Option<String>,
    pub backend_version: Option<String>,
    pub creator_identity: Option<crate::process::ExecutionProcessIdentity>,
    pub workspace_receipt: Option<QualificationWorkspaceReceipt>,
    pub view_borrow_intent: Option<QualificationWorkspaceBorrowIntent>,
    pub scope_allocation: Option<lillux::ProcessScopeAllocation>,
    pub scope_recovery: Option<lillux::ProcessScopeRecovery>,
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
        if let Some(identity) = &self.creator_identity {
            crate::process::validate_execution_process_identity_shape(identity)?;
        }
        if let Some(allocation) = &self.scope_allocation {
            allocation.validate().map_err(anyhow::Error::msg)?;
        }
        if let Some(recovery) = &self.scope_recovery {
            ensure!(
                self.scope_allocation
                    .as_ref()
                    .is_some_and(|allocation| recovery.matches_allocation(allocation)),
                "qualification local scope recovery changed its allocation"
            );
        }
        if let Some(receipt) = &self.workspace_receipt {
            ensure!(
                self.backend_id.as_deref() == Some(receipt.backend_id.as_str())
                    && self.backend_version.as_deref() == Some(receipt.backend_version.as_str())
                    && !receipt.mount_identity.is_empty()
                    && receipt.mount_identity == receipt.view_identity,
                "qualification local view changed its selected backend or mount"
            );
        }
        if let Some(intent) = &self.view_borrow_intent {
            let receipt = self
                .workspace_receipt
                .as_ref()
                .context("qualification view borrow lost its exact Create receipt")?;
            ensure!(
                intent.workspace_id == self.workspace_id
                    && intent.view_identity == receipt.view_identity
                    && intent.reserved_at_ms >= self.created_at_ms
                    && intent.reserved_at_ms <= self.updated_at_ms,
                "qualification view borrow changed its original view or time"
            );
        }
        if let Some(digest) = &self.worker_incarnation_digest {
            validate_sha256("local Worker incarnation", digest)?;
        }
        if let Some(reason) = &self.cleanup_reason {
            validate_bounded_runtime_text("local cleanup reason", reason, 1024)?;
        }
        ensure!(
            match self.phase {
                QualificationLocalStartPhase::Reserved =>
                    self.backend_id.is_none()
                        && self.backend_version.is_none()
                        && self.creator_identity.is_none()
                        && self.workspace_receipt.is_none()
                        && self.view_borrow_intent.is_none()
                        && self.scope_allocation.is_none()
                        && self.scope_recovery.is_none()
                        && self.worker_incarnation_digest.is_none()
                        && self.cleanup.is_none(),
                QualificationLocalStartPhase::WorkspaceCreating =>
                    self.backend_id.is_some()
                        && self.backend_version.is_some()
                        && self.workspace_receipt.is_none()
                        && self.view_borrow_intent.is_none()
                        && self.scope_allocation.is_none()
                        && self.scope_recovery.is_none()
                        && self.worker_incarnation_digest.is_none()
                        && self.cleanup.is_none(),
                QualificationLocalStartPhase::WorkspaceReady =>
                    self.workspace_receipt.is_some()
                        && self.creator_identity.is_some()
                        && self.view_borrow_intent.is_none()
                        && self.scope_allocation.is_none()
                        && self.scope_recovery.is_none()
                        && self.worker_incarnation_digest.is_none()
                        && self.cleanup.is_none(),
                QualificationLocalStartPhase::ScopeAllocating =>
                    self.workspace_receipt.is_some()
                        && self.creator_identity.is_some()
                        && self.view_borrow_intent.is_none()
                        && self.scope_allocation.is_some()
                        && self.scope_recovery.is_none()
                        && self.worker_incarnation_digest.is_none()
                        && self.cleanup.is_none(),
                QualificationLocalStartPhase::ScopeBound =>
                    self.workspace_receipt.is_some()
                        && self.creator_identity.is_some()
                        && self.scope_allocation.is_some()
                        && self.scope_recovery.is_some()
                        && self.worker_incarnation_digest.is_none()
                        && self.cleanup.is_none(),
                QualificationLocalStartPhase::WorkerAttached =>
                    self.workspace_receipt.is_some()
                        && self.creator_identity.is_some()
                        && self.view_borrow_intent.is_some()
                        && self.scope_allocation.is_some()
                        && self.scope_recovery.is_some()
                        && self.worker_incarnation_digest.is_some()
                        && self.cleanup.is_none(),
                QualificationLocalStartPhase::CleanupUnproved => self.cleanup.is_none(),
                QualificationLocalStartPhase::CleanupIntent
                | QualificationLocalStartPhase::ScopeRetirementPending
                | QualificationLocalStartPhase::ScopeRetired
                | QualificationLocalStartPhase::ViewClosed
                | QualificationLocalStartPhase::DestroyPending
                | QualificationLocalStartPhase::Destroyed
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
        if let Some(reason) = &cleanup.prior_failure_reason {
            validate_bounded_runtime_text("prior local cleanup failure", reason, 1024)?;
        }
        let stage = match self.phase {
            QualificationLocalStartPhase::CleanupIntent => 0,
            QualificationLocalStartPhase::ScopeRetirementPending => 1,
            QualificationLocalStartPhase::ScopeRetired => 2,
            QualificationLocalStartPhase::ViewClosed => 3,
            QualificationLocalStartPhase::DestroyPending => 4,
            QualificationLocalStartPhase::Destroyed => 5,
            QualificationLocalStartPhase::WorkspaceRemoved => 6,
            QualificationLocalStartPhase::StateRemoved => 7,
            QualificationLocalStartPhase::Closed => 8,
            _ => bail!("qualification cleanup evidence has no cleanup phase"),
        };
        let created = cleanup.route == QualificationLocalCleanupRoute::Created;
        ensure!(
            !created || self.scope_allocation.is_none() || self.scope_recovery.is_some(),
            "qualification cleanup cannot retire an unbound process scope"
        );
        ensure!(
            (created && self.workspace_receipt.is_some() && self.creator_identity.is_some())
                || (!created
                    && self.workspace_receipt.is_none()
                    && self.backend_id.is_none()
                    && self.backend_version.is_none()
                    && self.creator_identity.is_none()
                    && self.view_borrow_intent.is_none()
                    && self.scope_allocation.is_none()
                    && self.scope_recovery.is_none()
                    && self.worker_incarnation_digest.is_none()
                    && !matches!(stage, 1..=5)),
            "qualification cleanup changed its Create route"
        );
        ensure!(
            cleanup.intent_at_ms >= self.created_at_ms
                && cleanup.intent_at_ms <= self.updated_at_ms
                && cleanup.scope_retirement_intent_at_ms.is_some() == (created && stage >= 1)
                && cleanup.scope_retired_at_ms.is_some() == (created && stage >= 2)
                && cleanup.view_closed_at_ms.is_some() == (created && stage >= 3)
                && cleanup.destroy_intent_at_ms.is_some() == (created && stage >= 4)
                && cleanup.destroy_receipt.is_some() == (created && stage >= 5)
                && cleanup.workspace_removed_at_ms.is_some() == (stage >= 6)
                && cleanup.state_removed_at_ms.is_some() == (stage >= 7)
                && cleanup.closed_at_ms.is_some() == (stage >= 8),
            "qualification cleanup phase lacks its exact ordered evidence"
        );
        let mut previous = cleanup.intent_at_ms;
        for instant in [
            cleanup.scope_retirement_intent_at_ms,
            cleanup.scope_retired_at_ms,
            cleanup.view_closed_at_ms,
            cleanup.destroy_intent_at_ms,
            cleanup
                .destroy_receipt
                .as_ref()
                .map(|receipt| receipt.destroyed_at_ms),
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
        if let Some(destroyed) = &cleanup.destroy_receipt {
            let created = self
                .workspace_receipt
                .as_ref()
                .context("qualification Destroy lost its original Create receipt")?;
            ensure!(
                destroyed.backend_id == created.backend_id
                    && destroyed.backend_version == created.backend_version
                    && destroyed.pinned_root_identities == created.pinned_root_identities
                    && destroyed.mount_identity == created.mount_identity,
                "qualification Destroy receipt changed its signed Create identity"
            );
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

fn read_local_start(
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

fn require_local_processes_settled(conn: &Connection, placement: &str) -> Result<()> {
    let worker: Option<String> = conn
        .query_row(
            "SELECT phase FROM external_qualification_worker_process WHERE placement_thread_id=?1",
            [placement],
            |row| row.get(0),
        )
        .optional()?;
    let connector: Option<String> = conn
        .query_row(
            "SELECT state FROM external_execution_connector WHERE placement_thread_id=?1",
            [placement],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        worker.as_deref().is_none_or(|phase| phase == "terminal")
            && connector.as_deref().is_none_or(|state| state == "closed"),
        "qualification local cleanup retains a live Worker or connector"
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
            QualificationLocalStartPhase::ScopeRetirementPending
                | QualificationLocalStartPhase::ScopeRetired
                | QualificationLocalStartPhase::ViewClosed
                | QualificationLocalStartPhase::DestroyPending
                | QualificationLocalStartPhase::Destroyed
                | QualificationLocalStartPhase::WorkspaceRemoved
                | QualificationLocalStartPhase::StateRemoved
                | QualificationLocalStartPhase::Closed
        ) {
            require_local_processes_settled(conn, &placement)?;
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
                            | QualificationLocalStartPhase::ScopeRetirementPending
                            | QualificationLocalStartPhase::ScopeRetired
                            | QualificationLocalStartPhase::ViewClosed
                            | QualificationLocalStartPhase::DestroyPending
                            | QualificationLocalStartPhase::Destroyed
                            | QualificationLocalStartPhase::WorkspaceRemoved
                            | QualificationLocalStartPhase::StateRemoved
                            | QualificationLocalStartPhase::Closed
                    ) && record.worker_incarnation_digest.as_deref()
                        == Some(identity.owner_incarnation_digest()?.as_str())
                        && record.scope_recovery.as_ref() == identity.process_scope.as_ref(),
                    "qualification local start lost its exact Worker attachment"
                );
            }
            None => ensure!(
                record.phase != QualificationLocalStartPhase::WorkerAttached,
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
        current.phase == QualificationLocalStartPhase::ScopeBound
            && current.view_borrow_intent.is_some()
            && current.daemon_generation_id == daemon
            && current.scope_recovery.as_ref() == identity.process_scope.as_ref(),
        "qualification Worker changed its one-use local scope"
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
            record.phase == QualificationLocalStartPhase::ScopeBound
                && record.view_borrow_intent.is_some(),
            "qualification local spawn requires its one-use original view borrow"
        );
        require_current_start(&self.conn, &record)?;
        Ok(record)
    }

    /// Commit one possible descriptor borrow before cloning the original
    /// workspace view. A failed clone consumes this attempt and requires
    /// cleanup; it must never be retried with a replacement view or process.
    pub(crate) fn reserve_qualification_workspace_view_borrow(
        &self,
        placement: &str,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, placement)?
            .context("qualification workspace borrow lost its start")?;
        ensure!(
            current.phase == QualificationLocalStartPhase::ScopeBound
                && current.view_borrow_intent.is_none(),
            "qualification workspace view borrow is one-use"
        );
        require_current_start(&tx, &current)?;
        let receipt = current
            .workspace_receipt
            .as_ref()
            .context("qualification workspace borrow has no original Create receipt")?;
        let mut next = current.clone();
        next.updated_at_ms = now_after(&current)?;
        next.view_borrow_intent = Some(QualificationWorkspaceBorrowIntent {
            workspace_id: current.workspace_id.clone(),
            view_identity: receipt.view_identity.clone(),
            reserved_at_ms: next.updated_at_ms,
        });
        let prior = canonical(&current)?;
        let next_json = canonical(&next)?;
        ensure!(
            tx.execute(
                "UPDATE external_qualification_local_start SET record_json=?3,updated_at_ms=?4
                 WHERE placement_thread_id=?1 AND record_json=?2 AND phase='scope_bound'",
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
            backend_id: None,
            backend_version: None,
            creator_identity: None,
            workspace_receipt: None,
            view_borrow_intent: None,
            scope_allocation: None,
            scope_recovery: None,
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

    /// Pin the selected signed backend before its first Create side effect.
    pub(crate) fn begin_qualification_workspace_create(
        &self,
        placement: &str,
        backend_id: &str,
        backend_version: &str,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, placement)?
            .context("qualification workspace has no reservation")?;
        ensure!(
            current.phase == QualificationLocalStartPhase::Reserved,
            "qualification workspace Create is one-use"
        );
        require_current_start(&tx, &current)?;
        validate_bounded_runtime_text("Q workspace backend", backend_id, 256)?;
        validate_bounded_runtime_text("Q workspace backend version", backend_version, 256)?;
        let mut next = current.clone();
        next.backend_id = Some(backend_id.to_owned());
        next.backend_version = Some(backend_version.to_owned());
        next.phase = QualificationLocalStartPhase::WorkspaceCreating;
        next.updated_at_ms = now_after(&current)?;
        transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    /// Called by the signed backend's held-creator callback before release.
    pub(crate) fn attach_qualification_workspace_creator(
        &self,
        placement: &str,
        identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, placement)?
            .context("qualification workspace has no Create intent")?;
        ensure!(
            current.phase == QualificationLocalStartPhase::WorkspaceCreating
                && current.creator_identity.is_none(),
            "qualification workspace creator is one-use"
        );
        require_current_start(&tx, &current)?;
        crate::process::validate_execution_process_identity_shape(identity)?;
        ensure!(
            identity.process_scope.is_none(),
            "workspace creator cannot consume Worker scope"
        );
        let mut next = current.clone();
        next.creator_identity = Some(identity.clone());
        next.updated_at_ms = now_after(&current)?;
        // Creator attachment is an in-phase, one-use field transition.
        let prior = canonical(&current)?;
        let next_json = canonical(&next)?;
        ensure!(
            tx.execute(
                "UPDATE external_qualification_local_start SET record_json=?3,updated_at_ms=?4
             WHERE placement_thread_id=?1 AND record_json=?2 AND phase='workspace_creating'",
                params![placement, prior, next_json, next.updated_at_ms],
            )? == 1,
            "qualification workspace creator lost Create intent"
        );
        tx.commit()?;
        Ok(next)
    }

    /// The caller must have installed the original Create view in `guard`.
    /// Exact creator whole-process death is checked before Ready is durable.
    pub(crate) fn complete_qualification_workspace_create(
        &self,
        placement: &str,
        evidence: &ryeos_engine::isolation::WorkspaceLifecycleEvidence,
        guard: &crate::temp_dir_guard::TempDirGuard,
    ) -> Result<QualificationLocalStartRecord> {
        let initial = read_local_start(&self.conn, placement)?
            .context("qualification workspace has no Create intent")?;
        ensure!(
            initial.phase == QualificationLocalStartPhase::WorkspaceCreating,
            "qualification workspace Create is not pending"
        );
        let creator = initial
            .creator_identity
            .as_ref()
            .context("qualification workspace has no held creator")?;
        crate::process::assert_reaped_process_group_absent(creator)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, placement)?
            .context("qualification workspace Create intent disappeared")?;
        ensure!(
            current == initial,
            "qualification workspace Create intent changed during reap check"
        );
        require_current_start(&tx, &current)?;
        ensure!(
            evidence.operation == ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create
                && evidence.workspace_id == current.workspace_id
                && evidence.launch_owner == current.launch_owner
                && evidence.backend_id == current.backend_id.as_deref().unwrap_or("")
                && evidence.backend_version == current.backend_version.as_deref().unwrap_or("")
                && !evidence.destroyed
                && evidence.mutations.is_empty(),
            "qualification workspace Create evidence changed its signed backend or owner"
        );
        let mount = evidence
            .mount_identity
            .as_ref()
            .context("qualification workspace Create has no mount identity")?;
        let (view_workspace, view_identity) = guard
            .workspace_view_identity()?
            .context("qualification workspace Create has no original installed view")?;
        let workspace_root = std::path::Path::new(&current.workspace_root);
        ensure!(
            view_workspace == current.workspace_id
                && view_identity == *mount
                && guard.path().as_deref() == Some(workspace_root)
                && guard.owns_effective_path(&workspace_root.join("project")),
            "qualification workspace view is not the original exact Create result"
        );
        let mut next = current.clone();
        next.workspace_receipt = Some(QualificationWorkspaceReceipt {
            backend_id: evidence.backend_id.clone(),
            backend_version: evidence.backend_version.clone(),
            pinned_root_identities: evidence.pinned_root_identities.clone(),
            mount_identity: mount.clone(),
            view_identity,
        });
        next.phase = QualificationLocalStartPhase::WorkspaceReady;
        next.updated_at_ms = now_after(&current)?;
        transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    /// Commit the Lillux allocation intent before allocating a kernel scope.
    pub(crate) fn begin_qualification_scope_allocation(
        &self,
        placement: &str,
        allocation: &lillux::ProcessScopeAllocation,
    ) -> Result<QualificationLocalStartRecord> {
        allocation.validate().map_err(anyhow::Error::msg)?;
        let lifetime = allocation.host_lifetime().map_err(anyhow::Error::msg)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current =
            read_local_start(&tx, placement)?.context("qualification scope has no workspace")?;
        ensure!(
            current.phase == QualificationLocalStartPhase::WorkspaceReady,
            "qualification process scope allocation is one-use"
        );
        require_current_start(&tx, &current)?;
        if let Some(incumbent) = super::super::read_scope_lifetime_fence(&tx)? {
            ensure!(
                incumbent == lifetime || incumbent.has_ended().map_err(anyhow::Error::msg)?,
                "qualification scope requires retirement of prior host lifetime"
            );
        }
        let encoded_lifetime = lillux::canonical_json(&serde_json::to_value(&lifetime)?)?;
        tx.execute(
            "UPDATE execution_lifetime_fence SET host_lifetime=?1 WHERE singleton=1",
            [&encoded_lifetime],
        )?;
        let mut next = current.clone();
        next.scope_allocation = Some(allocation.clone());
        next.phase = QualificationLocalStartPhase::ScopeAllocating;
        next.updated_at_ms = now_after(&current)?;
        transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    pub(crate) fn bind_qualification_scope(
        &self,
        placement: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, placement)?
            .context("qualification scope has no allocation intent")?;
        ensure!(
            current.phase == QualificationLocalStartPhase::ScopeAllocating
                && current
                    .scope_allocation
                    .as_ref()
                    .is_some_and(|allocation| recovery.matches_allocation(allocation)),
            "qualification scope recovery changed or replayed its allocation"
        );
        require_current_start(&tx, &current)?;
        let mut next = current.clone();
        next.scope_recovery = Some(recovery.clone());
        next.phase = QualificationLocalStartPhase::ScopeBound;
        next.updated_at_ms = now_after(&current)?;
        transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    /// Preserve the exact kernel scope recovery coordinate when binding it
    /// as launch authority failed. This is a cleanup fence only: it grants no
    /// second bind, allocation, or process contact. A signed-generation or Q
    /// occurrence rotation must not erase recovery evidence after allocation.
    pub(crate) fn retain_failed_qualification_scope_recovery(
        &self,
        placement: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, placement)?
            .context("failed qualification scope lost its allocation intent")?;
        ensure!(
            current.phase == QualificationLocalStartPhase::ScopeAllocating
                && current.scope_recovery.is_none()
                && current
                    .scope_allocation
                    .as_ref()
                    .is_some_and(|allocation| { recovery.matches_allocation(allocation) }),
            "failed qualification scope recovery changed or replayed its one-use allocation"
        );
        // `read_local_start` joins the historical allocation, accepted owner,
        // base and channel. Cleanup evidence deliberately does not require a
        // currently live occurrence, signed generation or channel deadline.
        let mut next = current.clone();
        next.scope_recovery = Some(recovery.clone());
        next.phase = QualificationLocalStartPhase::CleanupUnproved;
        next.cleanup_reason = Some("scope binding failed after kernel allocation".to_owned());
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
                    | QualificationLocalStartPhase::WorkspaceCreating
                    | QualificationLocalStartPhase::WorkspaceReady
                    | QualificationLocalStartPhase::ScopeAllocating
                    | QualificationLocalStartPhase::ScopeBound
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
    /// StateStore wrapper authenticates process-local original custody. An
    /// incomplete Create has no view receipt and cannot enter this route.
    pub(crate) fn begin_qualification_local_cleanup(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_local_start(&tx, &expected.placement_thread_id)?
            .context("qualification cleanup lost its retained start")?;
        ensure!(
            current == *expected,
            "qualification cleanup changed its original custody row"
        );
        let route = if current.phase == QualificationLocalStartPhase::Reserved
            || (current.phase == QualificationLocalStartPhase::CleanupUnproved
                && current.backend_id.is_none()
                && current.workspace_receipt.is_none())
        {
            ensure!(
                current.creator_identity.is_none()
                    && current.scope_allocation.is_none()
                    && current.view_borrow_intent.is_none(),
                "no-Create cleanup has possible backend or process contact"
            );
            require_local_processes_settled(&tx, &current.placement_thread_id)?;
            QualificationLocalCleanupRoute::NoCreate
        } else {
            ensure!(
                matches!(
                    current.phase,
                    QualificationLocalStartPhase::WorkspaceReady
                        | QualificationLocalStartPhase::ScopeAllocating
                        | QualificationLocalStartPhase::ScopeBound
                        | QualificationLocalStartPhase::WorkerAttached
                        | QualificationLocalStartPhase::CleanupUnproved
                ) && current.workspace_receipt.is_some()
                    && current.creator_identity.is_some()
                    && (current.scope_allocation.is_none() || current.scope_recovery.is_some()),
                "qualification cleanup has no completed original Create receipt"
            );
            QualificationLocalCleanupRoute::Created
        };
        let now = now_after(&current)?;
        let mut next = current.clone();
        next.phase = QualificationLocalStartPhase::CleanupIntent;
        next.cleanup_reason = None;
        next.cleanup = Some(QualificationLocalCleanupEvidence {
            route,
            prior_failure_reason: current.cleanup_reason.clone(),
            intent_at_ms: now,
            scope_retirement_intent_at_ms: None,
            scope_retired_at_ms: None,
            view_closed_at_ms: None,
            destroy_intent_at_ms: None,
            destroy_receipt: None,
            workspace_removed_at_ms: None,
            state_removed_at_ms: None,
            closed_at_ms: None,
        });
        next.updated_at_ms = now;
        transition(&tx, &current, &next)?;
        tx.commit()?;
        Ok(next)
    }

    /// Commit an exact scope retirement intent before Lillux touches the
    /// cgroup. No attached Worker or connector may remain live. A missing
    /// recovery after a possible allocation stays fenced indefinitely.
    pub(crate) fn reserve_qualification_local_scope_retirement(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        cleanup_cas(
            &self.conn,
            expected,
            QualificationLocalStartPhase::CleanupIntent,
            QualificationLocalStartPhase::ScopeRetirementPending,
            |tx, next, now| {
                ensure!(
                    next.cleanup.as_ref().is_some_and(
                        |cleanup| cleanup.route == QualificationLocalCleanupRoute::Created
                    ),
                    "qualification scope retirement requires a created workspace"
                );
                ensure!(
                    next.scope_allocation.is_none() || next.scope_recovery.is_some(),
                    "qualification scope allocation has no exact recovery"
                );
                require_local_processes_settled(tx, &next.placement_thread_id)?;
                next.cleanup.as_mut().unwrap().scope_retirement_intent_at_ms = Some(now);
                Ok(())
            },
        )
    }

    /// Called only after the retained recovery's idempotent, exact
    /// `retire_after_settlement` succeeds (or no scope was ever allocated).
    pub(crate) fn complete_qualification_local_scope_retirement(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        cleanup_cas(
            &self.conn,
            expected,
            QualificationLocalStartPhase::ScopeRetirementPending,
            QualificationLocalStartPhase::ScopeRetired,
            |tx, next, now| {
                ensure!(
                    next.scope_allocation.is_none() || next.scope_recovery.is_some(),
                    "qualification scope retirement lost its exact recovery"
                );
                require_local_processes_settled(tx, &next.placement_thread_id)?;
                next.cleanup.as_mut().unwrap().scope_retired_at_ms = Some(now);
                Ok(())
            },
        )
    }

    /// Physical close is supplied only by the original in-memory guard.
    pub(crate) fn record_qualification_local_view_closed(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        cleanup_cas(
            &self.conn,
            expected,
            QualificationLocalStartPhase::ScopeRetired,
            QualificationLocalStartPhase::ViewClosed,
            |tx, next, now| {
                require_local_processes_settled(tx, &next.placement_thread_id)?;
                next.cleanup.as_mut().unwrap().view_closed_at_ms = Some(now);
                Ok(())
            },
        )
    }

    pub(crate) fn reserve_qualification_workspace_destroy(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        cleanup_cas(
            &self.conn,
            expected,
            QualificationLocalStartPhase::ViewClosed,
            QualificationLocalStartPhase::DestroyPending,
            |_, next, now| {
                next.cleanup.as_mut().unwrap().destroy_intent_at_ms = Some(now);
                Ok(())
            },
        )
    }

    /// Retain the typed signed adapter's exact Destroy result; a caller flag
    /// or path absence cannot substitute for this response.
    pub(crate) fn record_qualification_workspace_destroyed(
        &self,
        expected: &QualificationLocalStartRecord,
        evidence: &ryeos_engine::isolation::WorkspaceLifecycleEvidence,
    ) -> Result<QualificationLocalStartRecord> {
        cleanup_cas(
            &self.conn,
            expected,
            QualificationLocalStartPhase::DestroyPending,
            QualificationLocalStartPhase::Destroyed,
            |_, next, now| {
                let original = next
                    .workspace_receipt
                    .as_ref()
                    .context("qualification Destroy lost original Create receipt")?;
                ensure!(
                    evidence.operation
                        == ryeos_isolation_protocol::WorkspaceLifecycleOperation::Destroy
                        && evidence.destroyed
                        && evidence.mutations.is_empty()
                        && evidence.workspace_id == next.workspace_id
                        && evidence.launch_owner == next.launch_owner
                        && evidence.backend_id == original.backend_id
                        && evidence.backend_version == original.backend_version
                        && evidence.pinned_root_identities == original.pinned_root_identities
                        && evidence.mount_identity.as_deref()
                            == Some(original.mount_identity.as_str()),
                    "qualification Destroy changed its signed original workspace identity"
                );
                next.cleanup.as_mut().unwrap().destroy_receipt =
                    Some(QualificationWorkspaceDestroyReceipt {
                        backend_id: evidence.backend_id.clone(),
                        backend_version: evidence.backend_version.clone(),
                        pinned_root_identities: evidence.pinned_root_identities.clone(),
                        mount_identity: original.mount_identity.clone(),
                        destroyed_at_ms: now,
                    });
                Ok(())
            },
        )
    }

    /// The StateStore wrapper removes the original pinned workspace root
    /// before calling this CAS. No-Create bypasses view/Destroy only because
    /// the Reserved journal preceded every possible backend Create.
    pub(crate) fn record_qualification_workspace_removed(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        let route = expected
            .cleanup
            .as_ref()
            .context("Q removal has no cleanup intent")?
            .route;
        let from = match route {
            QualificationLocalCleanupRoute::NoCreate => QualificationLocalStartPhase::CleanupIntent,
            QualificationLocalCleanupRoute::Created => QualificationLocalStartPhase::Destroyed,
        };
        cleanup_cas(
            &self.conn,
            expected,
            from,
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
                require_local_processes_settled(tx, &next.placement_thread_id)?;
                next.cleanup.as_mut().unwrap().closed_at_ms = Some(now);
                Ok(())
            },
        )
    }
}
