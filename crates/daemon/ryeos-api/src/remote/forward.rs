//! Shared unary remote execution helper.
//!
//! Provides a single `execute_unary_forward` function that implements the
//! push → execute → pull/apply cycle. Used by:
//! - `remote_execute` handler (existing explicit remote execution)
//! - `execute_mode` response mode (future target-site forwarding in Phase 3)
//!
//! The helper takes an already-resolved `ResolvedRemote` (from Phase 1's
//! `resolve_remote_by_site_id`) so it doesn't duplicate remote config
//! lookup logic. Callers are responsible for:
//! - resolving the remote
//! - validating project bindings
//! - resolving ignore rules
//! - checking self-target conditions
//!
//! The impl doc calls this "one production implementation of
//! push/execute/pull".

use std::path::Path;

use serde_json::Value;

use crate::remote::client::RemoteClient;
use crate::remote::config::ResolvedRemote;
use crate::remote::pull::{extract_snapshot_hash, pull_results, PullResultsError};
use crate::remote::push::{push_project, PushResult};
use ryeos_app::ignore::IgnoreMatcher;
use ryeos_app::state::AppState;
use ryeos_state::{
    FinishSyncJobAttempt, NewSyncJob, NewSyncJobAttempt, SyncJobAttemptState, SyncJobState,
    SyncJobUpdate,
};

/// Request shape for the shared unary forward helper.
///
/// Designed so both the explicit `remote_execute` handler and the
/// future `execute_mode` target-site forwarding path can construct
/// this from their respective request types.
pub struct RemoteForwardRequest<'a> {
    /// Already-resolved remote (from `resolve_remote_by_site_id`
    /// or `from_named_remote`).
    pub remote: &'a ResolvedRemote,
    /// Canonical item ref to execute on the remote.
    pub item_ref: &'a str,
    /// Local project root path. `None` for --no-project mode.
    pub local_project_path: Option<&'a Path>,
    /// Remote project path used in push-head ref and /execute body.
    pub remote_project_path: &'a str,
    /// Parameters for the item.
    pub parameters: Value,
    /// Acting principal (caller identity).
    pub acting_principal: &'a str,
    /// Ignore rules for project ingest.
    pub remote_ignore: &'a IgnoreMatcher,
    /// Optional operation name. If `None`, the remote uses its
    /// default. Forwarded as-is to the remote /execute body.
    pub operation: Option<&'a str>,
    /// Optional op-specific inputs. Forwarded as-is to the remote
    /// /execute body.
    pub inputs: Option<Value>,
}

/// Result from the shared unary forward helper.
pub struct RemoteForwardResult {
    /// The full remote execute response JSON.
    pub remote_result: Value,
    /// Push summary.
    pub push_summary: PushSummary,
    /// Snapshot hash of the remote execution result.
    pub result_snapshot_hash: String,
    /// Pull summary.
    pub pull_summary: PullSummary,
}

/// Push-phase summary for the response envelope.
pub struct PushSummary {
    pub pushed_snapshot_hash: String,
    pub manifest_entries: usize,
    pub blobs_uploaded: usize,
    pub blobs_skipped: usize,
}

/// Pull-phase summary for the response envelope.
pub struct PullSummary {
    pub snapshot_hash: String,
    pub cas_objects_fetched: usize,
    pub files_updated: usize,
    pub files_deleted: usize,
    pub user_files_updated: usize,
    pub user_files_deleted: usize,
}

/// Errors from the unary forward pipeline.
#[derive(Debug, thiserror::Error)]
pub enum RemoteForwardError {
    /// Durable sync job ledger failed before or during remote orchestration.
    #[error("sync job ledger failed: {0}")]
    JobLedgerFailed(String),
    /// Push phase failed.
    #[error("push failed: {0}")]
    PushFailed(String),
    /// Remote /execute failed.
    #[error("remote execute failed: {0}")]
    ExecuteFailed(String),
    /// Remote result has no snapshot hash.
    #[error("remote result missing snapshot hash — async remote execute not supported")]
    MissingSnapshotHash,
    /// Pull found local workspace changes since push.
    #[error("local workspace conflict at '{path}' — local files changed since push")]
    PullLocalConflict { path: String },
    /// Pull saw a result without a snapshot hash.
    #[error("remote execution result missing snapshot_hash")]
    PullMissingSnapshotHash,
    /// Pull found a malformed remote snapshot.
    #[error("invalid remote snapshot: {message}")]
    PullInvalidRemoteSnapshot { message: String },
    /// Pull refused to apply an unrelated snapshot.
    #[error("remote returned result snapshot '{result}' which is not a descendant of pushed snapshot '{pushed}' — refusing to apply")]
    PullUnrelatedSnapshot { pushed: String, result: String },
    /// Pull phase failed unexpectedly.
    #[error("pull results failed: {0}")]
    PullFailed(String),
}

impl RemoteForwardError {
    /// Machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::JobLedgerFailed(_) => "remote_job_ledger_failed",
            Self::PushFailed(_) => "remote_push_failed",
            Self::ExecuteFailed(_) => "remote_execute_failed",
            Self::MissingSnapshotHash => "remote_missing_snapshot_hash",
            Self::PullLocalConflict { .. } => "remote_pull_local_conflict",
            Self::PullMissingSnapshotHash => "remote_pull_missing_snapshot_hash",
            Self::PullInvalidRemoteSnapshot { .. } => "remote_pull_invalid_snapshot",
            Self::PullUnrelatedSnapshot { .. } => "remote_pull_unrelated_snapshot",
            Self::PullFailed(_) => "remote_pull_failed",
        }
    }
}

/// Execute a unary remote forward: push → execute → pull/apply.
///
/// This is the single production implementation of the push/execute/pull
/// pipeline. Both the explicit `remote_execute` handler and the future
/// `execute_mode` target-site forwarding call this function.
pub async fn execute_unary_forward(
    state: &std::sync::Arc<AppState>,
    client: &RemoteClient,
    req: RemoteForwardRequest<'_>,
) -> Result<RemoteForwardResult, RemoteForwardError> {
    let job_id = format!("remote-execute:{}", uuid::Uuid::new_v4());
    let attempt_id = format!("remote-execute-attempt:{}", uuid::Uuid::new_v4());
    record_sync_job(
        state,
        &NewSyncJob {
            job_id: job_id.clone(),
            operation_type: "remote_execute".to_string(),
            peer: Some(client.base_url().to_string()),
            roots: Vec::new(),
            heads: Vec::new(),
            max_attempts: 1,
        },
    )?;
    record_sync_job_attempt(
        state,
        &NewSyncJobAttempt {
            attempt_id: attempt_id.clone(),
            job_id: job_id.clone(),
            worker_id: Some("remote-forward".to_string()),
            phase: "push_uploading".to_string(),
        },
    )?;
    update_sync_job(
        state,
        &job_id,
        SyncJobUpdate {
            state: SyncJobState::Running,
            phase: "push_uploading".to_string(),
            roots: None,
            heads: None,
            uploaded_hashes: Vec::new(),
            fetched_hashes: Vec::new(),
            last_error: None,
            result: None,
            increment_attempts: false,
        },
    )?;

    // 1. Push project (or no-project user-space-only push).
    let push_result = match req.local_project_path {
        Some(proj_path) => push_project(
            client,
            state,
            proj_path,
            req.remote_project_path,
            req.remote_ignore,
        )
        .await
        .map_err(|e| {
            let message = format!("{e:#}");
            let _ = update_sync_job(
                state,
                &job_id,
                SyncJobUpdate {
                    state: SyncJobState::Failed,
                    phase: "push_failed".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: Vec::new(),
                    fetched_hashes: Vec::new(),
                    last_error: Some(message.clone()),
                    result: None,
                    increment_attempts: false,
                },
            );
            let _ = finish_sync_job_attempt(
                state,
                &attempt_id,
                FinishSyncJobAttempt {
                    state: SyncJobAttemptState::Failed,
                    phase: "push_failed".to_string(),
                    error: Some(message.clone()),
                    result: None,
                },
            );
            RemoteForwardError::PushFailed(message)
        })?,
        None => {
            // --no-project mode: push user space only.
            match push_no_project(state, client, req.remote_project_path).await {
                Ok(value) => value,
                Err(err) => {
                    let _ = update_sync_job(
                        state,
                        &job_id,
                        SyncJobUpdate {
                            state: SyncJobState::Failed,
                            phase: "push_failed".to_string(),
                            roots: None,
                            heads: None,
                            uploaded_hashes: Vec::new(),
                            fetched_hashes: Vec::new(),
                            last_error: Some(err.to_string()),
                            result: None,
                            increment_attempts: false,
                        },
                    );
                    let _ = finish_sync_job_attempt(
                        state,
                        &attempt_id,
                        FinishSyncJobAttempt {
                            state: SyncJobAttemptState::Failed,
                            phase: "push_failed".to_string(),
                            error: Some(err.to_string()),
                            result: None,
                        },
                    );
                    return Err(err);
                }
            }
        }
    };

    update_sync_job(
        state,
        &job_id,
        SyncJobUpdate {
            state: SyncJobState::Running,
            phase: "remote_execute".to_string(),
            roots: Some(vec![push_result.snapshot_hash.clone()]),
            heads: None,
            uploaded_hashes: vec![push_result.snapshot_hash.clone()],
            fetched_hashes: Vec::new(),
            last_error: None,
            result: None,
            increment_attempts: false,
        },
    )?;

    // 2. Execute on remote with project_source: pushed_head.
    let remote_result = match client
        .execute_with_options(
            req.item_ref,
            req.remote_project_path,
            &req.parameters,
            "pushed_head",
            req.operation,
            req.inputs.as_ref(),
        )
        .await
    {
        Ok(value) => value,
        Err(e) => {
            let message = format!("{e:#}");
            update_sync_job(
                state,
                &job_id,
                SyncJobUpdate {
                    state: SyncJobState::Failed,
                    phase: "remote_execute_failed".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![push_result.snapshot_hash.clone()],
                    fetched_hashes: Vec::new(),
                    last_error: Some(message.clone()),
                    result: None,
                    increment_attempts: false,
                },
            )?;
            finish_sync_job_attempt(
                state,
                &attempt_id,
                FinishSyncJobAttempt {
                    state: SyncJobAttemptState::Failed,
                    phase: "remote_execute_failed".to_string(),
                    error: Some(message.clone()),
                    result: None,
                },
            )?;
            return Err(RemoteForwardError::ExecuteFailed(message));
        }
    };

    // 3. Extract result snapshot hash.
    let Some(result_snapshot_hash) = extract_snapshot_hash(&remote_result) else {
        update_sync_job(
            state,
            &job_id,
            SyncJobUpdate {
                state: SyncJobState::Failed,
                phase: "missing_snapshot_hash".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![push_result.snapshot_hash.clone()],
                fetched_hashes: Vec::new(),
                last_error: Some("remote result missing snapshot hash".to_string()),
                result: Some(remote_result.clone()),
                increment_attempts: false,
            },
        )?;
        finish_sync_job_attempt(
            state,
            &attempt_id,
            FinishSyncJobAttempt {
                state: SyncJobAttemptState::Failed,
                phase: "missing_snapshot_hash".to_string(),
                error: Some("remote result missing snapshot hash".to_string()),
                result: Some(remote_result.clone()),
            },
        )?;
        return Err(RemoteForwardError::MissingSnapshotHash);
    };
    update_sync_job(
        state,
        &job_id,
        SyncJobUpdate {
            state: SyncJobState::Running,
            phase: "pull_results".to_string(),
            roots: None,
            heads: Some(vec![result_snapshot_hash.clone()]),
            uploaded_hashes: vec![push_result.snapshot_hash.clone()],
            fetched_hashes: vec![result_snapshot_hash.clone()],
            last_error: None,
            result: None,
            increment_attempts: false,
        },
    )?;

    // 4. Pull results and apply to local workspace.
    let pull_result = pull_results(
        client,
        &state.config.system_space_dir,
        &push_result.snapshot_hash,
        &result_snapshot_hash,
        req.local_project_path,
        &push_result.manifest,
        push_result.user_manifest.as_ref(),
    )
    .await
    .map_err(|e| {
        let err = match e {
            PullResultsError::LocalConflict(path) => RemoteForwardError::PullLocalConflict { path },
            PullResultsError::MissingSnapshotHash => RemoteForwardError::PullMissingSnapshotHash,
            PullResultsError::InvalidRemoteSnapshot(message) => {
                RemoteForwardError::PullInvalidRemoteSnapshot { message }
            }
            PullResultsError::UnrelatedSnapshot { pushed, result } => {
                RemoteForwardError::PullUnrelatedSnapshot { pushed, result }
            }
            PullResultsError::Other(e) => RemoteForwardError::PullFailed(format!("{e:#}")),
        };
        let _ = update_sync_job(
            state,
            &job_id,
            SyncJobUpdate {
                state: SyncJobState::Failed,
                phase: "pull_results_failed".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![push_result.snapshot_hash.clone()],
                fetched_hashes: vec![result_snapshot_hash.clone()],
                last_error: Some(err.to_string()),
                result: Some(remote_result.clone()),
                increment_attempts: false,
            },
        );
        let _ = finish_sync_job_attempt(
            state,
            &attempt_id,
            FinishSyncJobAttempt {
                state: SyncJobAttemptState::Failed,
                phase: "pull_results_failed".to_string(),
                error: Some(err.to_string()),
                result: Some(remote_result.clone()),
            },
        );
        err
    })?;

    update_sync_job(
        state,
        &job_id,
        SyncJobUpdate {
            state: SyncJobState::Completed,
            phase: "completed".to_string(),
            roots: None,
            heads: None,
            uploaded_hashes: vec![push_result.snapshot_hash.clone()],
            fetched_hashes: vec![result_snapshot_hash.clone()],
            last_error: None,
            result: Some(serde_json::json!({
                "snapshot_hash": pull_result.snapshot_hash,
                "cas_objects_fetched": pull_result.cas_objects_fetched,
                "files_updated": pull_result.files_updated,
                "files_deleted": pull_result.files_deleted,
                "user_files_updated": pull_result.user_files_updated,
                "user_files_deleted": pull_result.user_files_deleted,
            })),
            increment_attempts: false,
        },
    )?;
    finish_sync_job_attempt(
        state,
        &attempt_id,
        FinishSyncJobAttempt {
            state: SyncJobAttemptState::Completed,
            phase: "completed".to_string(),
            error: None,
            result: Some(serde_json::json!({
                "snapshot_hash": pull_result.snapshot_hash,
                "cas_objects_fetched": pull_result.cas_objects_fetched,
                "files_updated": pull_result.files_updated,
                "files_deleted": pull_result.files_deleted,
                "user_files_updated": pull_result.user_files_updated,
                "user_files_deleted": pull_result.user_files_deleted,
            })),
        },
    )?;

    Ok(RemoteForwardResult {
        remote_result,
        push_summary: PushSummary {
            pushed_snapshot_hash: push_result.snapshot_hash,
            manifest_entries: push_result.manifest_entries,
            blobs_uploaded: push_result.blobs_uploaded,
            blobs_skipped: push_result.blobs_skipped,
        },
        result_snapshot_hash,
        pull_summary: PullSummary {
            snapshot_hash: pull_result.snapshot_hash,
            cas_objects_fetched: pull_result.cas_objects_fetched,
            files_updated: pull_result.files_updated,
            files_deleted: pull_result.files_deleted,
            user_files_updated: pull_result.user_files_updated,
            user_files_deleted: pull_result.user_files_deleted,
        },
    })
}

fn record_sync_job(
    state: &std::sync::Arc<AppState>,
    job: &NewSyncJob,
) -> Result<(), RemoteForwardError> {
    state
        .state_store
        .with_state_db(|db| db.create_sync_job(job))
        .map(|_| ())
        .map_err(|err| RemoteForwardError::JobLedgerFailed(format!("{err:#}")))
}

fn record_sync_job_attempt(
    state: &std::sync::Arc<AppState>,
    attempt: &NewSyncJobAttempt,
) -> Result<(), RemoteForwardError> {
    state
        .state_store
        .with_state_db(|db| db.create_sync_job_attempt(attempt))
        .map(|_| ())
        .map_err(|err| RemoteForwardError::JobLedgerFailed(format!("{err:#}")))
}

fn finish_sync_job_attempt(
    state: &std::sync::Arc<AppState>,
    attempt_id: &str,
    finish: FinishSyncJobAttempt,
) -> Result<(), RemoteForwardError> {
    state
        .state_store
        .with_state_db(|db| db.finish_sync_job_attempt(attempt_id, &finish))
        .map_err(|err| RemoteForwardError::JobLedgerFailed(format!("{err:#}")))
}

fn update_sync_job(
    state: &std::sync::Arc<AppState>,
    job_id: &str,
    update: SyncJobUpdate,
) -> Result<(), RemoteForwardError> {
    state
        .state_store
        .with_state_db(|db| db.update_sync_job(job_id, &update))
        .map_err(|err| RemoteForwardError::JobLedgerFailed(format!("{err:#}")))
}

/// --no-project mode: push user space only (no project ingest).
///
/// This is extracted from the inline push logic in `remote_execute.rs`.
/// It creates an empty manifest, ingests user space, and uploads.
async fn push_no_project(
    state: &AppState,
    client: &RemoteClient,
    remote_project_path: &str,
) -> Result<PushResult, RemoteForwardError> {
    use crate::remote::push::{collect_snapshot_hashes, upload_missing};

    let local_cas_root = state
        .config
        .system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("objects");
    let local_cas = lillux::cas::CasStore::new(local_cas_root);

    let empty_manifest = ryeos_state::objects::SourceManifest {
        item_source_hashes: std::collections::HashMap::new(),
    };
    let manifest_hash = local_cas
        .store_object(&empty_manifest.to_value())
        .map_err(|e| RemoteForwardError::PushFailed(format!("store empty manifest: {e:#}")))?;

    let (user_manifest_hash, user_manifest) =
        crate::remote::push::ingest_user_space_for_push(&local_cas)
            .map_err(|e| RemoteForwardError::PushFailed(format!("ingest user space: {e:#}")))?;

    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_manifest_hash: manifest_hash.clone(),
        user_manifest_hash: user_manifest_hash.clone(),
        project_sync_scope: ryeos_state::project_sync::ProjectSyncScope::FullProject,
        parent_hashes: Vec::new(),
        created_at: lillux::time::iso8601_now(),
        source: "push-no-project".to_string(),
    };
    let snapshot_hash = local_cas
        .store_object(&snapshot.to_value())
        .map_err(|e| RemoteForwardError::PushFailed(format!("store snapshot: {e:#}")))?;

    let all_hashes = collect_snapshot_hashes(
        &local_cas,
        &empty_manifest,
        user_manifest.as_ref(),
        user_manifest_hash.as_deref(),
        &manifest_hash,
        &snapshot_hash,
    );
    let (blobs_uploaded, blobs_skipped) = upload_missing(client, &local_cas, &all_hashes)
        .await
        .map_err(|e| RemoteForwardError::PushFailed(format!("upload missing objects: {e:#}")))?;

    client
        .push_head(remote_project_path, &snapshot_hash)
        .await
        .map_err(|e| RemoteForwardError::PushFailed(format!("push_head failed: {e:#}")))?;

    Ok(PushResult {
        snapshot_hash,
        manifest_hash,
        manifest: empty_manifest,
        manifest_entries: 0,
        blobs_uploaded,
        blobs_skipped,
        user_manifest_hash,
        user_manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(
            RemoteForwardError::JobLedgerFailed("x".into()).code(),
            "remote_job_ledger_failed"
        );
        assert_eq!(
            RemoteForwardError::PushFailed("x".into()).code(),
            "remote_push_failed"
        );
        assert_eq!(
            RemoteForwardError::ExecuteFailed("x".into()).code(),
            "remote_execute_failed"
        );
        assert_eq!(
            RemoteForwardError::MissingSnapshotHash.code(),
            "remote_missing_snapshot_hash"
        );
        assert_eq!(
            RemoteForwardError::PullLocalConflict { path: "x".into() }.code(),
            "remote_pull_local_conflict"
        );
        assert_eq!(
            RemoteForwardError::PullMissingSnapshotHash.code(),
            "remote_pull_missing_snapshot_hash"
        );
        assert_eq!(
            RemoteForwardError::PullInvalidRemoteSnapshot {
                message: "x".into()
            }
            .code(),
            "remote_pull_invalid_snapshot"
        );
        assert_eq!(
            RemoteForwardError::PullUnrelatedSnapshot {
                pushed: "a".into(),
                result: "b".into(),
            }
            .code(),
            "remote_pull_unrelated_snapshot"
        );
        assert_eq!(
            RemoteForwardError::PullFailed("x".into()).code(),
            "remote_pull_failed"
        );
    }

    #[test]
    fn error_display_includes_context() {
        let e = RemoteForwardError::PushFailed("connection refused".into());
        assert!(e.to_string().contains("push failed"), "got: {e}");
        assert!(e.to_string().contains("connection refused"), "got: {e}");

        let e = RemoteForwardError::ExecuteFailed("502 Bad Gateway".into());
        assert!(e.to_string().contains("remote execute failed"), "got: {e}");

        let e = RemoteForwardError::PullFailed("conflict at /foo".into());
        assert!(e.to_string().contains("pull results failed"), "got: {e}");
    }

    #[test]
    fn push_summary_fields() {
        let s = PushSummary {
            pushed_snapshot_hash: "snap".into(),
            manifest_entries: 3,
            blobs_uploaded: 2,
            blobs_skipped: 1,
        };
        assert_eq!(s.pushed_snapshot_hash, "snap");
        assert_eq!(s.manifest_entries, 3);
        assert_eq!(s.blobs_uploaded, 2);
        assert_eq!(s.blobs_skipped, 1);
    }

    #[test]
    fn pull_summary_fields() {
        let s = PullSummary {
            snapshot_hash: "snap".into(),
            cas_objects_fetched: 5,
            files_updated: 3,
            files_deleted: 1,
            user_files_updated: 2,
            user_files_deleted: 0,
        };
        assert_eq!(s.snapshot_hash, "snap");
        assert_eq!(s.cas_objects_fetched, 5);
        assert_eq!(s.files_updated, 3);
        assert_eq!(s.files_deleted, 1);
        assert_eq!(s.user_files_updated, 2);
        assert_eq!(s.user_files_deleted, 0);
    }
}
