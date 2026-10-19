//! One local, pre-kernel start attempt for a bound Q allocation.
//!
//! This is a journal of intent and exact receipts. It cannot recreate the
//! original private workspace directory after a crash. A surviving row grants
//! cleanup/recovery only, never another directory borrow or Worker launch.

use super::qualification_controller_canary::QualificationControllerCanaryIntent;
use super::*;
use ryeos_state::external_execution::journal::load_binding;
use ryeos_state::external_execution::qualification_controller_canary::{
    QControllerCanaryBinding, QControllerCanaryEvidence,
};

// A present null is permitted; omitting the journal field is not.
fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize<'de>>::deserialize(deserializer)
}

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
    controller_canary_intent_json TEXT,
    controller_canary_binding_json TEXT,
    controller_canary_terminal_json TEXT,
    CHECK (updated_at_ms >= created_at_ms),
    CHECK (length(CAST(record_json AS BLOB)) BETWEEN 1 AND 131072)
);
CREATE TRIGGER external_qualification_local_start_insert_guard
BEFORE INSERT ON external_qualification_local_start
WHEN NEW.phase!='reserved'
 OR json_type(NEW.record_json,'$.credential_custody') IS NOT 'null'
 OR json_type(NEW.record_json,'$.controller_canary_intent') IS NOT 'null'
 OR json_type(NEW.record_json,'$.controller_canary_binding') IS NOT 'null'
 OR json_type(NEW.record_json,'$.controller_canary_terminal') IS NOT 'null'
 OR NEW.controller_canary_intent_json IS NOT NULL
 OR NEW.controller_canary_binding_json IS NOT NULL
 OR NEW.controller_canary_terminal_json IS NOT NULL
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
      '$.worker_incarnation_digest','$.cleanup_reason','$.cleanup','$.credential_custody','$.controller_canary_intent','$.controller_canary_binding','$.controller_canary_terminal','$.updated_at_ms')
    != json_remove(OLD.record_json,'$.phase','$.workspace_identity','$.directory_borrow_intent',
      '$.worker_incarnation_digest','$.cleanup_reason','$.cleanup','$.credential_custody','$.controller_canary_intent','$.controller_canary_binding','$.controller_canary_terminal','$.updated_at_ms')
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
      ((NEW.controller_canary_intent_json IS NOT OLD.controller_canary_intent_json
        OR NEW.controller_canary_binding_json IS NOT OLD.controller_canary_binding_json
        OR NEW.controller_canary_terminal_json IS NOT OLD.controller_canary_terminal_json)
       AND json_remove(NEW.record_json,'$.controller_canary_intent','$.controller_canary_binding','$.controller_canary_terminal','$.updated_at_ms')
           = json_remove(OLD.record_json,'$.controller_canary_intent','$.controller_canary_binding','$.controller_canary_terminal','$.updated_at_ms'))
      OR (json_extract(NEW.record_json,'$.credential_custody') IS NOT json_extract(OLD.record_json,'$.credential_custody')
       AND json_remove(NEW.record_json,'$.credential_custody','$.updated_at_ms')
           = json_remove(OLD.record_json,'$.credential_custody','$.updated_at_ms'))
      OR (OLD.phase='workspace_ready'
      AND json_extract(OLD.record_json,'$.directory_borrow_intent') IS NULL
      AND json_extract(NEW.record_json,'$.directory_borrow_intent') IS NOT NULL
      AND json_remove(NEW.record_json,'$.directory_borrow_intent','$.updated_at_ms')
          = json_remove(OLD.record_json,'$.directory_borrow_intent','$.updated_at_ms'))))
 OR NOT ((NEW.phase=OLD.phase AND (
      json_extract(NEW.record_json,'$.credential_custody') IS NOT json_extract(OLD.record_json,'$.credential_custody')
      OR NEW.controller_canary_intent_json IS NOT OLD.controller_canary_intent_json
      OR NEW.controller_canary_binding_json IS NOT OLD.controller_canary_binding_json
      OR NEW.controller_canary_terminal_json IS NOT OLD.controller_canary_terminal_json))
      OR (OLD.phase='reserved' AND NEW.phase='workspace_ready')
      OR (OLD.phase='workspace_ready' AND NEW.phase IN ('workspace_ready','worker_attached'))
      OR (OLD.phase IN ('reserved','workspace_ready','worker_attached') AND NEW.phase='cleanup_unproved')
      OR (OLD.phase IN ('reserved','workspace_ready','worker_attached','cleanup_unproved') AND NEW.phase='cleanup_intent')
      OR (OLD.phase='cleanup_intent' AND NEW.phase='workspace_removed')
      OR (OLD.phase='workspace_removed' AND NEW.phase='state_removed')
      OR (OLD.phase='state_removed' AND NEW.phase='closed'))
BEGIN SELECT RAISE(ABORT, 'qualification local start transition changed its one-use owner'); END;
CREATE TRIGGER external_qualification_local_canary_transition_guard
BEFORE UPDATE ON external_qualification_local_start
WHEN json_type(NEW.record_json,'$.controller_canary_intent') IS NULL
 OR json_type(NEW.record_json,'$.controller_canary_binding') IS NULL
 OR json_type(NEW.record_json,'$.controller_canary_terminal') IS NULL
 OR NEW.controller_canary_intent_json IS NOT json_extract(NEW.record_json,'$.controller_canary_intent')
 OR NEW.controller_canary_binding_json IS NOT json_extract(NEW.record_json,'$.controller_canary_binding')
 OR NEW.controller_canary_terminal_json IS NOT json_extract(NEW.record_json,'$.controller_canary_terminal')
 OR ((NEW.controller_canary_intent_json IS NOT OLD.controller_canary_intent_json
      OR NEW.controller_canary_binding_json IS NOT OLD.controller_canary_binding_json
      OR NEW.controller_canary_terminal_json IS NOT OLD.controller_canary_terminal_json)
  AND NOT COALESCE((NEW.phase=OLD.phase AND (
   (OLD.phase IN ('reserved','workspace_ready')
    AND json_type(OLD.record_json,'$.directory_borrow_intent')='null'
    AND json_type(OLD.record_json,'$.worker_incarnation_digest')='null'
    AND json_type(OLD.record_json,'$.credential_custody')='object'
    AND json_type(OLD.record_json,'$.credential_custody.released_at_ms')='null'
    AND OLD.controller_canary_intent_json IS NULL
    AND json_type(NEW.record_json,'$.controller_canary_intent')='object'
    AND OLD.controller_canary_binding_json IS NULL AND NEW.controller_canary_binding_json IS NULL
    AND OLD.controller_canary_terminal_json IS NULL AND NEW.controller_canary_terminal_json IS NULL)
   OR (OLD.phase IN ('reserved','workspace_ready')
    AND json_type(OLD.record_json,'$.directory_borrow_intent')='null'
    AND json_type(OLD.record_json,'$.worker_incarnation_digest')='null'
    AND json_type(OLD.record_json,'$.credential_custody')='object'
    AND json_type(OLD.record_json,'$.credential_custody.released_at_ms')='null'
    AND OLD.controller_canary_intent_json IS NOT NULL
    AND NEW.controller_canary_intent_json=OLD.controller_canary_intent_json
    AND OLD.controller_canary_binding_json IS NULL
    AND json_type(NEW.record_json,'$.controller_canary_binding')='object'
    AND OLD.controller_canary_terminal_json IS NULL AND NEW.controller_canary_terminal_json IS NULL)
   OR (OLD.phase IN ('worker_attached','cleanup_unproved')
    AND OLD.controller_canary_binding_json IS NOT NULL
    AND NEW.controller_canary_intent_json=OLD.controller_canary_intent_json
    AND NEW.controller_canary_binding_json=OLD.controller_canary_binding_json
    AND OLD.controller_canary_terminal_json IS NULL
    AND json_type(NEW.record_json,'$.controller_canary_terminal')='object'
    AND json_type(OLD.record_json,'$.credential_custody.released_at_ms')='null'
    AND json_type(OLD.record_json,'$.worker_incarnation_digest')='text'
    AND EXISTS(SELECT 1 FROM external_qualification_worker_process p
       WHERE p.placement_thread_id=OLD.placement_thread_id AND p.phase='terminal')
    AND NOT EXISTS(SELECT 1 FROM external_execution_connector c
       WHERE c.placement_thread_id=OLD.placement_thread_id AND c.state!='closed'))
  )),0))
BEGIN SELECT RAISE(ABORT, 'qualification canary changed its original custody stage'); END;
CREATE TRIGGER external_qualification_local_credential_transition_guard
BEFORE UPDATE ON external_qualification_local_start
WHEN json_type(NEW.record_json,'$.credential_custody') IS NULL
 OR (json_extract(NEW.record_json,'$.credential_custody') IS NOT json_extract(OLD.record_json,'$.credential_custody')
 AND NOT COALESCE((
   (OLD.phase='reserved' AND NEW.phase='reserved'
    AND json_type(OLD.record_json,'$.credential_custody')='null'
    AND json_type(NEW.record_json,'$.credential_custody')='object'
    AND json_type(NEW.record_json,'$.credential_custody.released_at_ms')='null'
    AND json_extract(NEW.record_json,'$.credential_custody.lock_owner')=json_extract(NEW.record_json,'$.launch_owner')
    AND EXISTS(SELECT 1 FROM credential_profile p
      JOIN external_execution_allocation a ON a.placement_thread_id=NEW.placement_thread_id
      WHERE json_extract(a.reservation_json,'$.owner.operator_principal')=json_extract(NEW.record_json,'$.credential_custody.owner_principal')
        AND p.profile_id=json_extract(NEW.record_json,'$.credential_custody.profile_id')
        AND p.owner_principal=json_extract(NEW.record_json,'$.credential_custody.owner_principal')
        AND p.home_id=json_extract(NEW.record_json,'$.credential_custody.home_id')
        AND p.credential_generation=json_extract(NEW.record_json,'$.credential_custody.credential_generation')
        AND p.authority_revision=json_extract(NEW.record_json,'$.credential_custody.authority_revision')
        AND p.lock_owner=json_extract(NEW.record_json,'$.credential_custody.lock_owner') AND p.state='active'))
   OR (NEW.phase=OLD.phase
    AND json_type(OLD.record_json,'$.credential_custody')='object'
    AND json_type(OLD.record_json,'$.credential_custody.released_at_ms')='null'
    AND json_type(NEW.record_json,'$.credential_custody.released_at_ms')='integer'
    AND json_extract(NEW.record_json,'$.credential_custody.released_at_ms')>=json_extract(OLD.record_json,'$.credential_custody.acquired_at_ms')
    AND json_remove(json_extract(NEW.record_json,'$.credential_custody'),'$.released_at_ms')
        =json_remove(json_extract(OLD.record_json,'$.credential_custody'),'$.released_at_ms')
    AND NOT EXISTS(SELECT 1 FROM external_execution_connector c
      WHERE c.placement_thread_id=OLD.placement_thread_id AND c.state!='closed')
    AND ((json_extract(OLD.record_json,'$.directory_borrow_intent') IS NULL
          AND json_extract(OLD.record_json,'$.worker_incarnation_digest') IS NULL
          AND NOT EXISTS(SELECT 1 FROM external_qualification_worker_process p WHERE p.placement_thread_id=OLD.placement_thread_id))
      OR (json_extract(OLD.record_json,'$.worker_incarnation_digest') IS NOT NULL
          AND EXISTS(SELECT 1 FROM external_qualification_worker_process p
            WHERE p.placement_thread_id=OLD.placement_thread_id AND p.phase='terminal'))))
 ),0))
 OR (NEW.phase='closed' AND json_type(NEW.record_json,'$.credential_custody')='object'
     AND json_type(NEW.record_json,'$.credential_custody.released_at_ms') IS NOT 'integer')
BEGIN SELECT RAISE(ABORT, 'qualification credential custody changed its one-use owner'); END;
CREATE TRIGGER credential_profile_qualification_custody_guard
BEFORE UPDATE ON credential_profile
WHEN EXISTS(SELECT 1 FROM external_qualification_local_start q
 WHERE json_extract(q.record_json,'$.credential_custody.profile_id')=OLD.profile_id
   AND json_type(q.record_json,'$.credential_custody')='object'
   AND json_type(q.record_json,'$.credential_custody.released_at_ms')='null')
 AND (NEW.profile_id IS NOT OLD.profile_id OR NEW.owner_principal IS NOT OLD.owner_principal
      OR NEW.home_id IS NOT OLD.home_id OR NEW.lock_owner IS NOT OLD.lock_owner
      OR NEW.state IN ('revoked','deleting','deleted'))
BEGIN SELECT RAISE(ABORT, 'credential profile retains unresolved qualification custody'); END;
CREATE TRIGGER credential_profile_qualification_custody_delete_guard
BEFORE DELETE ON credential_profile
WHEN EXISTS(SELECT 1 FROM external_qualification_local_start q
 WHERE json_extract(q.record_json,'$.credential_custody.profile_id')=OLD.profile_id
   AND json_type(q.record_json,'$.credential_custody')='object'
   AND json_type(q.record_json,'$.credential_custody.released_at_ms')='null')
BEGIN SELECT RAISE(ABORT, 'credential profile retains unresolved qualification custody'); END;
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

/// Exact node-private credential ownership for this original Q launch. Signed
/// contract labels describe admitted use, not metadata inferred from auth bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationLocalCredentialCustody {
    pub profile_id: String,
    pub owner_principal: String,
    pub credential_generation: u64,
    pub authority_revision: u64,
    pub home_id: String,
    pub lock_owner: String,
    pub workload_family: String,
    pub subject_projection_contract: String,
    pub contract_digest: String,
    pub acquired_at_ms: i64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub released_at_ms: Option<i64>,
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub credential_custody: Option<QualificationLocalCredentialCustody>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub controller_canary_intent: Option<QualificationControllerCanaryIntent>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub controller_canary_binding: Option<QControllerCanaryBinding>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub controller_canary_terminal: Option<QControllerCanaryEvidence>,
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
        super::qualification_controller_canary::validate_canary_shape(self)?;
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
        if let Some(custody) = &self.credential_custody {
            for (label, value) in [
                ("credential profile", &custody.profile_id),
                ("credential principal", &custody.owner_principal),
                ("credential home", &custody.home_id),
                ("credential workload family", &custody.workload_family),
                (
                    "credential subject contract",
                    &custody.subject_projection_contract,
                ),
            ] {
                validate_bounded_runtime_text(label, value, 256)?;
            }
            crate::private_artifact_home::home_path(std::path::Path::new("/"), &custody.home_id)?;
            validate_sha256("credential contract", &custody.contract_digest)?;
            for label in [
                &custody.workload_family,
                &custody.subject_projection_contract,
            ] {
                ensure!(
                    label.bytes().all(|byte| byte.is_ascii_alphanumeric()
                        || matches!(byte, b'-' | b'_' | b'.' | b'/')),
                    "qualification credential contract label is not a bounded identifier"
                );
            }
            ensure!(
                custody.credential_generation > 0
                    && custody.authority_revision > 0
                    && custody.lock_owner == self.launch_owner
                    && custody.acquired_at_ms >= self.created_at_ms
                    && custody.acquired_at_ms <= self.updated_at_ms
                    && custody
                        .released_at_ms
                        .is_none_or(|at| at >= custody.acquired_at_ms && at <= self.updated_at_ms)
                    && (self.phase != QualificationLocalStartPhase::Closed
                        || custody.released_at_ms.is_some()),
                "qualification credential custody has invalid owner or timing"
            );
        }
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

pub(super) fn canonical(record: &QualificationLocalStartRecord) -> Result<String> {
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
    require_historical_credential_custody(conn, &record)?;
    super::qualification_controller_canary::require_retained_canary(conn, &record)?;
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
            && allocation.reservation.base_snapshot_hash == record.base_snapshot_hash
            && record
                .credential_custody
                .as_ref()
                .is_none_or(|custody| custody.owner_principal == owner.operator_principal),
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

fn require_historical_credential_custody(
    conn: &Connection,
    record: &QualificationLocalStartRecord,
) -> Result<()> {
    let Some(custody) = &record.credential_custody else {
        return Ok(());
    };
    let profile: Option<(String,String,i64,i64,Option<String>)> = conn.query_row(
        "SELECT owner_principal,home_id,credential_generation,authority_revision,lock_owner FROM credential_profile WHERE profile_id=?1",
        [&custody.profile_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).optional()?;
    if custody.released_at_ms.is_some() {
        ensure!(
            profile
                .as_ref()
                .is_none_or(|profile| profile.4.as_deref() != Some(custody.lock_owner.as_str())),
            "released qualification credential still owns its old profile lock"
        );
        return Ok(());
    }
    let (owner, home, generation, revision, lock_owner) =
        profile.context("qualification credential profile disappeared")?;
    ensure!(
        owner == custody.owner_principal
            && home == custody.home_id
            && u64::try_from(generation)? >= custody.credential_generation
            && u64::try_from(revision)? >= custody.authority_revision
            && lock_owner.as_deref() == Some(custody.lock_owner.as_str()),
        "qualification credential lost its historical profile fence"
    );
    Ok(())
}

pub(super) fn require_live_credential_custody(
    conn: &Connection,
    record: &QualificationLocalStartRecord,
) -> Result<()> {
    let custody = record
        .credential_custody
        .as_ref()
        .context("qualification has no credential custody")?;
    ensure!(
        custody.released_at_ms.is_none(),
        "qualification credential custody was released"
    );
    let exact: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM credential_profile WHERE profile_id=?1 AND owner_principal=?2
         AND home_id=?3 AND credential_generation=?4 AND authority_revision=?5 AND lock_owner=?6 AND state='active')",
        params![custody.profile_id,custody.owner_principal,custody.home_id,i64::try_from(custody.credential_generation)?,i64::try_from(custody.authority_revision)?,custody.lock_owner],
        |row| row.get(0),
    )?;
    ensure!(
        exact,
        "qualification credential profile is no longer exactly active and owned"
    );
    Ok(())
}

pub(super) fn require_current_start(
    conn: &Connection,
    record: &QualificationLocalStartRecord,
) -> Result<()> {
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

/// Recheck original credential custody inside the caller's existing transaction.
/// Held-process attachment must join this check atomically without opening a
/// second transaction on the same runtime connection.
pub(super) fn require_live_credential_start_tx(
    conn: &Connection,
    placement: &str,
) -> Result<QualificationLocalStartRecord> {
    let record =
        read_local_start(conn, placement)?.context("Q credential check has no original start")?;
    require_current_start(conn, &record)?;
    require_live_credential_custody(conn, &record)?;
    Ok(record)
}

pub(super) fn transition(
    conn: &Connection,
    current: &QualificationLocalStartRecord,
    next: &QualificationLocalStartRecord,
) -> Result<()> {
    let prior = canonical(current)?;
    let next_json = canonical(next)?;
    ensure!(
        conn.execute(
            "UPDATE external_qualification_local_start SET phase=?3,record_json=?4,updated_at_ms=?5,
                 controller_canary_intent_json=?7,controller_canary_binding_json=?8,controller_canary_terminal_json=?9
             WHERE placement_thread_id=?1 AND record_json=?2 AND phase=?6",
            params![
                current.placement_thread_id,
                prior,
                next.phase.as_str(),
                next_json,
                next.updated_at_ms,
                current.phase.as_str(),
                super::qualification_controller_canary::optional_json(next.controller_canary_intent.as_ref())?,
                super::qualification_controller_canary::optional_json(next.controller_canary_binding.as_ref())?,
                super::qualification_controller_canary::optional_json(next.controller_canary_terminal.as_ref())?,
            ],
        )? == 1,
        "qualification local start lost its exact one-use transition"
    );
    Ok(())
}

pub(super) fn now_after(current: &QualificationLocalStartRecord) -> Result<i64> {
    Ok(i64::try_from(lillux::time::timestamp_millis())?.max(current.updated_at_ms))
}

pub(super) fn require_local_processes_settled(
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
        self.conn.with_sql(|conn| read_local_start(conn, placement))
    }

    /// Claim the original launch's credential generation exactly once. The
    /// family/projection labels are signed intent supplied by admission; the
    /// generic profile ledger does not independently observe those labels.
    pub(crate) fn claim_qualification_local_credential_custody(
        &self,
        placement: &str,
        profile_id: &str,
        owner_principal: &str,
        contract_digest: &str,
        workload_family: &str,
        subject_projection_contract: &str,
        proof: &crate::state_store::VerifiedExternalQualificationOwner,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let current = read_local_start(&tx, placement)?
                .context("Q credential claim has no original start")?;
            ensure!(
                current.phase == QualificationLocalStartPhase::Reserved
                    && current.credential_custody.is_none(),
                "Q credential claim cannot replay or replace original custody"
            );
            require_current_start(&tx, &current)?;
            let allocation = read(&tx, placement)?.context("Q credential claim lost allocation")?;
            require_new_owner(&tx, &allocation.reservation, None, Some(proof))?;
            ensure!(
                matches!(&allocation.reservation.owner,
                ExternalAllocationOwner::QualificationCandidate(owner) if owner.operator_principal == owner_principal),
                "Q credential principal differs from its authenticated allocation owner"
            );
            let profile = self
                .credential_profile(profile_id)?
                .context("Q credential profile is absent")?;
            ensure!(
                profile.owner_principal == owner_principal
                    && profile.state == "active"
                    && profile.lock_owner.is_none(),
                "Q credential profile is not active, owned and available"
            );
            let now = now_after(&current)?;
            let mut next = current.clone();
            next.credential_custody = Some(QualificationLocalCredentialCustody {
                profile_id: profile_id.to_owned(),
                owner_principal: owner_principal.to_owned(),
                credential_generation: profile.credential_generation,
                authority_revision: profile.authority_revision,
                home_id: profile.home_id,
                lock_owner: current.launch_owner.clone(),
                workload_family: workload_family.to_owned(),
                subject_projection_contract: subject_projection_contract.to_owned(),
                contract_digest: contract_digest.to_owned(),
                acquired_at_ms: now,
                released_at_ms: None,
            });
            next.updated_at_ms = now;
            next.validate()?;
            ensure!(
                tx.execute(
                    "UPDATE credential_profile SET lock_owner=?3,updated_at_ms=MAX(updated_at_ms,?4)
             WHERE profile_id=?1 AND owner_principal=?2 AND lock_owner IS NULL AND state='active'
               AND credential_generation=?5 AND authority_revision=?6",
                    params![
                        profile_id,
                        owner_principal,
                        current.launch_owner,
                        now,
                        i64::try_from(profile.credential_generation)?,
                        i64::try_from(profile.authority_revision)?
                    ],
                )? == 1,
                "Q credential claim lost its exact active profile CAS"
            );
            transition(&tx, &current, &next)?;
            require_historical_credential_custody(&tx, &next)?;
            tx.commit()?;
            Ok(next)
        })
    }

    /// Fresh contact must still own the original active profile generation.
    pub(crate) fn require_live_qualification_local_credential_custody(
        &self,
        placement: &str,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = require_live_credential_start_tx(&tx, placement)?;
            tx.commit()?;
            Ok(record)
        })
    }

    /// Cleanup may release revoked/expired custody, but only after exact local
    /// retirement or proof that no directory borrow could have spawned a child.
    /// The old lock and its release marker change in the same transaction.
    pub(crate) fn release_qualification_local_credential_custody(
        &self,
        placement: &str,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let current = read_local_start(&tx, placement)?
                .context("Q credential release has no original start")?;
            let Some(custody) = &current.credential_custody else {
                tx.commit()?;
                return Ok(current);
            };
            if custody.released_at_ms.is_some() {
                tx.commit()?;
                return Ok(current);
            }
            require_local_processes_settled(&tx, &current)?;
            let now = now_after(&current)?;
            let mut next = current.clone();
            next.credential_custody
                .as_mut()
                .expect("custody checked")
                .released_at_ms = Some(now);
            next.updated_at_ms = now;
            transition(&tx, &current, &next)?;
            ensure!(
                tx.execute(
                    "UPDATE credential_profile SET lock_owner=NULL,updated_at_ms=MAX(updated_at_ms,?3)
             WHERE profile_id=?1 AND lock_owner=?2 AND owner_principal=?4 AND home_id=?5",
                    params![
                        custody.profile_id,
                        custody.lock_owner,
                        now,
                        custody.owner_principal,
                        custody.home_id
                    ],
                )? == 1,
                "Q credential release lost its exact old profile lock"
            );
            require_historical_credential_custody(&tx, &next)?;
            tx.commit()?;
            Ok(next)
        })
    }

    /// Read-only final pre-spawn gate. The app caller holds the Q occurrence
    /// contact gate from this check through held attachment; this projection
    /// itself grants neither a descriptor nor process contact.
    pub(crate) fn authorize_qualification_local_spawn(
        &self,
        placement: &str,
        proof: &crate::state_store::VerifiedExternalQualificationOwner,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let allocation =
                read(&tx, placement)?.context("qualification local spawn lost allocation")?;
            require_new_owner(&tx, &allocation.reservation, None, Some(proof))?;
            let record = read_local_start(&tx, placement)?
                .context("qualification local spawn has no original start")?;
            ensure!(
                record.phase == QualificationLocalStartPhase::WorkspaceReady
                    && record.directory_borrow_intent.is_some(),
                "qualification local spawn requires its one-use original directory borrow"
            );
            require_current_start(&tx, &record)?;
            require_live_credential_custody(&tx, &record)?;
            super::qualification_controller_canary::require_bound_canary(&record, proof, None)?;
            tx.commit()?;
            Ok(record)
        })
    }

    /// Rejoin the original attached process and current released Q channel
    /// before the held process may execute. This check performs no process I/O.
    pub(crate) fn authorize_qualification_worker_release(
        &self,
        placement: &str,
        proof: &crate::state_store::VerifiedExternalQualificationOwner,
    ) -> Result<()> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let allocation =
                read(&tx, placement)?.context("qualification Worker release lost allocation")?;
            require_new_owner(&tx, &allocation.reservation, None, Some(proof))?;
            let local = read_local_start(&tx, placement)?
                .context("qualification Worker release lost its original local start")?;
            ensure!(
                local.phase == QualificationLocalStartPhase::WorkerAttached,
                "qualification Worker release requires its original attached process"
            );
            require_current_start(&tx, &local)?;
            require_live_credential_custody(&tx, &local)?;
            super::qualification_controller_canary::require_bound_canary(&local, proof, None)?;
            let process = qualification_process::require_current_attached_worker(&tx, placement)?;
            ensure!(
                local.worker_incarnation_digest.as_deref()
                    == Some(
                        process
                            .process_identity
                            .owner_incarnation_digest()?
                            .as_str()
                    )
                    && process.channel_binding_digest == local.channel_binding_digest
                    && process.daemon_generation_id == local.daemon_generation_id,
                "qualification Worker release changed its exact attached incarnation"
            );
            tx.commit()?;
            Ok(())
        })
    }

    /// Commit one possible descriptor borrow before cloning the original
    /// workspace directory. A failed clone consumes this attempt and requires
    /// cleanup; it must never be retried with a replacement directory or process.
    pub(crate) fn reserve_qualification_workspace_directory_borrow(
        &self,
        placement: &str,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
    }

    /// Reserve before creating a directory. Exact replay is refused even when
    /// the first attempt may have stopped before its first filesystem effect.
    pub(crate) fn reserve_qualification_local_start(
        &self,
        placement: &str,
        runtime_cache_root: &std::path::Path,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
                credential_custody: None,
                controller_canary_intent: None,
                controller_canary_binding: None,
                controller_canary_terminal: None,
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
                "INSERT INTO external_qualification_local_start (placement_thread_id,accepted_root_id,accepted_capsule_hash,qualification_operation_id,qualification_occurrence_id,channel_binding_digest,phase,record_json,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,?4,?5,?6,'reserved',?7,?8,?8)",
                params![record.placement_thread_id, record.accepted_root_id, record.accepted_capsule_hash,
                    record.qualification_operation_id, record.qualification_occurrence_id,
                    record.channel_binding_digest, json, now],
            )?;
            tx.commit()?;
            Ok(record)
        })
    }

    /// Retain the original populated project child identity before any Worker
    /// can borrow it. A surviving reservation never grants replacement creation.
    pub(crate) fn complete_qualification_private_workspace(
        &self,
        placement: &str,
        identity: lillux::PinnedDirectoryIdentity,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
    }

    /// Fence every unresolved side effect. This transition proves no cleanup.
    pub(crate) fn mark_qualification_local_cleanup_unproved(
        &self,
        placement: &str,
        reason: &str,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            validate_bounded_runtime_text("Q local cleanup reason", reason, 1024)?;
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
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
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
                        &format!(
                            "SELECT EXISTS(SELECT 1 FROM {table} WHERE placement_thread_id=?1)"
                        ),
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
        })
    }

    /// Recheck exact retained custody and settled local processes immediately
    /// before the wrapper removes its original pinned workspace directory.
    pub(crate) fn authorize_qualification_workspace_removal(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<()> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let current = read_local_start(&tx, &expected.placement_thread_id)?
                .context("qualification workspace removal lost its original row")?;
            ensure!(
                current == *expected
                    && current.phase == QualificationLocalStartPhase::CleanupIntent,
                "qualification workspace removal requires its exact cleanup intent"
            );
            require_local_processes_settled(&tx, &current)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// The StateStore wrapper removes the original pinned workspace directory
    /// before recording this exact receipt. CleanupIntent already fences spawn.
    pub(crate) fn record_qualification_workspace_removed(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            cleanup_cas(
                conn,
                expected,
                QualificationLocalStartPhase::CleanupIntent,
                QualificationLocalStartPhase::WorkspaceRemoved,
                |_, next, now| {
                    next.cleanup.as_mut().unwrap().workspace_removed_at_ms = Some(now);
                    Ok(())
                },
            )
        })
    }

    pub(crate) fn record_qualification_state_root_removed(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            cleanup_cas(
                conn,
                expected,
                QualificationLocalStartPhase::WorkspaceRemoved,
                QualificationLocalStartPhase::StateRemoved,
                |_, next, now| {
                    next.cleanup.as_mut().unwrap().state_removed_at_ms = Some(now);
                    Ok(())
                },
            )
        })
    }

    pub(crate) fn close_qualification_local_start(
        &self,
        expected: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            cleanup_cas(
                conn,
                expected,
                QualificationLocalStartPhase::StateRemoved,
                QualificationLocalStartPhase::Closed,
                |tx, next, now| {
                    require_local_processes_settled(tx, next)?;
                    next.cleanup.as_mut().unwrap().closed_at_ms = Some(now);
                    Ok(())
                },
            )
        })
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
            credential_custody: None,
            controller_canary_intent: None,
            controller_canary_binding: None,
            controller_canary_terminal: None,
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
        let error = crate::runtime_db::test_native::with_transient(|conn| {
            Ok(require_local_processes_settled(conn, &record).unwrap_err())
        });
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

    fn fixture_journal(conn: &Connection) -> QualificationLocalStartRecord {
        conn.execute_batch(
            "CREATE TABLE credential_profile(profile_id TEXT PRIMARY KEY,owner_principal TEXT,home_id TEXT,credential_generation INTEGER,authority_revision INTEGER,lock_owner TEXT,state TEXT);
             CREATE TABLE external_qualification_worker_process(placement_thread_id TEXT PRIMARY KEY,phase TEXT,process_identity_json TEXT);
             CREATE TABLE external_execution_connector(placement_thread_id TEXT PRIMARY KEY,state TEXT);
             CREATE TABLE external_execution_allocation(placement_thread_id TEXT PRIMARY KEY, phase TEXT, reservation_json TEXT);
             CREATE TABLE external_execution_channel(placement_thread_id TEXT PRIMARY KEY, binding_digest TEXT, state TEXT);",
        ).unwrap();
        conn.execute_batch(JOURNAL_SQL).unwrap();
        let current = fixture_record();
        let reservation = serde_json::json!({"owner": {
            "kind": "qualification_candidate",
            "operator_principal": "principal",
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
        conn.execute("INSERT INTO external_qualification_local_start (placement_thread_id,accepted_root_id,accepted_capsule_hash,qualification_operation_id,qualification_occurrence_id,channel_binding_digest,phase,record_json,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,'reserved',?7,1,1)",
            params![current.placement_thread_id, current.accepted_root_id, current.accepted_capsule_hash,
                current.qualification_operation_id, current.qualification_occurrence_id,
                current.channel_binding_digest, canonical(&current).unwrap()]).unwrap();
        current
    }

    // This fixture serves the real retained readers, unlike fixture_journal's
    // deliberately partial rows for isolated SQL trigger tests. It supplies no
    // admission token, provider contact, process, or successful held attachment.
    fn fixture_retained_credential_journal(conn: &Connection) -> QualificationLocalStartRecord {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use ryeos_external_execution_contract::{
            restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
            runtime_snapshot::RuntimeSnapshotQualificationOccurrence,
        };
        use ryeos_state::{
            external_content::products::qualification::ProductQualificationConsumerRuntimeMemberIdentity,
            external_execution::{
                EXECUTION_CHANNEL_BINDING_SCHEMA, ExecutionChannelBinding,
                admission::test_support::{fixture_qualification_use, fixture_requirement},
                qualification_session::{
                    AdmittedExternalQualificationSessionProgram,
                    QUALIFICATION_SESSION_PROGRAM_SCHEMA, qualification_candidate_placement_key,
                },
            },
            objects::{
                EXTERNAL_CONTENT_MANIFEST_KIND, ExternalContentKind, canonical_value_digest,
            },
        };
        let requirement = fixture_requirement();
        let program = AdmittedExternalQualificationSessionProgram {
            schema: QUALIFICATION_SESSION_PROGRAM_SCHEMA,
            qualification_purpose_digest: "7".repeat(64),
            consumer_content_digest: "8".repeat(64),
            qualification_use: fixture_qualification_use(&requirement).unwrap(),
            runtime_manifest_kind: EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_content_kind: ExternalContentKind::Tree,
            runtime_manifest_hash: "e".repeat(64),
            runtime_member: ProductQualificationConsumerRuntimeMemberIdentity {
                product_declaration_id: requirement.runtime_product_declaration_id.clone(),
                relative_path: requirement.runtime_recipe.executable_relative_path.clone(),
                executable_sha256: "6".repeat(64),
            },
            runtime_recipe_digest: requirement.runtime_recipe.digest().unwrap(),
            requirement,
            fixture: None,
        };
        program.validate().unwrap();
        let coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-qualification-root".into(),
            accepted_capsule_hash: "2".repeat(64),
            qualification_purpose_digest: program.qualification_purpose_digest.clone(),
            scenario_id: "hosted_worker".into(),
            scenario_source_digest: "a".repeat(64),
            subject_digest: "b".repeat(64),
            use_digest: canonical_value_digest(
                &serde_json::to_value(&program.qualification_use).unwrap(),
            )
            .unwrap(),
            prerequisite_measurement_attempt_id: "c".repeat(64),
            prerequisite_measurement_observation_digest: "d".repeat(64),
        };
        let operation = "3".repeat(64);
        let placement = qualification_candidate_placement_key(&coordinate, &operation).unwrap();
        let owner_key = lillux::crypto::generate_signing_key();
        let supervisor_key = lillux::crypto::generate_signing_key();
        let binding = ExecutionChannelBinding {
            schema: EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode: ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {},
            placement_thread_id: placement.clone(),
            allocation_request_digest: "a".repeat(64),
            occurrence_id: "qualification-occurrence".into(),
            admitted_capsule_hash: coordinate.accepted_capsule_hash.clone(),
            base_snapshot_hash: "5".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: program.runtime_manifest_hash.clone(),
            candidate_program_digest: program.digest().unwrap(),
            channel_nonce: "f".repeat(64),
            owner_public_key: STANDARD.encode(owner_key.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor_key.verifying_key().as_bytes()),
            issued_at_ms: 1,
            execution_deadline_ms: 60_001,
            expires_at_ms: 120_001,
            candidate_export_max_bytes: 0,
            max_frames: 100,
            max_bytes: 1024 * 1024,
        };
        binding.validate().unwrap();
        let reservation = ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: placement.clone(),
            admitted_capsule_hash: coordinate.accepted_capsule_hash.clone(),
            owner: ExternalAllocationOwner::QualificationCandidate(
                ExternalQualificationCandidateOwner {
                    coordinate: coordinate.clone(),
                    qualification_operation_id: operation.clone(),
                    qualification_occurrence: RuntimeSnapshotQualificationOccurrence {
                        schema: 1,
                        operation_id: operation.clone(),
                        occurrence_id: binding.occurrence_id.clone(),
                        provider_response_sha256: "9".repeat(64),
                        contact_deadline_exceeded: false,
                    },
                    operator_principal: "principal".into(),
                    program,
                },
            ),
            base_snapshot_hash: binding.base_snapshot_hash.clone(),
            binding_hash: binding.execution_binding_hash.clone(),
            capacity_owner: "b".repeat(64),
            channel_authority_generation: "c".repeat(64),
            channel_owner_public_key: binding.owner_public_key.clone(),
            channel_bootstrap_capability_hash: "e".repeat(64),
            admitted_adapter_request_base64: None,
            request_digest: binding.allocation_request_digest.clone(),
            max_active: 1,
            timeout_seconds: 60,
            contact_deadline_ms: 60_001,
            startup_started_at_ms: 1,
            startup_deadline_ms: 60_001,
        };
        reservation.validate().unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: binding.occurrence_id.clone(),
            provider_observation_digest: "9".repeat(64),
        };
        occurrence.validate(&reservation).unwrap();
        let mut current = fixture_record();
        current.placement_thread_id = placement;
        current.accepted_root_id = coordinate.accepted_root_id.clone();
        current.accepted_capsule_hash = coordinate.accepted_capsule_hash.clone();
        current.qualification_operation_id = operation.clone();
        current.qualification_occurrence_id = occurrence.occurrence_id.clone();
        current.base_snapshot_hash = reservation.base_snapshot_hash.clone();
        current.channel_binding_digest = binding.digest().unwrap();
        let digest = local_workspace_digest(
            &current.placement_thread_id,
            &coordinate.accepted_root_id,
            &coordinate.accepted_capsule_hash,
            &operation,
            &occurrence.occurrence_id,
            &current.channel_binding_digest,
            &current.daemon_generation_id,
        )
        .unwrap();
        current.workspace_id = format!("QW-{digest}");
        current.launch_owner = format!("Q-local-{digest}");
        current.workspace_root = format!(
            "/tmp/qualification-cache/executions/{}",
            current.workspace_id
        );
        current.state_root = format!(
            "/tmp/qualification-cache/qualification-local/{}",
            current.workspace_id
        );
        current.validate().unwrap();
        conn.execute_batch(
            "CREATE TABLE credential_profile(profile_id TEXT PRIMARY KEY,owner_principal TEXT,home_id TEXT,credential_generation INTEGER,authority_revision INTEGER,lock_owner TEXT,state TEXT);
             CREATE TABLE external_qualification_worker_process(placement_thread_id TEXT PRIMARY KEY,phase TEXT,process_identity_json TEXT);
             CREATE TABLE external_execution_connector(placement_thread_id TEXT PRIMARY KEY,state TEXT);
             CREATE TABLE external_execution_allocation(placement_thread_id TEXT PRIMARY KEY,capacity_owner TEXT,reservation_json TEXT,phase TEXT,occurrence_json TEXT);
             CREATE TABLE external_execution_channel(placement_thread_id TEXT PRIMARY KEY,binding_digest TEXT,binding_json TEXT,state TEXT);",
        ).unwrap();
        conn.execute_batch(JOURNAL_SQL).unwrap();
        let json = |value: serde_json::Value| lillux::canonical_json(&value).unwrap();
        conn.execute(
            "INSERT INTO external_execution_allocation VALUES(?1,?2,?3,'bound',?4)",
            params![
                current.placement_thread_id,
                reservation.capacity_owner,
                json(serde_json::to_value(&reservation).unwrap()),
                json(serde_json::to_value(&occurrence).unwrap())
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO external_execution_channel VALUES(?1,?2,?3,'running')",
            params![
                current.placement_thread_id,
                current.channel_binding_digest,
                json(serde_json::to_value(&binding).unwrap())
            ],
        )
        .unwrap();
        conn.execute("INSERT INTO external_qualification_local_start (placement_thread_id,accepted_root_id,accepted_capsule_hash,qualification_operation_id,qualification_occurrence_id,channel_binding_digest,phase,record_json,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,'reserved',?7,1,1)",
            params![current.placement_thread_id, current.accepted_root_id, current.accepted_capsule_hash,
                current.qualification_operation_id, current.qualification_occurrence_id,
                current.channel_binding_digest, canonical(&current).unwrap()]).unwrap();
        assert_eq!(
            read_local_start(conn, &current.placement_thread_id).unwrap(),
            Some(current.clone())
        );
        current
    }

    fn claim_fixture_credential(
        conn: &Connection,
        current: &QualificationLocalStartRecord,
    ) -> QualificationLocalStartRecord {
        conn.execute("INSERT INTO credential_profile VALUES('profile','principal','private-home',1,1,?1,'active')",
            [&current.launch_owner]).unwrap();
        let mut claimed = current.clone();
        claimed.credential_custody = Some(QualificationLocalCredentialCustody {
            profile_id: "profile".into(),
            owner_principal: "principal".into(),
            home_id: "private-home".into(),
            credential_generation: 1,
            authority_revision: 1,
            lock_owner: current.launch_owner.clone(),
            workload_family: "codex".into(),
            subject_projection_contract: "codex.account.v1".into(),
            contract_digest: "a".repeat(64),
            acquired_at_ms: 1,
            released_at_ms: None,
        });
        transition(conn, current, &claimed).unwrap();
        claimed
    }

    #[test]
    fn credential_custody_required_null_and_immutable_claim_are_cold_checked() {
        let (current, claimed) = crate::runtime_db::test_native::with_transient(|conn| {
            let current = fixture_journal(conn);
            let claimed = claim_fixture_credential(conn, &current);
            require_historical_credential_custody(conn, &claimed).unwrap();
            require_live_credential_custody(conn, &claimed).unwrap();
            assert!(transition(conn, &claimed, &claimed).is_err());
            let mut swapped = claimed.clone();
            swapped.credential_custody.as_mut().unwrap().contract_digest = "b".repeat(64);
            assert!(transition(conn, &claimed, &swapped).is_err());
            // A cold read of lost custody cannot treat an unlocked profile as safe.
            conn.execute_batch("DROP TRIGGER credential_profile_qualification_custody_guard; UPDATE credential_profile SET lock_owner=NULL;").unwrap();
            assert!(require_historical_credential_custody(conn, &claimed).is_err());
            Ok((current, claimed))
        });
        crate::runtime_db::test_native::with_transient(|other_conn| {
            let other_reserved = fixture_journal(other_conn);
            other_conn.execute("INSERT INTO credential_profile VALUES('profile','principal','private-home',1,1,?1,'active')", [&other_reserved.launch_owner]).unwrap();
            other_conn.execute("UPDATE external_execution_allocation SET reservation_json=json_set(reservation_json,'$.owner.operator_principal','another-principal')", []).unwrap();
            assert!(transition(other_conn, &other_reserved, &claimed).is_err());
            Ok(())
        });
        let mut absent = serde_json::to_value(&current).unwrap();
        absent.as_object_mut().unwrap().remove("credential_custody");
        assert!(serde_json::from_value::<QualificationLocalStartRecord>(absent).is_err());
        let mut absent = serde_json::to_value(&claimed).unwrap();
        absent["credential_custody"]
            .as_object_mut()
            .unwrap()
            .remove("released_at_ms");
        assert!(serde_json::from_value::<QualificationLocalStartRecord>(absent).is_err());
    }

    #[test]
    fn credential_check_borrows_outer_transaction_without_renewing_original_generation() {
        crate::runtime_db::test_native::with_transient(|conn| {
            let original = fixture_retained_credential_journal(conn);
            let claimed = claim_fixture_credential(conn, &original);
            assert_eq!(
                read_local_start(conn, &claimed.placement_thread_id).unwrap(),
                Some(claimed.clone())
            );
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
            require_live_credential_custody(&tx, &claimed).unwrap();
            // This fixture's original daemon is intentionally stale. The borrowed
            // check must reach that semantic fence, rather than fail a nested BEGIN.
            // This is a refused fresh-use check, not proof of a successful held attach.
            assert_eq!(claimed.phase, QualificationLocalStartPhase::Reserved);
            assert!(claimed.directory_borrow_intent.is_none());
            assert!(claimed.worker_incarnation_digest.is_none());
            let error =
                require_live_credential_start_tx(&tx, &claimed.placement_thread_id).unwrap_err();
            assert!(
                error.to_string().contains("another daemon generation"),
                "{error:#}"
            );
            assert!(!tx.is_autocommit());
            // Revocation fences fresh use while unresolved Q custody keeps its
            // original lock; a terminal revoked state is deliberately forbidden.
            tx.execute("UPDATE credential_profile SET state='revoking',credential_generation=2,authority_revision=2", []).unwrap();
            require_historical_credential_custody(&tx, &claimed).unwrap();
            assert!(require_live_credential_custody(&tx, &claimed).is_err());
            tx.rollback().unwrap();
            require_live_credential_custody(conn, &claimed).unwrap();
            assert_eq!(
                read_local_start(conn, &claimed.placement_thread_id).unwrap(),
                Some(claimed)
            );
            Ok(())
        });
    }

    #[test]
    fn revocation_fences_fresh_use_without_erasing_historical_q_custody() {
        crate::runtime_db::test_native::with_transient(|conn| {
            let current = fixture_journal(conn);
            let claimed = claim_fixture_credential(conn, &current);
            conn.execute("UPDATE credential_profile SET state='revoking',credential_generation=2,authority_revision=2", []).unwrap();
            require_historical_credential_custody(conn, &claimed).unwrap();
            assert!(require_live_credential_custody(conn, &claimed).is_err());
            for sql in [
                "UPDATE credential_profile SET lock_owner=NULL",
                "UPDATE credential_profile SET state='revoked',lock_owner=NULL",
                "UPDATE credential_profile SET state='deleting'",
                "DELETE FROM credential_profile",
            ] {
                assert!(conn.execute(sql, []).is_err(), "{sql}");
            }
            require_local_processes_settled(conn, &claimed).unwrap();
            let mut released = claimed.clone();
            released.credential_custody.as_mut().unwrap().released_at_ms = Some(1);
            let tx = conn.unchecked_transaction().unwrap();
            transition(&tx, &claimed, &released).unwrap();
            tx.execute("UPDATE credential_profile SET lock_owner=NULL", [])
                .unwrap();
            tx.commit().unwrap();
            require_historical_credential_custody(conn, &released).unwrap();
            // Historical cleanup must not clear a successor's lock or demand the
            // original generation still be active after a completed release.
            conn.execute("UPDATE credential_profile SET state='active',credential_generation=3,authority_revision=3,lock_owner='successor'", []).unwrap();
            require_historical_credential_custody(conn, &released).unwrap();
            assert!(transition(conn, &released, &claimed).is_err());
            assert_eq!(
                conn.query_row("SELECT lock_owner FROM credential_profile", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
                "successor"
            );
            Ok(())
        });
    }

    #[test]
    fn unregistered_rollback_and_resume_release_credentials_before_closed() {
        for release_phase in [
            QualificationLocalStartPhase::Reserved,
            QualificationLocalStartPhase::CleanupIntent,
            QualificationLocalStartPhase::WorkspaceRemoved,
            QualificationLocalStartPhase::StateRemoved,
        ] {
            crate::runtime_db::test_native::with_transient(|conn| {
                let reserved = fixture_journal(conn);
                let mut current = claim_fixture_credential(conn, &reserved);
                for next_phase in [
                    QualificationLocalStartPhase::CleanupIntent,
                    QualificationLocalStartPhase::WorkspaceRemoved,
                    QualificationLocalStartPhase::StateRemoved,
                    QualificationLocalStartPhase::Closed,
                ] {
                    if current.phase == release_phase {
                        require_local_processes_settled(conn, &current).unwrap();
                        let mut released = current.clone();
                        released.credential_custody.as_mut().unwrap().released_at_ms = Some(1);
                        let tx = conn.unchecked_transaction().unwrap();
                        transition(&tx, &current, &released).unwrap();
                        tx.execute("UPDATE credential_profile SET lock_owner=NULL", [])
                            .unwrap();
                        tx.commit().unwrap();
                        // A pre-release snapshot is stale after this CAS. Resuming
                        // cleanup must use the newly retained canonical row.
                        assert!(transition(conn, &current, &released).is_err());
                        current = released;
                    }
                    let mut next = current.clone();
                    next.phase = next_phase;
                    match next_phase {
                        QualificationLocalStartPhase::CleanupIntent => {
                            next.cleanup = Some(QualificationLocalCleanupEvidence {
                                prior_failure_reason: None,
                                intent_at_ms: 1,
                                workspace_removed_at_ms: None,
                                state_removed_at_ms: None,
                                closed_at_ms: None,
                            })
                        }
                        QualificationLocalStartPhase::WorkspaceRemoved => {
                            next.cleanup.as_mut().unwrap().workspace_removed_at_ms = Some(1)
                        }
                        QualificationLocalStartPhase::StateRemoved => {
                            next.cleanup.as_mut().unwrap().state_removed_at_ms = Some(1)
                        }
                        QualificationLocalStartPhase::Closed => {
                            next.cleanup.as_mut().unwrap().closed_at_ms = Some(1)
                        }
                        _ => unreachable!(),
                    }
                    transition(conn, &current, &next).unwrap();
                    current = next;
                }
                assert_eq!(current.phase, QualificationLocalStartPhase::Closed);
                require_historical_credential_custody(conn, &current).unwrap();
                Ok(())
            });
        }
    }

    #[test]
    fn uncertain_directory_borrow_and_live_connector_prevent_credential_release() {
        crate::runtime_db::test_native::with_transient(|conn| {
            let current = fixture_journal(conn);
            let claimed = claim_fixture_credential(conn, &current);
            let mut released = claimed.clone();
            released.credential_custody.as_mut().unwrap().released_at_ms = Some(1);
            conn.execute(
                "INSERT INTO external_execution_connector VALUES(?1,'connected')",
                [&claimed.placement_thread_id],
            )
            .unwrap();
            assert!(require_local_processes_settled(conn, &claimed).is_err());
            assert!(transition(conn, &claimed, &released).is_err());
            conn.execute("UPDATE external_execution_connector SET state='closed'", [])
                .unwrap();
            let mut ready = claimed.clone();
            ready.phase = QualificationLocalStartPhase::WorkspaceReady;
            ready.workspace_identity = Some(directory_identity(2));
            transition(conn, &claimed, &ready).unwrap();
            let mut borrowed = ready.clone();
            borrowed.directory_borrow_intent = Some(QualificationWorkspaceBorrowIntent {
                workspace_id: ready.workspace_id.clone(),
                directory_identity: ready.workspace_identity.unwrap(),
                reserved_at_ms: 1,
            });
            transition(conn, &ready, &borrowed).unwrap();
            let mut released = borrowed.clone();
            released.credential_custody.as_mut().unwrap().released_at_ms = Some(1);
            assert!(require_local_processes_settled(conn, &borrowed).is_err());
            assert!(transition(conn, &borrowed, &released).is_err());
            Ok(())
        });
    }

    #[test]
    fn private_directory_journal_pins_identity_and_allows_one_borrow() {
        crate::runtime_db::test_native::with_transient(|conn| {
            let mut current = fixture_journal(conn);
            let mut ready = current.clone();
            ready.phase = QualificationLocalStartPhase::WorkspaceReady;
            ready.workspace_identity = Some(directory_identity(2));
            transition(conn, &current, &ready).unwrap();
            current = ready;
            let mut borrowed = current.clone();
            borrowed.directory_borrow_intent = Some(QualificationWorkspaceBorrowIntent {
                workspace_id: current.workspace_id.clone(),
                directory_identity: current.workspace_identity.unwrap(),
                reserved_at_ms: 1,
            });
            transition(conn, &current, &borrowed).unwrap();
            assert!(transition(conn, &borrowed, &borrowed).is_err());
            let mut replaced = borrowed.clone();
            replaced.workspace_identity = Some(directory_identity(3));
            replaced
                .directory_borrow_intent
                .as_mut()
                .unwrap()
                .directory_identity = directory_identity(3);
            assert!(transition(conn, &borrowed, &replaced).is_err());
            let mut skipped = borrowed.clone();
            skipped.phase = QualificationLocalStartPhase::WorkspaceRemoved;
            assert!(transition(conn, &borrowed, &skipped).is_err());
            Ok(())
        });
    }
    // Public shape testimony for isolated SQL transition tests. These rows do
    // not mint original file custody, accepted-root proof, or adapter execution.
    fn canary_stage_fixture(
        current: &QualificationLocalStartRecord,
    ) -> (
        QualificationControllerCanaryIntent,
        QControllerCanaryBinding,
    ) {
        use ryeos_external_execution_contract::qualification_request_binding::{
            QualificationRequestBindingInput, QualificationRequestBindingOutput,
        };
        use ryeos_state::external_execution::qualification_controller_canary::QControllerCanaryContract;
        let path = format!("{}/controller-canary/canary", current.state_root);
        let input = QualificationRequestBindingInput {
            schema: 1,
            template: serde_json::json!({"provider_defined":"template"}),
            controller_canary_path: path.clone(),
        };
        let output = QualificationRequestBindingOutput {
            schema: 1,
            input_digest: input.digest().unwrap(),
            resolved_body: serde_json::json!({"provider_defined":"bound"}),
        };
        let binding:QControllerCanaryBinding=serde_json::from_value(serde_json::json!({
            "schema":1,"controller_path":path,
            "initial_file":{"directory_identity":{"containing_device":1,"inode":44},
                "file_identity":{"containing_device":1,"inode":45},"content_sha256":"a".repeat(64),"size_bytes":65,"permission_mode":384},
            "provider_declaration_id":"provider","configuration_adapter_artifact_hash":"b".repeat(64),"configuration_adapter_artifact_bytes":1,
            "binding_input_digest":input.digest().unwrap(),"binding_output_digest":output.digest().unwrap(),
            "template_request_digest":ryeos_state::objects::canonical_value_digest(&input.template).unwrap(),
            "resolved_request_digest":ryeos_state::objects::canonical_value_digest(&output.resolved_body).unwrap(),
            "resolved_body":output.resolved_body,
        })).unwrap();
        binding.validate().unwrap();
        let intent = QualificationControllerCanaryIntent {
            schema: 1,
            contract: QControllerCanaryContract::OriginalControllerReadAndLocalRefusalV1,
            controller_path: path,
            program_digest: "c".repeat(64),
            provider_declaration_id: binding.provider_declaration_id.clone(),
            configuration_adapter_artifact_hash: binding
                .configuration_adapter_artifact_hash
                .clone(),
            configuration_adapter_artifact_bytes: binding.configuration_adapter_artifact_bytes,
            guest_input_identity: "d".repeat(64),
            template_request_digest: binding.template_request_digest.clone(),
            reserved_at_ms: 1,
        };
        (intent, binding)
    }

    #[test]
    fn canary_journal_requires_append_once_intent_before_binding_and_never_clears_it() {
        crate::runtime_db::test_native::with_transient(|conn| {
            let initial = fixture_journal(conn);
            let current = claim_fixture_credential(conn, &initial);
            let (intent, binding) = canary_stage_fixture(&current);
            let mut reserved = current.clone();
            reserved.controller_canary_intent = Some(intent);
            transition(conn, &current, &reserved).unwrap();
            assert!(transition(conn, &reserved, &reserved).is_err());
            assert!(transition(conn, &reserved, &current).is_err());
            let mut substituted = reserved.clone();
            substituted
                .controller_canary_intent
                .as_mut()
                .unwrap()
                .guest_input_identity = "e".repeat(64);
            assert!(transition(conn, &reserved, &substituted).is_err());
            let mut bound = reserved.clone();
            bound.controller_canary_binding = Some(binding);
            transition(conn, &reserved, &bound).unwrap();
            assert!(transition(conn, &bound, &bound).is_err());
            assert!(transition(conn, &bound, &reserved).is_err());
            // Neither stripping the required-null field nor changing one mirrored
            // SQL column can erase or replace the original append-only commitment.
            assert!(conn.execute("UPDATE external_qualification_local_start SET record_json=json_remove(record_json,'$.controller_canary_intent')",[]).is_err());
            assert!(
                conn.execute(
                    "UPDATE external_qualification_local_start SET controller_canary_binding_json=NULL",
                    []
                )
                .is_err()
            );
            let encoded: String = conn
                .query_row(
                    "SELECT record_json FROM external_qualification_local_start",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(encoded, canonical(&bound).unwrap());
            // Unsuccessful preparation is still cleanable before any directory
            // borrow. No missing terminal observation may strand proven-safe cleanup.
            require_local_processes_settled(conn, &bound).unwrap();
            let mut released = bound.clone();
            released.credential_custody.as_mut().unwrap().released_at_ms = Some(1);
            transition(conn, &bound, &released).unwrap();
            let mut late = released.clone();
            late.controller_canary_terminal = Some(QControllerCanaryEvidence {
                schema: 1,
                terminal_file: bound
                    .controller_canary_binding
                    .as_ref()
                    .unwrap()
                    .initial_file
                    .clone(),
                binding: bound.controller_canary_binding.clone().unwrap(),
            });
            assert!(transition(conn, &released, &late).is_err());
            Ok(())
        });
    }

    #[test]
    fn canary_journal_cold_shape_refuses_missing_fields_or_binding_without_original_intent() {
        let original = fixture_record();
        for field in [
            "controller_canary_intent",
            "controller_canary_binding",
            "controller_canary_terminal",
        ] {
            let mut value = serde_json::to_value(&original).unwrap();
            value.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<QualificationLocalStartRecord>(value).is_err());
        }
        let (_, binding) = canary_stage_fixture(&original);
        let mut changed = original.clone();
        changed.controller_canary_binding = Some(binding);
        assert!(changed.validate().is_err());
        let (intent, _) = canary_stage_fixture(&original);
        changed.controller_canary_intent = Some(intent);
        changed.validate().unwrap();
        changed
            .controller_canary_binding
            .as_mut()
            .unwrap()
            .configuration_adapter_artifact_hash = "f".repeat(64);
        assert!(changed.validate().is_err());
    }
}
