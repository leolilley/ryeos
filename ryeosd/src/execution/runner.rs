//! Unified execution runner.
//!
//! Both `/execute` and inbound webhooks use this to run the full
//! execution lifecycle: epoch → CAS context → snapshot → spawn →
//! fold-back → finalize → cleanup.
//!
//! The `ExecutionGuard` struct tracks all state and ensures cleanup
//! on every exit path (epoch completion, temp dir removal, thread
//! finalization on pre-spawn failures).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::task;

use rye_engine::contracts::ExecutionCompletion;

use crate::db::ThreadDetail;
use crate::execution::snapshot::{ExecutionSnapshot, RuntimeOutputsBundle};
use crate::services::thread_lifecycle::{
    self, ResolvedExecutionRequest, ThreadAttachProcessParams, ThreadFinalizeParams,
};
use crate::state::AppState;

/// All inputs needed to run an execution.
pub struct ExecutionParams {
    pub resolved: ResolvedExecutionRequest,
    pub acting_principal: String,
    pub project_path: PathBuf,
    pub vault_bindings: HashMap<String, String>,
    pub snapshot_hash: Option<String>,
    pub item_ref: String,
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
    epoch_id: Option<String>,
    temp_dir: Option<PathBuf>,
    thread_finalized: bool,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl ExecutionGuard {
    fn new(state: AppState) -> Self {
        Self {
            state,
            thread_id: None,
            epoch_id: None,
            temp_dir: None,
            thread_finalized: false,
            heartbeat: None,
        }
    }

    /// Register a GC writer epoch.
    fn register_epoch(&mut self) {
        self.epoch_id = crate::gc::epochs::register_epoch(
            &self.state.config.cas_root,
            self.state.identity.fingerprint(),
            Vec::new(),
        )
        .ok();
    }

    /// Add root hashes to the registered epoch so sweep marks them reachable.
    fn update_epoch_roots(&self, hashes: Vec<String>) {
        if let Some(ref eid) = self.epoch_id {
            let _ = crate::gc::epochs::update_epoch_roots(
                &self.state.config.cas_root,
                eid,
                hashes,
            );
        }
    }

    /// Start a periodic heartbeat to keep the epoch alive during long-running executions.
    fn start_epoch_heartbeat(&mut self) {
        if let Some(ref epoch_id) = self.epoch_id {
            let cas_root = self.state.config.cas_root.clone();
            let eid = epoch_id.clone();
            self.heartbeat = Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
                loop {
                    interval.tick().await;
                    let _ = crate::gc::epochs::touch_epoch(&cas_root, &eid);
                }
            }));
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

    /// Perform all cleanup: abort heartbeat, complete epoch, remove temp dir.
    fn cleanup(&mut self) {
        if let Some(h) = self.heartbeat.take() {
            h.abort();
        }
        if let Some(ref eid) = self.epoch_id {
            let _ = crate::gc::epochs::complete_epoch(&self.state.config.cas_root, eid);
        }
        if let Some(ref d) = self.temp_dir {
            if d.exists() {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }

    /// Consume into parts for moving into tokio::spawn.
    fn into_detached_parts(
        mut self,
    ) -> (AppState, Option<String>, Option<String>, Option<PathBuf>, Option<tokio::task::JoinHandle<()>>) {
        let state = self.state.clone();
        let thread_id = self.thread_id.take();
        let epoch_id = self.epoch_id.take();
        let temp_dir = self.temp_dir.take();
        let heartbeat = self.heartbeat.take();
        (state, thread_id, epoch_id, temp_dir, heartbeat)
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
    project_path: &std::path::Path,
    snapshot_hash: &Option<String>,
    pre_checkout_dir: &Option<PathBuf>,
    thread_id: &str,
    guard: &mut ExecutionGuard,
) -> Result<(PathBuf, Option<String>, Option<String>)> {
    // If we have a pre-checked-out dir from ResolvedProjectContext,
    // reuse it instead of doing a second checkout.
    if let (Some(snap_hash), Some(checkout_dir)) = (snapshot_hash, pre_checkout_dir) {
        let cas = state.cas_store();
        let snap_obj = cas
            .get_object(snap_hash)?
            .ok_or_else(|| anyhow::anyhow!("snapshot {} not found in CAS", snap_hash))?;
        let snapshot = crate::cas::ProjectSnapshot::from_json(&snap_obj)?;
        let manifest_hash = snapshot.project_manifest_hash.clone();
        guard.track_temp_dir(checkout_dir.clone());
        return Ok((checkout_dir.clone(), Some(manifest_hash), Some(snap_hash.clone())));
    }

    if let Some(snap_hash) = snapshot_hash {
        return checkout_from_snapshot(state, snap_hash, thread_id, guard);
    }

    // Live FS — ingest working dir, no checkout
    let cas = state.cas_store();
    let items = crate::cas::ingest_directory(cas, project_path)?;
    let manifest = crate::cas::SourceManifest { item_source_hashes: items };
    let manifest_hash = cas.store_object(&manifest.to_json())?;

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
    let cas = state.cas_store();
    let snap_obj = cas
        .get_object(snap_hash)?
        .ok_or_else(|| anyhow::anyhow!("snapshot {} not found in CAS", snap_hash))?;
    let snapshot = crate::cas::ProjectSnapshot::from_json(&snap_obj)?;

    let manifest_hash = snapshot.project_manifest_hash.clone();
    let exec_dir = state.config.state_dir.join("executions").join(thread_id);
    let cache = crate::execution::cache::MaterializationCache::new(
        state.config.state_dir.join("cache").join("snapshots"),
    );
    crate::execution::checkout_project(cas, &manifest_hash, &exec_dir, Some(&cache))?;

    guard.track_temp_dir(exec_dir.clone());
    Ok((exec_dir, Some(manifest_hash), Some(snap_hash.to_string())))
}

/// Store an ExecutionSnapshot in CAS and write a generic ref for the thread.
fn store_execution_snapshot(
    state: &AppState,
    thread_id: &str,
    item_ref: &str,
    pre_manifest_hash: &Option<String>,
    parameters: &Value,
) {
    let Some(manifest_hash) = pre_manifest_hash else {
        return;
    };
    let cas = state.cas_store();
    let snapshot = ExecutionSnapshot {
        thread_id: thread_id.to_string(),
        project_manifest_hash: manifest_hash.clone(),
        user_manifest_hash: None,
        item_ref: item_ref.to_string(),
        parameters: Some(parameters.clone()),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Ok(hash) = snapshot.store(cas) {
        let _ = state
            .refs_store()
            .write_ref(&format!("threads/{thread_id}/execution_snapshot"), &hash);
    }
}

/// Post-execution: fold back outputs, store RuntimeOutputsBundle, advance HEAD.
fn post_execution_foldback(
    state: &AppState,
    thread_id: &str,
    acting_principal: &str,
    pre_manifest_hash: &Option<String>,
    base_snapshot_hash: &Option<String>,
    project_path: &std::path::Path,
    execution_dir: Option<&std::path::Path>,
    completion: &ExecutionCompletion,
) {
    let Some(manifest_hash) = pre_manifest_hash else {
        return;
    };
    let cas = state.cas_store();
    let working_dir = execution_dir.unwrap_or(project_path);

    // Fold back changes
    let output_manifest_hash =
        match crate::execution::fold_back_outputs(cas, working_dir, manifest_hash) {
            Ok(hash) => hash,
            Err(err) => {
                tracing::warn!(error = %err, "fold-back failed");
                None
            }
        };

    // Advance HEAD if there were changes and we have a base snapshot
    if let Some(ref new_manifest_hash) = output_manifest_hash {
        if let Some(ref snap_hash) = base_snapshot_hash {
            let project_str = project_path.to_string_lossy();
            match crate::execution::advance_after_foldback(
                cas,
                state.refs_store(),
                acting_principal,
                &project_str,
                new_manifest_hash,
                snap_hash,
            ) {
                Ok(_new_snap) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "advance_after_foldback failed");
                }
            }
        } else {
            tracing::debug!("skipping HEAD advance: no base snapshot hash");
        }
    }

    // Get the execution_snapshot_hash for the bundle
    let exec_snap_hash = state
        .refs_store()
        .read_ref(&format!("threads/{thread_id}/execution_snapshot"))
        .ok()
        .flatten()
        .unwrap_or_default();

    // Store RuntimeOutputsBundle
    let status_str = format!("{:?}", completion.status).to_lowercase();
    let bundle = RuntimeOutputsBundle {
        thread_id: thread_id.to_string(),
        execution_snapshot_hash: exec_snap_hash,
        output_manifest_hash,
        artifacts: Vec::new(),
        status: status_str,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Ok(hash) = bundle.store(cas) {
        let _ = state
            .refs_store()
            .write_ref(&format!("threads/{thread_id}/runtime_outputs"), &hash);
    }
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
/// Handles the full lifecycle: epoch, CAS context, snapshot, spawn,
/// fold-back, finalize, cleanup. On any error, the thread is finalized
/// as failed and all resources are cleaned up.
pub async fn run_inline(
    state: AppState,
    mut params: ExecutionParams,
) -> Result<InlineResult> {
    let mut guard = ExecutionGuard::new(state.clone());
    // Track pre-existing temp dir immediately so cleanup covers all exit paths
    if let Some(ref d) = params.temp_dir {
        guard.track_temp_dir(d.clone());
    }
    guard.register_epoch();

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

    // Prepare CAS context — if this fails, finalize thread as failed
    let (effective_path, pre_manifest_hash, base_snapshot_hash) =
        match prepare_cas_context(&state, &params.project_path, &params.snapshot_hash, &params.temp_dir, &running.thread_id, &mut guard) {
            Ok(ctx) => ctx,
            Err(err) => {
                guard.fail_thread("cas_context_failed");
                guard.cleanup();
                return Err(err);
            }
        };

    // Store ExecutionSnapshot
    store_execution_snapshot(
        &state,
        &running.thread_id,
        &params.item_ref,
        &pre_manifest_hash,
        &params.parameters,
    );

    // Register known hashes as epoch roots so GC sweep protects them
    {
        let mut epoch_roots = Vec::new();
        if let Some(ref h) = pre_manifest_hash {
            epoch_roots.push(h.clone());
        }
        if let Some(ref h) = base_snapshot_hash {
            epoch_roots.push(h.clone());
        }
        if let Some(ref h) = state
            .refs_store()
            .read_ref(&format!("threads/{}/execution_snapshot", running.thread_id))
            .ok()
            .flatten()
        {
            epoch_roots.push(h.clone());
        }
        guard.update_epoch_roots(epoch_roots);
    }

    guard.start_epoch_heartbeat();

    // Update project context if CAS checkout changed the path
    if effective_path != params.project_path {
        params.resolved.plan_context.project_context =
            rye_engine::contracts::ProjectContext::LocalPath { path: effective_path.clone() };
    }

    // Spawn
    let engine = state.engine.clone();
    let tid = running.thread_id.clone();
    let crid = running.chain_root_id.clone();
    let resolved = params.resolved.clone();
    let vault = params.vault_bindings.clone();
    let spawned = match task::spawn_blocking(move || {
        thread_lifecycle::spawn_item(&engine, &resolved, &tid, &crid, vault)
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

    // Attach process
    let _ = state.threads.attach_process(&ThreadAttachProcessParams {
        thread_id: running.thread_id.clone(),
        pid: spawned.pid as i64,
        pgid: spawned.pgid,
        metadata: None,
    });

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
        &params.project_path,
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
/// guarantees as inline: epoch, CAS context, snapshot, spawn, fold-back,
/// finalize, cleanup.
pub async fn run_detached(
    state: AppState,
    mut params: ExecutionParams,
) -> Result<DetachedResult> {
    let mut guard = ExecutionGuard::new(state.clone());
    // Track pre-existing temp dir immediately so cleanup covers all exit paths
    if let Some(ref d) = params.temp_dir {
        guard.track_temp_dir(d.clone());
    }
    guard.register_epoch();

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

    // Prepare CAS context
    let (effective_path, pre_manifest_hash, base_snapshot_hash) =
        match prepare_cas_context(&state, &params.project_path, &params.snapshot_hash, &params.temp_dir, &running.thread_id, &mut guard) {
            Ok(ctx) => ctx,
            Err(err) => {
                guard.fail_thread("cas_context_failed");
                guard.cleanup();
                return Err(err);
            }
        };

    // Store ExecutionSnapshot
    store_execution_snapshot(
        &state,
        &running.thread_id,
        &params.item_ref,
        &pre_manifest_hash,
        &params.parameters,
    );

    // Register known hashes as epoch roots so GC sweep protects them
    {
        let mut epoch_roots = Vec::new();
        if let Some(ref h) = pre_manifest_hash {
            epoch_roots.push(h.clone());
        }
        if let Some(ref h) = base_snapshot_hash {
            epoch_roots.push(h.clone());
        }
        if let Some(ref h) = state
            .refs_store()
            .read_ref(&format!("threads/{}/execution_snapshot", running.thread_id))
            .ok()
            .flatten()
        {
            epoch_roots.push(h.clone());
        }
        guard.update_epoch_roots(epoch_roots);
    }

    guard.start_epoch_heartbeat();

    // Update project context if CAS checkout changed the path
    if effective_path != params.project_path {
        params.resolved.plan_context.project_context =
            rye_engine::contracts::ProjectContext::LocalPath { path: effective_path.clone() };
    }

    // Capture thread details before moving guard
    let running_thread_id = running.thread_id.clone();

    // Move guard parts into the background task
    let (bg_state, _bg_thread_id, bg_epoch_id, bg_temp_dir, bg_heartbeat) = guard.into_detached_parts();
    let bg_thread_id = running.thread_id.clone();
    let bg_chain_root_id = running.chain_root_id.clone();
    let bg_resolved = params.resolved.clone();
    let bg_engine = state.engine.clone();
    let bg_vault = params.vault_bindings.clone();
    let bg_acting_principal = params.acting_principal.clone();
    let bg_pre_manifest_hash = pre_manifest_hash;
    let bg_base_snapshot_hash = base_snapshot_hash;
    let bg_project_path = params.project_path.clone();

    tokio::spawn(async move {
        let tid = bg_thread_id.clone();
        let crid = bg_chain_root_id.clone();
        let res = bg_resolved.clone();
        let eng = bg_engine.clone();
        let vault = bg_vault;

        let spawn_result = task::spawn_blocking(move || {
            thread_lifecycle::spawn_item(&eng, &res, &tid, &crid, vault)
        })
        .await;

        match spawn_result {
            Ok(Ok(spawned)) => {
                let _ = bg_state.threads.attach_process(&ThreadAttachProcessParams {
                    thread_id: bg_thread_id.clone(),
                    pid: spawned.pid as i64,
                    pgid: spawned.pgid,
                    metadata: None,
                });

                let wait_result = task::spawn_blocking(move || spawned.wait()).await;
                match wait_result {
                    Ok(completion) => {
                        post_execution_foldback(
                            &bg_state,
                            &bg_thread_id,
                            &bg_acting_principal,
                            &bg_pre_manifest_hash,
                            &bg_base_snapshot_hash,
                            &bg_project_path,
                            bg_temp_dir.as_deref(),
                            &completion,
                        );
                        let _ = finalize_completion(&bg_state, &bg_thread_id, completion);
                    }
                    Err(join_err) => {
                        tracing::error!(error = %join_err, "task panic during detached wait");
                        fail_thread_static(&bg_state, &bg_thread_id, "task_panic");
                    }
                }
            }
            Ok(Err(err)) => {
                tracing::error!(error = %err, "engine error during detached spawn");
                fail_thread_static(&bg_state, &bg_thread_id, "engine_error");
            }
            Err(join_err) => {
                tracing::error!(error = %join_err, "task panic during detached spawn");
                fail_thread_static(&bg_state, &bg_thread_id, "task_panic");
            }
        }

        // Always cleanup in detached path
        if let Some(h) = bg_heartbeat {
            h.abort();
        }
        if let Some(ref eid) = bg_epoch_id {
            let _ = crate::gc::epochs::complete_epoch(&bg_state.config.cas_root, eid);
        }
        if let Some(ref d) = bg_temp_dir {
            if d.exists() {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    });

    // Re-fetch the thread detail (the original was consumed by the background task setup)
    let running_detail = state
        .threads
        .get_thread(&running_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("thread {running_thread_id} not found after spawn"))?;

    Ok(DetachedResult {
        running_thread: running_detail,
    })
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
