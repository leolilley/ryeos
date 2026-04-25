//! Unified execution runner.
//!
//! Both `/execute` and inbound webhooks use this to run the full
//! execution lifecycle: CAS context → snapshot → spawn →
//! fold-back → finalize → cleanup.
//!
//! The `ExecutionGuard` struct tracks all state and ensures cleanup
//! on every exit path (temp dir removal, thread finalization on
//! pre-spawn failures).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::task;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{ExecutionCompletion, PlanContext, ProjectContext};

use crate::launch_metadata::ResumeContext;
use crate::state_store::ThreadDetail;
use crate::services::thread_lifecycle::{
    self, ResolvedExecutionRequest, ThreadAttachProcessParams, ThreadFinalizeParams,
};
use crate::state::AppState;

/// All inputs needed to run an execution.
pub struct ExecutionParams {
    pub resolved: ResolvedExecutionRequest,
    pub acting_principal: String,
    /// Project working dir for live-FS execution and HEAD-ref naming.
    /// `None` only on resume of a thread whose original `ProjectContext`
    /// was non-`LocalPath` — in that case execution runs against the
    /// pinned snapshot and HEAD fold-back is skipped (there is no
    /// LocalPath to derive a project ref from).
    pub project_path: Option<PathBuf>,
    pub vault_bindings: HashMap<String, String>,
    pub snapshot_hash: Option<String>,
    pub parameters: Value,
    /// Pre-checked-out CAS temp dir (from ResolvedProjectContext).
    /// When set, the runner should reuse this instead of doing a second checkout.
    pub temp_dir: Option<PathBuf>,
}

/// Tracks execution state and ensures cleanup on all exit paths.
///
/// Call `cleanup()` explicitly when done (or on error). The guard does NOT
/// use Drop because cleanup is sync and we need access to AppState.
struct ExecutionGuard {
    state: AppState,
    thread_id: Option<String>,
    temp_dir: Option<PathBuf>,
    thread_finalized: bool,
}

impl ExecutionGuard {
    fn new(state: AppState) -> Self {
        Self {
            state,
            thread_id: None,
            temp_dir: None,
            thread_finalized: false,
        }
    }

    /// Mark thread as tracked by this guard.
    fn track_thread(&mut self, thread_id: &str) {
        self.thread_id = Some(thread_id.to_string());
    }

    /// Mark temp dir for cleanup.
    fn track_temp_dir(&mut self, dir: PathBuf) {
        self.temp_dir = Some(dir);
    }

    /// Fail the tracked thread if it hasn't been finalized yet.
    fn fail_thread(&mut self, outcome_code: &str) {
        if self.thread_finalized {
            return;
        }
        if let Some(ref tid) = self.thread_id {
            let _ = self.state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: tid.clone(),
                status: "failed".to_string(),
                outcome_code: Some(outcome_code.to_string()),
                result: None,
                error: Some(json!({ "code": outcome_code })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            });
            self.thread_finalized = true;
        }
    }

    /// Mark thread as finalized (by external code).
    fn mark_finalized(&mut self) {
        self.thread_finalized = true;
    }

    /// Perform all cleanup: remove temp dir.
    fn cleanup(&mut self) {
        if let Some(ref d) = self.temp_dir {
            if d.exists() {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }

    /// Consume into parts for moving into tokio::spawn.
    fn into_detached_parts(
        mut self,
    ) -> (AppState, Option<String>, Option<PathBuf>) {
        let state = self.state.clone();
        let thread_id = self.thread_id.take();
        let temp_dir = self.temp_dir.take();
        (state, thread_id, temp_dir)
    }
}

/// Prepare CAS execution context.
///
/// Two explicit paths:
///
/// 1. **CAS-backed**: `snapshot_hash` is provided → checkout from CAS into
///    an execution space. Used when the caller explicitly requests execution
///    against a pushed snapshot.
/// 2. **Live FS**: no `snapshot_hash` → ingest working dir into CAS for
///    tracking, execute against the live project path.
///
/// Returns (effective_project_path, pre_manifest_hash, base_snapshot_hash).
fn prepare_cas_context(
    state: &AppState,
    project_path: Option<&std::path::Path>,
    snapshot_hash: &Option<String>,
    pre_checkout_dir: &Option<PathBuf>,
    thread_id: &str,
    guard: &mut ExecutionGuard,
) -> Result<(PathBuf, Option<String>, Option<String>)> {
    // If we have a pre-checked-out dir from ResolvedProjectContext,
    // reuse it instead of doing a second checkout.
    if let (Some(snap_hash), Some(checkout_dir)) = (snapshot_hash, pre_checkout_dir) {
        let cas_root = state.state_store.cas_root()?;
        let cas = lillux::cas::CasStore::new(cas_root);
        let snap_obj = cas
            .get_object(snap_hash)?
            .ok_or_else(|| anyhow::anyhow!("snapshot {} not found in CAS", snap_hash))?;
        let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&snap_obj)?;
        let manifest_hash = snapshot.project_manifest_hash.clone();
        guard.track_temp_dir(checkout_dir.clone());
        tracing::trace!(thread_id = %thread_id, effective_path = %checkout_dir.display(), "CAS context prepared");
        return Ok((checkout_dir.clone(), Some(manifest_hash), Some(snap_hash.clone())));
    }

    if let Some(snap_hash) = snapshot_hash {
        let result = checkout_from_snapshot(state, snap_hash, thread_id, guard)?;
        tracing::trace!(thread_id = %thread_id, effective_path = %result.0.display(), "CAS context prepared");
        return Ok(result);
    }

    // Live FS — ingest working dir, no checkout. Requires a project_path.
    let project_path = project_path.ok_or_else(|| {
        anyhow::anyhow!(
            "live-FS execution requires a project_path; none provided \
             (caller passed neither snapshot_hash nor a working dir)"
        )
    })?;
    let _permit = state.write_barrier.try_acquire()
        .map_err(|e| anyhow::anyhow!("cannot acquire CAS write permit for ingest: {e}"))?;
    let cas_root = state.state_store.cas_root()?;
    let items = super::ingest::ingest_directory(&cas_root, project_path)?;
    let manifest = ryeos_state::objects::SourceManifest { item_source_hashes: items };
    let cas = lillux::cas::CasStore::new(cas_root);
    let manifest_hash = cas.store_object(&manifest.to_value())?;

    Ok((project_path.to_path_buf(), Some(manifest_hash), None))
}

/// Checkout a project from a CAS snapshot hash into an execution space.
///
/// Returns (exec_dir, manifest_hash, snapshot_hash).
fn checkout_from_snapshot(
    state: &AppState,
    snap_hash: &str,
    thread_id: &str,
    guard: &mut ExecutionGuard,
) -> Result<(PathBuf, Option<String>, Option<String>)> {
    let cas_root = state.state_store.cas_root()?;
    let cas = lillux::cas::CasStore::new(cas_root.clone());
    let snap_obj = cas
        .get_object(snap_hash)?
        .ok_or_else(|| anyhow::anyhow!("snapshot {} not found in CAS", snap_hash))?;
    let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&snap_obj)?;

    let manifest_hash = snapshot.project_manifest_hash.clone();
    let exec_dir = state.config.state_dir.join("executions").join(thread_id);
    let cache = crate::execution::cache::MaterializationCache::new(
        state.config.state_dir.join("cache").join("snapshots"),
    );
    crate::execution::checkout_project(&cas_root, &manifest_hash, &exec_dir, Some(&cache))?;

    guard.track_temp_dir(exec_dir.clone());
    Ok((exec_dir, Some(manifest_hash), Some(snap_hash.to_string())))
}

/// Post-execution: fold back outputs and advance HEAD.
fn post_execution_foldback(
    state: &AppState,
    _thread_id: &str,
    _acting_principal: &str,
    pre_manifest_hash: &Option<String>,
    base_snapshot_hash: &Option<String>,
    project_path: Option<&std::path::Path>,
    execution_dir: Option<&std::path::Path>,
    _completion: &ExecutionCompletion,
) {
    let Some(manifest_hash) = pre_manifest_hash else {
        return;
    };
    let cas_root = match state.state_store.cas_root() {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, "failed to get cas_root");
            return;
        }
    };
    let refs_root = match state.state_store.refs_root() {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, "failed to get refs_root");
            return;
        }
    };
    // Need a working dir for fold-back. If neither an exec checkout
    // nor a LocalPath project_path is available (resume of a non-
    // LocalPath thread), nothing to fold back into.
    let working_dir = match execution_dir.or(project_path) {
        Some(d) => d,
        None => {
            tracing::debug!(
                "skipping fold-back: no execution dir or project_path \
                 (resume of non-LocalPath project)"
            );
            return;
        }
    };

    // Acquire write barrier for CAS mutations (fold-back + head advance)
    let _permit = match state.write_barrier.try_acquire() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "cannot acquire CAS write permit for fold-back, skipping");
            return;
        }
    };

    // Fold back changes
    let output_manifest_hash =
        match crate::execution::fold_back_outputs(&cas_root, working_dir, manifest_hash) {
            Ok(hash) => hash,
            Err(err) => {
                tracing::warn!(error = %err, "fold-back failed");
                None
            }
        };

    // Advance HEAD if there were changes and we have a base snapshot
    // AND a LocalPath project (HEAD ref is keyed off the LocalPath).
    if let Some(ref new_manifest_hash) = output_manifest_hash {
        if let (Some(ref snap_hash), Some(pp)) = (base_snapshot_hash, project_path) {
            let project_str = pp.to_string_lossy();
            let project_hash = lillux::cas::sha256_hex(project_str.as_bytes());
            let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
            match crate::execution::advance_after_foldback(
                &cas_root,
                &refs_root,
                &signer,
                &project_hash,
                new_manifest_hash,
                snap_hash,
            ) {
                Ok(_new_snap) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "advance_after_foldback failed");
                }
            }
        } else {
            tracing::debug!(
                "skipping HEAD advance: missing base snapshot hash or LocalPath"
            );
        }
    }
}

/// Pin a LocalPath spawn's resume to a snapshot of the working dir
/// at spawn time.
///
/// **Why:** for `LocalPath` projects the runner does not pre-allocate
/// a `ProjectSnapshot` (only a `SourceManifest` is built by
/// `prepare_cas_context`). Without an `original_snapshot_hash` on the
/// captured `ResumeContext`, the reconciler would re-resolve the
/// resumed plan against the *current* working dir on restart — not
/// the version that was current when the checkpoint was written —
/// silently breaking the documented "Phase 6 pins resume to the
/// original project snapshot" promise. See
/// `docs/future/RESUME-ADVANCED-PATH.md`.
///
/// Returns the allocated snapshot hash on success, `None` if no
/// pinning was needed (no `native_resume`, or already pinned via a
/// caller-supplied snapshot).
fn pin_localpath_snapshot_if_needed(
    state: &AppState,
    launch_metadata: &mut crate::launch_metadata::RuntimeLaunchMetadata,
    pre_manifest_hash: &Option<String>,
    base_snapshot_hash: &Option<String>,
) -> Result<Option<String>> {
    if launch_metadata.native_resume.is_none() {
        return Ok(None);
    }
    if base_snapshot_hash.is_some() {
        return Ok(None);
    }
    let manifest_hash = match pre_manifest_hash {
        Some(h) => h.clone(),
        None => return Ok(None),
    };
    let cas_root = state.state_store.cas_root()?;
    let cas = lillux::cas::CasStore::new(cas_root);
    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_manifest_hash: manifest_hash,
        user_manifest_hash: None,
        parent_hashes: Vec::new(),
        created_at: lillux::time::iso8601_now(),
        source: "native_resume_pin".to_string(),
    };
    let snap_hash = cas.store_object(&snapshot.to_value())?;
    if let Some(ref mut ctx) = launch_metadata.resume_context {
        ctx.original_snapshot_hash = Some(snap_hash.clone());
    }
    Ok(Some(snap_hash))
}

/// Attach a freshly-spawned process to the daemon's runtime ledger.
/// On failure, hard-kill the spawned process group, finalize the
/// thread as failed, log, and return Err — so the live child never
/// outlives daemon ownership.
fn attach_or_kill(
    state: &AppState,
    thread_id: &str,
    spawned_pid: u32,
    spawned_pgid: i64,
    launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
    failed_outcome_code: &str,
) -> Result<()> {
    if let Err(err) = state.threads.attach_process(&ThreadAttachProcessParams {
        thread_id: thread_id.to_string(),
        pid: spawned_pid as i64,
        pgid: spawned_pgid,
        metadata: None,
        launch_metadata: launch_metadata.clone(),
    }) {
        tracing::error!(
            thread_id,
            pgid = spawned_pgid,
            error = %err,
            outcome_code = failed_outcome_code,
            "attach_process failed after spawn — killing child PG and finalizing thread"
        );
        let kill = crate::process::hard_kill_process_group(spawned_pgid);
        if !kill.success {
            tracing::warn!(
                thread_id,
                pgid = spawned_pgid,
                method = %kill.method,
                "hard_kill_process_group did not confirm success after attach failure"
            );
        }
        fail_thread_static(state, thread_id, failed_outcome_code);
        return Err(err);
    }
    Ok(())
}

/// Finalize a thread from its completion result.
fn finalize_completion(
    state: &AppState,
    thread_id: &str,
    completion: ExecutionCompletion,
) -> Result<ThreadDetail> {
    match state
        .threads
        .finalize_from_completion(thread_id, &completion)
    {
        Ok(thread) => Ok(thread),
        Err(err) => {
            tracing::error!(error = %err, "invalid completion during finalization");
            let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread_id.to_string(),
                status: "failed".to_string(),
                outcome_code: Some("invalid_completion".to_string()),
                result: None,
                error: Some(json!({ "code": "invalid_completion" })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            });
            Err(err)
        }
    }
}

/// Result of an inline execution.
pub struct InlineResult {
    pub finalized_thread: ThreadDetail,
    pub result: Value,
}

/// Result of a detached execution launch.
pub struct DetachedResult {
    pub running_thread: ThreadDetail,
}

/// Run an execution inline (blocking until completion).
///
/// Handles the full lifecycle: CAS context, snapshot, spawn,
/// fold-back, finalize, cleanup. On any error, the thread is finalized
/// as failed and all resources are cleaned up.
#[tracing::instrument(
    name = "thread:execute",
    skip(state, params),
    fields(
        thread_id = tracing::field::Empty,
        item_ref = %params.resolved.item_ref,
    )
)]
pub async fn run_inline(
    state: AppState,
    mut params: ExecutionParams,
) -> Result<InlineResult> {
    let mut guard = ExecutionGuard::new(state.clone());
    // Track pre-existing temp dir immediately so cleanup covers all exit paths
    if let Some(ref d) = params.temp_dir {
        guard.track_temp_dir(d.clone());
    }

    // Create and track thread
    let created = state
        .threads
        .create_root_thread(&params.resolved)
        .map_err(|e| { guard.cleanup(); e })?;
    let running = state
        .threads
        .mark_running(&created.thread_id)
        .map_err(|e| { guard.fail_thread("create_failed"); guard.cleanup(); e })?;
    guard.track_thread(&running.thread_id);
    tracing::Span::current().record("thread_id", &running.thread_id.as_str());

    // Prepare CAS context — if this fails, finalize thread as failed
    let (effective_path, pre_manifest_hash, mut base_snapshot_hash) =
        match prepare_cas_context(&state, params.project_path.as_deref(), &params.snapshot_hash, &params.temp_dir, &running.thread_id, &mut guard) {
            Ok(ctx) => ctx,
            Err(err) => {
                guard.fail_thread("cas_context_failed");
                guard.cleanup();
                return Err(err);
            }
        };

    // Update project context if CAS checkout changed the path
    if Some(&effective_path) != params.project_path.as_ref() {
        params.resolved.plan_context.project_context =
            ryeos_engine::contracts::ProjectContext::LocalPath { path: effective_path.clone() };
    }

    // Spawn
    let engine = state.engine.clone();
    let tid = running.thread_id.clone();
    let crid = running.chain_root_id.clone();
    let resolved = params.resolved.clone();
    let vault = params.vault_bindings.clone();
    // Daemon-owned per-thread state dir under config.state_dir — does
    // NOT live under the (ephemeral, CAS-checkout) working directory,
    // so checkpoints survive working-dir cleanup and daemon restart.
    // See `launch_metadata::daemon_thread_state_dir`.
    let state_dir = crate::launch_metadata::daemon_thread_state_dir(
        &state.config.state_dir,
        &tid,
    );
    let inline_snapshot = base_snapshot_hash.clone();
    let mut spawned = match task::spawn_blocking(move || {
        thread_lifecycle::spawn_item(
            &engine,
            &resolved,
            &tid,
            &crid,
            vault,
            Some(state_dir.as_path()),
            false,
            inline_snapshot.as_deref(),
        )
    })
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            tracing::error!(error = %err, "engine error during inline spawn");
            guard.fail_thread("engine_error");
            guard.cleanup();
            return Err(err);
        }
        Err(join_err) => {
            tracing::error!(error = %join_err, "task panic during inline spawn");
            guard.fail_thread("task_panic");
            guard.cleanup();
            return Err(anyhow::anyhow!("spawn task panic: {join_err}"));
        }
    };

    // Pin LocalPath native_resume to a snapshot before attach.
    match pin_localpath_snapshot_if_needed(
        &state,
        &mut spawned.launch_metadata,
        &pre_manifest_hash,
        &base_snapshot_hash,
    ) {
        Ok(Some(snap)) => {
            base_snapshot_hash = Some(snap);
        }
        Ok(None) => {}
        Err(err) => {
            tracing::error!(error = %err, "failed to pin LocalPath native_resume snapshot");
            // Kill the live child since attach is about to fail anyway.
            let _ = crate::process::hard_kill_process_group(spawned.pgid);
            guard.fail_thread("snapshot_pin_failed");
            guard.cleanup();
            return Err(err);
        }
    }

    // Attach process — on failure, kill the child + finalize.
    if let Err(err) = attach_or_kill(
        &state,
        &running.thread_id,
        spawned.pid,
        spawned.pgid,
        &spawned.launch_metadata,
        "attach_failed",
    ) {
        guard.mark_finalized();
        guard.cleanup();
        return Err(err);
    }

    // Wait
    let completion = match task::spawn_blocking(move || spawned.wait()).await {
        Ok(c) => c,
        Err(join_err) => {
            tracing::error!(error = %join_err, "task panic during inline wait");
            guard.fail_thread("task_panic");
            guard.cleanup();
            return Err(anyhow::anyhow!("wait task panic: {join_err}"));
        }
    };

    // Post-execution fold-back
    post_execution_foldback(
        &state,
        &running.thread_id,
        &params.acting_principal,
        &pre_manifest_hash,
        &base_snapshot_hash,
        params.project_path.as_deref(),
        guard.temp_dir.as_deref(),
        &completion,
    );

    // Finalize
    let finalized = match finalize_completion(&state, &running.thread_id, completion) {
        Ok(t) => {
            guard.mark_finalized();
            t
        }
        Err(err) => {
            guard.mark_finalized(); // finalize_completion already finalized as failed
            guard.cleanup();
            return Err(err);
        }
    };

    let result = state
        .threads
        .build_execute_result(&finalized.thread_id)?;

    guard.cleanup();

    Ok(InlineResult {
        finalized_thread: finalized,
        result: serde_json::to_value(&result).unwrap_or(json!(null)),
    })
}

/// Launch a detached execution (returns immediately, runs in background).
///
/// Handles the full lifecycle in a background tokio task with the same
/// guarantees as inline: CAS context, snapshot, spawn, fold-back,
/// finalize, cleanup.
#[tracing::instrument(
    name = "thread:execute",
    skip(state, params),
    fields(
        thread_id = tracing::field::Empty,
        item_ref = %params.resolved.item_ref,
    )
)]
pub async fn run_detached(
    state: AppState,
    mut params: ExecutionParams,
) -> Result<DetachedResult> {
    let mut guard = ExecutionGuard::new(state.clone());
    // Track pre-existing temp dir immediately so cleanup covers all exit paths
    if let Some(ref d) = params.temp_dir {
        guard.track_temp_dir(d.clone());
    }

    // Create and track thread
    let created = state
        .threads
        .create_root_thread(&params.resolved)
        .map_err(|e| { guard.cleanup(); e })?;
    let running = state
        .threads
        .mark_running(&created.thread_id)
        .map_err(|e| { guard.fail_thread("create_failed"); guard.cleanup(); e })?;
    guard.track_thread(&running.thread_id);
    tracing::Span::current().record("thread_id", &running.thread_id.as_str());

    // Prepare CAS context
    let (effective_path, pre_manifest_hash, base_snapshot_hash) =
        match prepare_cas_context(&state, params.project_path.as_deref(), &params.snapshot_hash, &params.temp_dir, &running.thread_id, &mut guard) {
            Ok(ctx) => ctx,
            Err(err) => {
                guard.fail_thread("cas_context_failed");
                guard.cleanup();
                return Err(err);
            }
        };

    // Update project context if CAS checkout changed the path
    if Some(&effective_path) != params.project_path.as_ref() {
        params.resolved.plan_context.project_context =
            ryeos_engine::contracts::ProjectContext::LocalPath { path: effective_path.clone() };
    }

    // Capture thread details before moving guard
    let running_thread_id = running.thread_id.clone();

    // Move guard parts into the background task
    let (bg_state, _bg_thread_id, bg_temp_dir) = guard.into_detached_parts();
    let bg_thread_id = running.thread_id.clone();
    let bg_chain_root_id = running.chain_root_id.clone();
    let bg_resolved = params.resolved.clone();
    let bg_engine = state.engine.clone();
    let bg_vault = params.vault_bindings.clone();
    let bg_acting_principal = params.acting_principal.clone();
    let bg_pre_manifest_hash = pre_manifest_hash;
    let bg_base_snapshot_hash = base_snapshot_hash;
    let bg_project_path = params.project_path.clone();
    let bg_state_root = state.config.state_dir.clone();

    tokio::spawn(dispatch_detached_bg_task(
        bg_state,
        bg_thread_id,
        bg_chain_root_id,
        bg_resolved,
        bg_engine,
        bg_vault,
        bg_acting_principal,
        bg_pre_manifest_hash,
        bg_base_snapshot_hash,
        bg_project_path,
        bg_temp_dir,
        bg_state_root,
        false, // is_resume
        None,  // prior_status_for_mark_running
    ));

    // Re-fetch the thread detail (the original was consumed by the background task setup)
    let running_detail = state
        .threads
        .get_thread(&running_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("thread {running_thread_id} not found after spawn"))?;

    Ok(DetachedResult {
        running_thread: running_detail,
    })
}

/// Shared background-task body for detached spawns.
///
/// Used by both `run_detached` (fresh thread row) and
/// `run_existing_detached` (reconciler resume). Centralizes the
/// spawn → pin → attach → wait → fold-back → finalize → cleanup flow
/// so both paths stay in lock-step on the fail-closed contract:
/// any failure between spawn and attach kills the live PG and
/// finalizes the thread; finalize errors are logged loudly.
///
/// `prior_status_for_mark_running` is `Some("created")` only on the
/// resume path when the persisted thread row has not yet been
/// transitioned out of `created`. The dispatcher calls
/// `mark_running` after attach so `drain_running_threads` (which
/// only queries `["running"]`) can reach the live child on shutdown.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "thread:dispatch",
    skip(
        bg_state, bg_chain_root_id, bg_resolved, bg_engine, bg_vault,
        bg_acting_principal, bg_pre_manifest_hash, bg_base_snapshot_hash,
        bg_project_path, bg_temp_dir, bg_state_root, prior_status_for_mark_running
    ),
    fields(
        thread_id = %bg_thread_id,
        item_ref = %bg_resolved.item_ref,
        is_resume,
        prior_status = tracing::field::Empty,
    )
)]
async fn dispatch_detached_bg_task(
    bg_state: AppState,
    bg_thread_id: String,
    bg_chain_root_id: String,
    bg_resolved: ResolvedExecutionRequest,
    bg_engine: std::sync::Arc<ryeos_engine::engine::Engine>,
    bg_vault: HashMap<String, String>,
    bg_acting_principal: String,
    bg_pre_manifest_hash: Option<String>,
    mut bg_base_snapshot_hash: Option<String>,
    bg_project_path: Option<PathBuf>,
    bg_temp_dir: Option<PathBuf>,
    bg_state_root: PathBuf,
    is_resume: bool,
    prior_status_for_mark_running: Option<String>,
) {
    if let Some(ref s) = prior_status_for_mark_running {
        tracing::Span::current().record("prior_status", &s.as_str());
    }
    let log_phase = if is_resume { "resume" } else { "detached" };
    let attach_outcome_code = if is_resume {
        "resume_attach_failed"
    } else {
        "attach_failed"
    };

    let state_dir =
        crate::launch_metadata::daemon_thread_state_dir(&bg_state_root, &bg_thread_id);

    let tid_for_spawn = bg_thread_id.clone();
    let crid_for_spawn = bg_chain_root_id.clone();
    let res_for_spawn = bg_resolved.clone();
    let eng_for_spawn = bg_engine.clone();
    let vault_for_spawn = bg_vault;
    let snap_for_spawn = bg_base_snapshot_hash.clone();

    let spawn_result = task::spawn_blocking(move || {
        thread_lifecycle::spawn_item(
            &eng_for_spawn,
            &res_for_spawn,
            &tid_for_spawn,
            &crid_for_spawn,
            vault_for_spawn,
            Some(state_dir.as_path()),
            is_resume,
            snap_for_spawn.as_deref(),
        )
    })
    .await;

    let mut spawned = match spawn_result {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            tracing::error!(
                phase = log_phase,
                error = %err,
                "engine error during spawn"
            );
            fail_thread_static(&bg_state, &bg_thread_id, "engine_error");
            cleanup_temp_dir(bg_temp_dir.as_deref());
            return;
        }
        Err(join_err) => {
            tracing::error!(
                phase = log_phase,
                error = %join_err,
                "task panic during spawn"
            );
            fail_thread_static(&bg_state, &bg_thread_id, "task_panic");
            cleanup_temp_dir(bg_temp_dir.as_deref());
            return;
        }
    };

    // Pin LocalPath native_resume to a snapshot before attach.
    match pin_localpath_snapshot_if_needed(
        &bg_state,
        &mut spawned.launch_metadata,
        &bg_pre_manifest_hash,
        &bg_base_snapshot_hash,
    ) {
        Ok(Some(snap)) => {
            bg_base_snapshot_hash = Some(snap);
        }
        Ok(None) => {}
        Err(err) => {
            tracing::error!(
                phase = log_phase,
                error = %err,
                "failed to pin LocalPath native_resume snapshot"
            );
            let _ = crate::process::hard_kill_process_group(spawned.pgid);
            fail_thread_static(&bg_state, &bg_thread_id, "snapshot_pin_failed");
            cleanup_temp_dir(bg_temp_dir.as_deref());
            return;
        }
    }

    // Attach process — on failure, kill the child + finalize.
    if let Err(err) = attach_or_kill(
        &bg_state,
        &bg_thread_id,
        spawned.pid,
        spawned.pgid,
        &spawned.launch_metadata,
        attach_outcome_code,
    ) {
        tracing::error!(
            phase = log_phase,
            thread_id = %bg_thread_id,
            error = %err,
            "{}: child killed and thread finalized",
            attach_outcome_code,
        );
        cleanup_temp_dir(bg_temp_dir.as_deref());
        return;
    }

    // Resume of a `created` row: transition to `running` so
    // `drain_running_threads` sees it on shutdown.
    if matches!(prior_status_for_mark_running.as_deref(), Some("created")) {
        if let Err(err) = bg_state.threads.mark_running(&bg_thread_id) {
            tracing::warn!(
                phase = log_phase,
                thread_id = %bg_thread_id,
                error = %err,
                "failed to transition created thread to running on resume"
            );
        }
    }

    let wait_result = task::spawn_blocking(move || spawned.wait()).await;
    match wait_result {
        Ok(completion) => {
            post_execution_foldback(
                &bg_state,
                &bg_thread_id,
                &bg_acting_principal,
                &bg_pre_manifest_hash,
                &bg_base_snapshot_hash,
                bg_project_path.as_deref(),
                bg_temp_dir.as_deref(),
                &completion,
            );
            if let Err(err) = finalize_completion(&bg_state, &bg_thread_id, completion) {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %err,
                    "finalize_completion failed; thread already finalized as failed by inner finalizer"
                );
            }
        }
        Err(join_err) => {
            tracing::error!(
                phase = log_phase,
                error = %join_err,
                "task panic during wait"
            );
            fail_thread_static(&bg_state, &bg_thread_id, "task_panic");
        }
    }

    cleanup_temp_dir(bg_temp_dir.as_deref());
}

fn cleanup_temp_dir(temp_dir: Option<&std::path::Path>) {
    if let Some(d) = temp_dir {
        if d.exists() {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// Fail a thread without a guard (for use inside detached tasks).
fn fail_thread_static(state: &AppState, thread_id: &str, outcome_code: &str) {
    let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: "failed".to_string(),
        outcome_code: Some(outcome_code.to_string()),
        result: None,
        error: Some(json!({ "code": outcome_code })),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    });
}

/// Build `ExecutionParams` from a captured `ResumeContext`.
///
/// Re-runs the engine resolver against the **original**
/// `ProjectContext`. When `original_snapshot_hash` is set, the
/// resolver runs against the pinned snapshot (via
/// `ProjectContext::SnapshotHash`), so the resumed plan matches the
/// project version captured at the original spawn — not the current
/// working-dir head. See `docs/future/RESUME-ADVANCED-PATH.md`.
#[tracing::instrument(
    name = "thread:resume_params",
    skip(state, resume),
    fields(
        item_ref = %resume.item_ref,
        kind = %resume.kind,
        snapshot_pinned = resume.original_snapshot_hash.is_some(),
    )
)]
pub fn execution_params_from_resume_context(
    state: &AppState,
    resume: &ResumeContext,
) -> Result<ExecutionParams> {
    // Pin resolution to the original snapshot when one was captured.
    let project_context = match resume.original_snapshot_hash.as_deref() {
        Some(hash) => ProjectContext::SnapshotHash {
            hash: hash.to_string(),
        },
        None => resume.project_context.clone(),
    };

    let plan_ctx = PlanContext {
        requested_by: resume.requested_by.clone(),
        project_context: project_context.clone(),
        current_site_id: resume.current_site_id.clone(),
        origin_site_id: resume.origin_site_id.clone(),
        execution_hints: resume.execution_hints.clone(),
        validate_only: false,
    };

    let canonical = CanonicalRef::parse(&resume.item_ref)
        .map_err(|e| anyhow::anyhow!("resume: invalid item ref {}: {e}", resume.item_ref))?;

    let resolved_item = state
        .engine
        .resolve(&plan_ctx, &canonical)
        .map_err(|e| anyhow::anyhow!("resume: resolve failed: {e}"))?;

    let executor_ref = resolved_item
        .metadata
        .executor_id
        .clone()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "resume: item {} does not declare an executor_id",
                resume.item_ref
            )
        })?;

    let resolved = ResolvedExecutionRequest {
        kind: resume.kind.clone(),
        item_ref: resume.item_ref.clone(),
        executor_ref,
        launch_mode: resume.launch_mode.clone(),
        current_site_id: resume.current_site_id.clone(),
        origin_site_id: resume.origin_site_id.clone(),
        target_site_id: None,
        requested_by: resume.requested_by_name(),
        parameters: resume.parameters.clone(),
        resolved_item,
        plan_context: plan_ctx,
    };

    // Project path is only meaningful when the original spawn was a
    // `LocalPath`. For non-`LocalPath` resumes the snapshot pin
    // carries project identity and there is no working dir to
    // reference; HEAD fold-back will be skipped (see
    // `post_execution_foldback`).
    let project_path = match &resume.project_context {
        ProjectContext::LocalPath { path } => Some(path.clone()),
        _ => None,
    };

    Ok(ExecutionParams {
        resolved,
        acting_principal: resume
            .requested_by_name()
            .unwrap_or_else(|| "fp:resume".to_string()),
        project_path,
        vault_bindings: HashMap::new(),
        snapshot_hash: resume.original_snapshot_hash.clone(),
        parameters: resume.parameters.clone(),
        temp_dir: None,
    })
}

/// Re-spawn an existing thread under its original `thread_id` after a
/// daemon restart. Used by the reconciler's auto-resume path.
///
/// Mirrors `run_detached` from spawn onward but does NOT call
/// `create_root_thread` — the thread row already exists from the
/// pre-crash spawn.
///
/// **Bounded-duplicates note:** if the daemon crashes after the
/// subprocess writes a checkpoint but before the next checkpoint
/// flushes, work between the last checkpoint and crash will replay.
/// Resume promises *bounded* duplicates after crash, NOT exactly-once
/// semantics. See `docs/future/RESUME-ADVANCED-PATH.md`
/// (Evolution 2 — supervisor side-car) for the trade-off discussion.
#[tracing::instrument(
    name = "thread:resume",
    skip(state, params),
    fields(
        thread_id = %thread_id,
        chain_root_id = %chain_root_id,
        item_ref = %params.resolved.item_ref,
        prior_status = %prior_status,
    )
)]
pub async fn run_existing_detached(
    state: AppState,
    thread_id: String,
    chain_root_id: String,
    mut params: ExecutionParams,
    prior_status: String,
) -> Result<()> {
    let mut guard = ExecutionGuard::new(state.clone());
    guard.track_thread(&thread_id);
    if let Some(ref d) = params.temp_dir {
        guard.track_temp_dir(d.clone());
    }

    // Prepare CAS context.
    let (effective_path, pre_manifest_hash, base_snapshot_hash) = match prepare_cas_context(
        &state,
        params.project_path.as_deref(),
        &params.snapshot_hash,
        &params.temp_dir,
        &thread_id,
        &mut guard,
    ) {
        Ok(ctx) => ctx,
        Err(err) => {
            guard.fail_thread("cas_context_failed");
            guard.cleanup();
            return Err(err);
        }
    };

    // Update plan_context to point at the materialized path so the
    // engine resolves item refs from there.
    if Some(&effective_path) != params.project_path.as_ref() {
        params.resolved.plan_context.project_context = ProjectContext::LocalPath {
            path: effective_path.clone(),
        };
    }

    let (bg_state, _bg_thread_id, bg_temp_dir) = guard.into_detached_parts();
    let bg_thread_id = thread_id.clone();
    let bg_chain_root_id = chain_root_id.clone();
    let bg_resolved = params.resolved.clone();
    let bg_engine = state.engine.clone();
    let bg_vault = params.vault_bindings.clone();
    let bg_acting_principal = params.acting_principal.clone();
    let bg_pre_manifest_hash = pre_manifest_hash;
    let bg_base_snapshot_hash = base_snapshot_hash;
    let bg_project_path = params.project_path.clone();
    let bg_state_root = state.config.state_dir.clone();

    tokio::spawn(dispatch_detached_bg_task(
        bg_state,
        bg_thread_id,
        bg_chain_root_id,
        bg_resolved,
        bg_engine,
        bg_vault,
        bg_acting_principal,
        bg_pre_manifest_hash,
        bg_base_snapshot_hash,
        bg_project_path,
        bg_temp_dir,
        bg_state_root,
        true, // is_resume
        Some(prior_status),
    ));

    Ok(())
}
