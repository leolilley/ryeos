//! Unified execution runner.
//!
//! Both `/execute` and inbound webhooks use this to run the full
//! execution lifecycle: CAS context → snapshot → spawn →
//! fold-back → finalize → cleanup.
//!
//! The `ExecutionGuard` struct tracks transient state (temp dir,
//! thread row, callback + thread-auth tokens) and exposes
//! `cleanup()` / `fail_thread()` for callers to invoke on their
//! return paths. It is **not** a Drop guard — synchronous panics
//! between resource acquisition and the explicit cleanup call will
//! leak. The detached background path additionally installs
//! `CbTokenGuard` and `TatTokenGuard` (real Drop guards) inside the
//! spawned task so background-task panic, error, and success exits
//! all revoke the per-thread tokens.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::task;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{ExecutionCompletion, PlanContext, ProjectContext};
use ryeos_engine::protocol_vocabulary::{produce_env_value, EnvInjectionSource};
use ryeos_engine::subprocess_spec::SubprocessBuildRequest;

use ryeos_app::callback_token::launch_token_ttl;
use ryeos_app::callback_token::effective_bundle_id_for_request;
use ryeos_app::execution_provenance::ExecutionProvenance;
use ryeos_app::launch_metadata::ResumeContext;
use ryeos_app::state::AppState;
use ryeos_app::state_store::ThreadDetail;
use ryeos_app::temp_dir_guard::TempDirGuard;
use ryeos_app::thread_lifecycle::{
    self, ResolvedExecutionRequest, ThreadAttachProcessParams, ThreadFinalizeParams,
};

// ── Resume-specific error type ────────────────────────────────────

/// Typed error for the resume path (`run_existing_detached`).
///
/// Each variant maps to a distinct `outcome_code` via `guard.fail_thread()`.
/// Resume preflight uses structured payloads for operator-fixable failures
/// such as missing required secrets so downstream consumers can extract
/// `env_var`, source attribution, and remediation without string parsing.
#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("cas context failed: {0}")]
    CasContext(#[source] anyhow::Error),
    #[error("vault read failed: {0}")]
    VaultRead(#[source] anyhow::Error),
    #[error("preflight failed: {0}")]
    Preflight(#[from] crate::execution::launch::MaterializationError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// All inputs needed to run an execution.
pub struct ExecutionParams {
    pub resolved: ResolvedExecutionRequest,
    pub acting_principal: String,
    pub vault_bindings: HashMap<String, String>,
    pub parameters: Value,
    /// Caller-supplied thread id. When `Some(id)`, the new thread row
    /// uses that id (so an external subscriber registered against `id`
    /// receives every lifecycle event from `thread_started` onward).
    /// When `None`, a fresh id is minted via the lifecycle service.
    /// Mirrors the same field on `dispatch::DispatchRequest` and
    /// `execution::launch::build_and_launch`'s native runtime path.
    pub pre_minted_thread_id: Option<String>,
    /// V5.5 P2: composed capability set the daemon will enforce on
    /// every callback dispatch the spawned subprocess attempts. The
    /// caller MUST supply this explicitly — empty `Vec` means deny-all
    /// (the trust-boundary default).
    pub effective_caps: Vec<String>,
    /// Required provenance for this execution. Drives engine,
    /// effective path, snapshot lifecycle gates, and callback minting.
    pub provenance: ExecutionProvenance,
    /// Captured runtime ref (`runtime:<name>`) the thread launched under, so the
    /// resume path resolves the SAME runtime by-ref rather than the kind's
    /// current default. `None` for fresh launches and non-runtime-registry kinds.
    pub runtime_ref: Option<String>,
}

/// Tracks execution state for explicit cleanup.
///
/// Callers MUST invoke `cleanup()` (success/normal-error returns) or
/// `fail_thread()` (pre-spawn errors) on every return path that owns
/// the guard. The guard does NOT use Drop because cleanup is sync
/// and needs access to AppState; a synchronous panic between
/// resource acquisition and the explicit cleanup call will leak the
/// temp dir and leave the tokens in their stores until TTL expiry.
/// For panic-safe revocation of the callback + thread-auth tokens
/// in the detached path, see the `CbTokenGuard` / `TatTokenGuard`
/// installed inside the spawned task.
struct ExecutionGuard {
    state: AppState,
    thread_id: Option<String>,
    temp_dir: Option<Arc<TempDirGuard>>,
    thread_finalized: bool,
    callback_token: Option<String>,
    thread_auth_token: Option<String>,
}

impl ExecutionGuard {
    fn new(state: AppState) -> Self {
        Self {
            state,
            thread_id: None,
            temp_dir: None,
            thread_finalized: false,
            callback_token: None,
            thread_auth_token: None,
        }
    }

    /// Mark thread as tracked by this guard.
    fn track_thread(&mut self, thread_id: &str) {
        self.thread_id = Some(thread_id.to_string());
    }

    /// Mark temp dir for cleanup. The Arc is cloned; the dir is removed
    /// when the last Arc holder drops.
    fn track_temp_dir(&mut self, guard: Arc<TempDirGuard>) {
        self.temp_dir = Some(guard);
    }

    /// Track a callback token for revocation on cleanup.
    fn track_callback_token(&mut self, token: String) {
        self.callback_token = Some(token);
    }

    /// Track a thread-auth token for revocation on cleanup. Wave 5.5
    /// audit: previously the runner minted a thread-auth token via
    /// `mint_callback_env` and dropped the return value (`_tat`), so
    /// the token outlived its only legitimate use and was never
    /// invalidated on resume/retry. The detached background task
    /// now also acquires a `TatTokenGuard` so revocation is symmetric
    /// with the callback token.
    fn track_thread_auth_token(&mut self, token: String) {
        self.thread_auth_token = Some(token);
    }

    /// Fail the tracked thread if it hasn't been finalized yet.
    /// Also revokes the callback and thread-auth tokens.
    fn fail_thread(&mut self, outcome_code: &str) {
        self.fail_thread_with_error(outcome_code, json!({ "code": outcome_code }));
    }

    /// Like [`fail_thread`] but with a custom error JSON body.
    fn fail_thread_with_error(&mut self, outcome_code: &str, error: serde_json::Value) {
        self.revoke_callback_token();
        self.revoke_thread_auth_token();
        if self.thread_finalized {
            return;
        }
        if let Some(ref tid) = self.thread_id {
            let _ = self.state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: tid.clone(),
                status: "failed".to_string(),
                outcome_code: Some(outcome_code.to_string()),
                result: None,
                error: Some(error),
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

    /// Revoke the callback token if one was tracked.
    fn revoke_callback_token(&mut self) {
        if let Some(ref token) = self.callback_token {
            self.state.callback_tokens.invalidate(token);
            if let Some(ref tid) = self.thread_id {
                self.state.callback_tokens.invalidate_for_thread(tid);
            }
        }
    }

    /// Revoke the thread-auth token if one was tracked. Symmetric to
    /// `revoke_callback_token` so resume/retry rotation that mints a
    /// fresh `tat-` token also kills the previous one server-side
    /// instead of leaving it dangling in `ThreadAuthStore`.
    fn revoke_thread_auth_token(&mut self) {
        if let Some(ref token) = self.thread_auth_token {
            self.state.thread_auth.invalidate(token);
            if let Some(ref tid) = self.thread_id {
                self.state.thread_auth.invalidate_for_thread(tid);
            }
        }
    }

    /// Perform all cleanup: drop the temp dir Arc (removes dir when
    /// last holder drops) and revoke tokens.
    fn cleanup(&mut self) {
        self.revoke_callback_token();
        self.revoke_thread_auth_token();
        // Drop the Arc<TempDirGuard>. If this is the last holder,
        // the directory is removed by the TempDirGuard Drop impl.
        self.temp_dir = None;
    }

    /// Consume into parts for moving into tokio::spawn.
    fn into_detached_parts(mut self) -> ExecutionGuardParts {
        ExecutionGuardParts {
            state: self.state.clone(),
            temp_dir: self.temp_dir.take(),
            callback_token: self.callback_token.take(),
            thread_auth_token: self.thread_auth_token.take(),
        }
    }
}

/// Parts harvested from an `ExecutionGuard` before moving into a
/// detached `tokio::spawn`. The background task re-installs deferred
/// revocation guards (`CbTokenGuard`, `TatTokenGuard`) from the token
/// fields so the spawned task's success, error, and panic exits all
/// revoke the per-thread tokens.
///
/// `thread_id` is intentionally omitted — both detached call sites
/// already keep an owned `bg_thread_id` from the `ThreadInsertResult`
/// so the guard's copy would be redundant.
struct ExecutionGuardParts {
    state: AppState,
    temp_dir: Option<Arc<TempDirGuard>>,
    callback_token: Option<String>,
    thread_auth_token: Option<String>,
}

/// Prepare CAS execution context from canonical provenance.
/// Returns (effective_project_path, pre_manifest_hash, base_snapshot_hash).
fn prepare_cas_context(
    state: &AppState,
    provenance: &ExecutionProvenance,
    thread_id: &str,
    guard: &mut ExecutionGuard,
) -> Result<(PathBuf, Option<String>, Option<String>)> {
    match provenance {
        ExecutionProvenance::BorrowedChildLiveFs { project_path, .. } => {
            if !project_path.is_dir() {
                anyhow::bail!(
                    "borrowed effective_path does not exist or is not a directory: {}",
                    project_path.display()
                );
            }
            tracing::trace!(
                thread_id = %thread_id,
                effective_path = %project_path.display(),
                "borrowed CAS context prepared"
            );
            Ok((project_path.clone(), None, None))
        }
        ExecutionProvenance::BorrowedChildPushedHead { effective_path, .. } => {
            if !effective_path.is_dir() {
                anyhow::bail!(
                    "borrowed effective_path does not exist or is not a directory: {}",
                    effective_path.display()
                );
            }
            tracing::trace!(
                thread_id = %thread_id,
                effective_path = %effective_path.display(),
                "borrowed CAS context prepared"
            );
            Ok((effective_path.clone(), None, None))
        }
        ExecutionProvenance::RootLiveFs { project_path, .. } => {
            let _permit = state
                .write_barrier
                .try_acquire()
                .map_err(|e| anyhow::anyhow!("cannot acquire CAS write permit for ingest: {e}"))?;
            let cas_root = state.state_store.cas_root()?;
            let items =
                super::ingest::ingest_directory(&cas_root, project_path, &state.ignore_matcher)?;
            let manifest = ryeos_state::objects::SourceManifest {
                item_source_hashes: items,
            };
            let cas = lillux::cas::CasStore::new(cas_root);
            let manifest_hash = cas.store_object(&manifest.to_value())?;
            tracing::trace!(
                thread_id = %thread_id,
                effective_path = %project_path.display(),
                "live CAS context prepared"
            );

            Ok((project_path.clone(), Some(manifest_hash), None))
        }
        ExecutionProvenance::RootPushedHead {
            effective_path,
            workspace_lifeline,
            snapshot_hash,
            ..
        } => {
            guard.track_temp_dir(workspace_lifeline.clone());
            let manifest_hash = read_pre_manifest_for_snapshot(state, snapshot_hash)?;
            tracing::trace!(
                thread_id = %thread_id,
                effective_path = %effective_path.display(),
                snapshot_hash = %snapshot_hash,
                "pushed CAS context prepared"
            );
            Ok((
                effective_path.clone(),
                Some(manifest_hash),
                Some(snapshot_hash.clone()),
            ))
        }
    }
}

fn read_pre_manifest_for_snapshot(state: &AppState, snap_hash: &str) -> Result<String> {
    let cas_root = state.state_store.cas_root()?;
    let cas = lillux::cas::CasStore::new(cas_root);
    let snap_obj = cas
        .get_object(snap_hash)?
        .ok_or_else(|| anyhow::anyhow!("snapshot {} not found in CAS", snap_hash))?;
    let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&snap_obj)?;
    Ok(snapshot.project_manifest_hash)
}

struct PostExecutionFoldbackParams<'a> {
    pub state: &'a AppState,
    pub thread_id: &'a str,
    pub acting_principal: &'a str,
    pub pre_manifest_hash: &'a Option<String>,
    pub base_snapshot_hash: &'a Option<String>,
    pub project_path: Option<&'a std::path::Path>,
    pub execution_dir: Option<&'a std::path::Path>,
    pub completion: &'a ExecutionCompletion,
}

fn post_execution_foldback(params: PostExecutionFoldbackParams<'_>) {
    let PostExecutionFoldbackParams {
        state,
        thread_id: _thread_id,
        acting_principal,
        pre_manifest_hash,
        base_snapshot_hash,
        project_path,
        execution_dir,
        completion: _completion,
    } = params;
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
    let output_manifest_hash = match crate::execution::fold_back_outputs(
        &cas_root,
        working_dir,
        manifest_hash,
        &state.ignore_matcher,
    ) {
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
            let principal_key = ryeos_state::refs::principal_storage_key(acting_principal);
            let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
            match crate::execution::advance_after_foldback(
                &cas_root,
                &refs_root,
                &signer,
                principal_key,
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
            tracing::debug!("skipping HEAD advance: missing base snapshot hash or LocalPath");
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
/// `docs/future/native-resume-snapshot-pinning.md`.
///
/// Returns the allocated snapshot hash on success, `None` if no
/// pinning was needed (no `native_resume`, or already pinned via a
/// caller-supplied snapshot).
fn pin_localpath_snapshot_if_needed(
    state: &AppState,
    launch_metadata: &mut ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    pre_manifest_hash: &Option<String>,
    _pre_user_manifest_hash: &Option<String>,
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
        message: None,
        project_sync_scope: ryeos_state::project_sync::ProjectSyncScope::FullProject,
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
    launch_metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
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
        let kill = ryeos_app::process::hard_kill_process_group(spawned_pgid);
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
    /// The `--debug-raw` block (resolved cmd/args/cwd/env keys + exit code and
    /// size-limited raw stdout/stderr), present only when the flag was set.
    pub debug: Option<Value>,
}

/// Result of a detached execution launch.
pub struct DetachedResult {
    pub running_thread: ThreadDetail,
}

/// Mint a fresh callback token and build the standard daemon-callback env
/// bindings that every spawned subprocess receives.
///
/// The token is registered in `state.callback_tokens` before return and
/// must be invalidated by the caller after the child exits (or on any
/// failure path that prevents child execution). The token is fresh per
/// spawn and per resume — no reuse across attempts.
///
/// `effective_caps` is the composed capability set the daemon will
/// enforce on every callback dispatch this token authorizes. The
/// caller MUST supply the explicit set; an empty Vec means deny-all.
fn mint_callback_env(
    state: &AppState,
    thread_id: &str,
    project_path: Option<&std::path::Path>,
    duration_seconds: Option<u64>,
    effective_caps: Vec<String>,
    acting_principal: &str,
    caller_scopes: Vec<String>,
    provenance: ExecutionProvenance,
    item_ref: &str,
    // Bundle identity for the token, derived once from the resolved canonical
    // ref by the caller via `effective_bundle_id_for_request` so it matches the
    // identity the runtime-cap minter used. `item_ref` stays the requested ref
    // for provenance/display.
    effective_bundle_id: Option<String>,
) -> Result<(HashMap<String, String>, String, String)> {
    // Run-scoped token: cover the run's full duration + finalization window.
    let ttl = launch_token_ttl(duration_seconds);
    let cap = state.callback_tokens.generate_with_context(
        thread_id,
        project_path.map(|p| p.to_path_buf()).unwrap_or_default(),
        ttl,
        effective_caps,
        provenance,
        effective_bundle_id,
        Some(item_ref.to_string()),
        serde_json::Value::Null,
        0,
    );

    let thread_auth =
        state
            .thread_auth
            .mint(thread_id, acting_principal.to_string(), caller_scopes, ttl);

    let request = SubprocessBuildRequest {
        cmd: PathBuf::new(),
        args: Vec::new(),
        cwd: project_path
            .unwrap_or_else(|| std::path::Path::new("/"))
            .to_path_buf(),
        timeout: std::time::Duration::from_secs(0),
        item_ref: CanonicalRef::parse("runtime:spawn")
            .map_err(|e| anyhow::anyhow!("canonical ref parse: {e}"))?,
        thread_id: thread_id.to_string(),
        project_path: project_path.map(|p| p.to_path_buf()).unwrap_or_default(),
        acting_principal: acting_principal.to_string(),
        cas_root: state.config.app_root.join("cas"),
        callback_token: Some(cap.token.clone()),
        callback_socket_path: Some(state.config.uds_path.to_string_lossy().to_string()),
        vault_handle: None,
        app_root: state.config.app_root.clone(),
        thread_auth_token: Some(thread_auth.token.clone()),
        params: json!({}),
        resolution_output: None,
    };

    let injections: &[(EnvInjectionSource, &str)] = &[
        (EnvInjectionSource::CallbackSocketPath, "RYEOSD_SOCKET_PATH"),
        (EnvInjectionSource::CallbackToken, "RYEOSD_CALLBACK_TOKEN"),
        (EnvInjectionSource::ThreadId, "RYEOSD_THREAD_ID"),
        (EnvInjectionSource::AppRoot, "RYEOS_APP_ROOT"),
        (
            EnvInjectionSource::ThreadAuthToken,
            "RYEOSD_THREAD_AUTH_TOKEN",
        ),
    ];

    let mut bindings = HashMap::new();
    for (source, name) in injections {
        let value = produce_env_value(*source, &request)
            .map_err(|e| anyhow::anyhow!("daemon env injection '{name}': {e}"))?;
        bindings.insert(name.to_string(), value);
    }
    if project_path.is_some() {
        let value = produce_env_value(EnvInjectionSource::ProjectPath, &request)
            .map_err(|e| anyhow::anyhow!("daemon env injection 'RYEOSD_PROJECT_PATH': {e}"))?;
        bindings.insert("RYEOSD_PROJECT_PATH".to_string(), value);
    }

    Ok((bindings, cap.token, thread_auth.token))
}

/// Run an execution inline (blocking until completion).
///
/// Handles the full lifecycle: CAS context, snapshot, spawn,
/// fold-back, finalize, cleanup. On any handled error, the thread
/// is finalized as failed and `guard.cleanup()` is invoked
/// explicitly. The guard is not Drop-based, so a synchronous panic
/// outside of the spawn-task `join_err` branches will leak the
/// temp dir and per-thread tokens until TTL expiry — see the
/// module-level docs.
#[tracing::instrument(
    name = "thread:execute",
    skip(state, params),
    fields(
        thread_id = tracing::field::Empty,
        item_ref = %params.resolved.item_ref,
    )
)]
pub async fn run_inline(state: AppState, mut params: ExecutionParams) -> Result<InlineResult> {
    let mut guard = ExecutionGuard::new(state.clone());

    // Create and track thread.
    // When the caller pre-minted a thread id (e.g. an SSE subscriber
    // registered against a known id before launch), use it verbatim
    // so the persistence-first contract holds: the very first
    // `thread_started` event lands under the id the subscriber sees.
    let created = match params.pre_minted_thread_id.as_deref() {
        Some(id) => state
            .threads
            .create_root_thread_with_id(id, &params.resolved),
        None => state.threads.create_root_thread(&params.resolved),
    }
    .inspect_err(|_e| {
        guard.cleanup();
    })?;
    let running = state
        .threads
        .mark_running(&created.thread_id)
        .inspect_err(|_e| {
            guard.fail_thread("create_failed");
            guard.cleanup();
        })?;
    guard.track_thread(&running.thread_id);
    tracing::Span::current().record("thread_id", running.thread_id.as_str());

    // Prepare CAS context — if this fails, finalize thread as failed
    let (effective_path, pre_manifest_hash, mut base_snapshot_hash) =
        match prepare_cas_context(&state, &params.provenance, &running.thread_id, &mut guard) {
            Ok(ctx) => ctx,
            Err(err) => {
                guard.fail_thread("cas_context_failed");
                guard.cleanup();
                return Err(err);
            }
        };

    // Update project context if CAS checkout changed the path
    if effective_path != params.provenance.effective_path() {
        params.resolved.plan_context.project_context =
            ryeos_engine::contracts::ProjectContext::LocalPath {
                path: effective_path.clone(),
            };
    }

    // Spawn — use the per-request engine (pushed_head overlay or
    // daemon startup engine), NOT state.engine directly.
    let engine = params.provenance.request_engine().clone();
    let tid = running.thread_id.clone();
    let crid = running.chain_root_id.clone();
    let resolved = params.resolved.clone();
    let vault = params.vault_bindings.clone();

    // Mint callback + thread-auth tokens for the spawned subprocess.
    // Track both on the guard so the inline cleanup path invalidates
    // them on handled-error returns. The spawn-task panic branches
    // below catch panics inside the blocking task and run cleanup;
    // a synchronous panic in run_inline itself between mint and the
    // explicit cleanup call would leak both tokens until TTL expiry.
    let child_provenance = params.provenance.clone_for_borrowed_child();
    let (cb_bindings, cb_token, tat_token) = mint_callback_env(
        &state,
        &tid,
        Some(&effective_path),
        None,
        params.effective_caps.clone(),
        &params.acting_principal,
        vec!["execute".to_string()],
        child_provenance,
        &params.resolved.item_ref,
        effective_bundle_id_for_request(&params.resolved),
    )?;
    guard.track_callback_token(cb_token);
    guard.track_thread_auth_token(tat_token);

    // Daemon-owned per-thread state dir under config.app_root — does
    // NOT live under the (ephemeral, CAS-checkout) working directory,
    // so checkpoints survive working-dir cleanup and daemon restart.
    // See `launch_metadata::daemon_thread_state_dir`.
    let thread_state_dir =
        ryeos_app::launch_metadata::daemon_thread_state_dir(&state.config.app_root, &tid);
    let inline_snapshot = base_snapshot_hash.clone();
    let inline_roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
        &engine.resolution_roots(Some(effective_path.clone())),
        &state.config.app_root,
    );
    let mut spawned = match task::spawn_blocking(move || {
        thread_lifecycle::spawn_item(thread_lifecycle::SpawnItemParams {
            engine: &engine,
            resolved: &resolved,
            thread_id: &tid,
            chain_root_id: &crid,
            vault_bindings: vault,
            daemon_callback_env: cb_bindings,
            roots: inline_roots,
            thread_state_dir: Some(thread_state_dir.as_path()),
            is_resume: false,
            original_snapshot_hash: inline_snapshot.as_deref(),
        })
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
    // pre_user_manifest_hash is intentionally None here: the
    // LocalPath flow runs against the daemon's live app root, not
    // a captured snapshot, so there's no pre-execution user manifest
    // to pin alongside the project manifest. The pushed_head flow is
    // where user-manifest lineage matters and is handled separately.
    // LIFECYCLE-INVARIANT: snapshot pin + foldback + HEAD advance run
    // exactly once per pushed_head execution, owned by the Root.
    // BorrowedCallbackChild executions inherit the parent's lineage
    // and MUST NOT touch any of these.
    //   - Root LiveFs:                 skip pin/foldback (no CAS)
    //   - Root PushedHead:             pin + foldback
    //   - BorrowedCallbackChild *:     never pin/foldback
    // `provenance.is_borrowed_child()` returns true iff this is a
    // borrowed callback child. LiveFs gating happens inside the
    // pin/foldback helpers themselves (existing behaviour).
    if !params.provenance.is_borrowed_child() {
        match pin_localpath_snapshot_if_needed(
            &state,
            &mut spawned.launch_metadata,
            &pre_manifest_hash,
            &None,
            &base_snapshot_hash,
        ) {
            Ok(Some(snap)) => {
                base_snapshot_hash = Some(snap);
            }
            Ok(None) => {}
            Err(err) => {
                tracing::error!(error = %err, "failed to pin LocalPath native_resume snapshot");
                // Kill the live child since attach is about to fail anyway.
                let _ = ryeos_app::process::hard_kill_process_group(spawned.pgid);
                guard.fail_thread("snapshot_pin_failed");
                guard.cleanup();
                return Err(err);
            }
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

    // Lift the `--debug-raw` block out of the completion metadata before
    // finalization consumes the completion. `None` on the normal path.
    let debug_block = completion
        .metadata
        .as_ref()
        .and_then(|m| m.get("debug"))
        .cloned();

    if !params.provenance.is_borrowed_child() {
        let guard_exec_dir = guard.temp_dir.as_ref().and_then(|g| g.path());
        post_execution_foldback(PostExecutionFoldbackParams {
            state: &state,
            thread_id: &running.thread_id,
            acting_principal: &params.acting_principal,
            pre_manifest_hash: &pre_manifest_hash,
            base_snapshot_hash: &base_snapshot_hash,
            project_path: Some(params.provenance.original_project_path()),
            execution_dir: guard_exec_dir.as_deref(),
            completion: &completion,
        });
    }

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

    let result = state.threads.build_execute_result(&finalized.thread_id)?;

    guard.cleanup();

    Ok(InlineResult {
        finalized_thread: finalized,
        result: serde_json::to_value(&result).unwrap_or(json!(null)),
        debug: debug_block,
    })
}

/// Launch a detached execution (returns immediately, runs in background).
///
/// Handles the full lifecycle in a background tokio task: CAS
/// context, snapshot, spawn, fold-back, finalize, cleanup. The
/// pre-spawn synchronous setup uses the same explicit-cleanup
/// `ExecutionGuard` discipline as `run_inline` and is **not**
/// panic-safe; once the background task is spawned, the deferred
/// `CbTokenGuard` / `TatTokenGuard` inside it cover token revocation
/// on success, error, and panic.
#[tracing::instrument(
    name = "thread:execute",
    skip(state, params),
    fields(
        thread_id = tracing::field::Empty,
        item_ref = %params.resolved.item_ref,
    )
)]
pub async fn run_detached(state: AppState, mut params: ExecutionParams) -> Result<DetachedResult> {
    let mut guard = ExecutionGuard::new(state.clone());

    // Create and track thread.
    // See `run_inline` for why pre_minted_thread_id is honored.
    let created = match params.pre_minted_thread_id.as_deref() {
        Some(id) => state
            .threads
            .create_root_thread_with_id(id, &params.resolved),
        None => state.threads.create_root_thread(&params.resolved),
    }
    .inspect_err(|_e| {
        guard.cleanup();
    })?;
    let running = state
        .threads
        .mark_running(&created.thread_id)
        .inspect_err(|_e| {
            guard.fail_thread("create_failed");
            guard.cleanup();
        })?;
    guard.track_thread(&running.thread_id);
    tracing::Span::current().record("thread_id", running.thread_id.as_str());

    // Prepare CAS context
    let (effective_path, pre_manifest_hash, base_snapshot_hash) =
        match prepare_cas_context(&state, &params.provenance, &running.thread_id, &mut guard) {
            Ok(ctx) => ctx,
            Err(err) => {
                guard.fail_thread("cas_context_failed");
                guard.cleanup();
                return Err(err);
            }
        };

    // Update project context if CAS checkout changed the path
    if effective_path != params.provenance.effective_path() {
        params.resolved.plan_context.project_context =
            ryeos_engine::contracts::ProjectContext::LocalPath {
                path: effective_path.clone(),
            };
    }

    // Capture thread details before moving guard
    let running_thread_id = running.thread_id.clone();

    // Mint callback + thread-auth tokens for the spawned subprocess.
    // Token lifetimes are owned by the background task (revoked via
    // CbTokenGuard / TatTokenGuard on wait completion, error, or panic).
    let child_provenance = params.provenance.clone_for_borrowed_child();
    let (cb_bindings, cb_token, tat_token) = mint_callback_env(
        &state,
        &running.thread_id,
        Some(effective_path.as_path()),
        None,
        params.effective_caps.clone(),
        &params.acting_principal,
        vec!["execute".to_string()],
        child_provenance,
        &params.resolved.item_ref,
        effective_bundle_id_for_request(&params.resolved),
    )?;
    guard.track_callback_token(cb_token);
    guard.track_thread_auth_token(tat_token);

    // Move guard parts into the background task
    let parts = guard.into_detached_parts();
    let bg_state = parts.state;
    let bg_temp_dir = parts.temp_dir;
    let bg_cb_token = parts.callback_token;
    let bg_tat_token = parts.thread_auth_token;
    let bg_thread_id = running.thread_id.clone();
    let bg_chain_root_id = running.chain_root_id.clone();
    let bg_resolved = params.resolved.clone();
    // Per-request engine (pushed_head overlay or daemon startup engine).
    let bg_engine = params.provenance.request_engine().clone();
    let bg_vault = params.vault_bindings.clone();
    let bg_cb_bindings = cb_bindings;
    let bg_acting_principal = params.acting_principal.clone();
    let bg_pre_manifest_hash = pre_manifest_hash;
    let bg_base_snapshot_hash = base_snapshot_hash;
    let bg_project_path = Some(params.provenance.original_project_path().to_path_buf());
    let bg_skip_snapshot_lifecycle = params.provenance.is_borrowed_child();
    let bg_runtime_state_dir = state.config.app_root.clone();

    tokio::spawn(dispatch_detached_bg_task(
        bg_state,
        bg_thread_id,
        bg_chain_root_id,
        bg_resolved,
        bg_engine,
        bg_vault,
        bg_cb_bindings,
        bg_acting_principal,
        bg_pre_manifest_hash,
        bg_base_snapshot_hash,
        bg_project_path,
        bg_temp_dir,
        bg_skip_snapshot_lifecycle,
        bg_runtime_state_dir,
        false, // is_resume
        None,  // prior_status_for_mark_running
        bg_cb_token,
        bg_tat_token,
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
        bg_cb_bindings, bg_acting_principal, bg_pre_manifest_hash,
        bg_base_snapshot_hash,
        bg_project_path, bg_temp_dir, bg_skip_snapshot_lifecycle, bg_runtime_state_dir,
        prior_status_for_mark_running,
        bg_cb_token, bg_tat_token
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
    bg_cb_bindings: HashMap<String, String>,
    bg_acting_principal: String,
    bg_pre_manifest_hash: Option<String>,
    mut bg_base_snapshot_hash: Option<String>,
    bg_project_path: Option<PathBuf>,
    mut bg_temp_dir: Option<Arc<TempDirGuard>>,
    bg_skip_snapshot_lifecycle: bool,
    bg_runtime_state_dir: PathBuf,
    is_resume: bool,
    prior_status_for_mark_running: Option<String>,
    bg_cb_token: Option<String>,
    bg_tat_token: Option<String>,
) {
    // Revoke callback + thread-auth tokens on every exit path of this
    // function. Both tokens' lifetimes are owned by this background task.
    let _cb_guard = defer_cb_token_revocation(&bg_state, &bg_thread_id, &bg_cb_token);
    let _tat_guard = defer_tat_token_revocation(&bg_state, &bg_thread_id, &bg_tat_token);

    if let Some(ref s) = prior_status_for_mark_running {
        tracing::Span::current().record("prior_status", s.as_str());
    }
    let log_phase = if is_resume { "resume" } else { "detached" };
    let attach_outcome_code = if is_resume {
        "resume_attach_failed"
    } else {
        "attach_failed"
    };

    let thread_state_dir =
        ryeos_app::launch_metadata::daemon_thread_state_dir(&bg_runtime_state_dir, &bg_thread_id);

    let tid_for_spawn = bg_thread_id.clone();
    let crid_for_spawn = bg_chain_root_id.clone();
    let res_for_spawn = bg_resolved.clone();
    let eng_for_spawn = bg_engine.clone();
    let vault_for_spawn = bg_vault;
    let cb_for_spawn = bg_cb_bindings;
    let snap_for_spawn = bg_base_snapshot_hash.clone();

    let spawn_result = task::spawn_blocking(move || {
        let project_root = match &res_for_spawn.plan_context.project_context {
            ryeos_engine::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
            _ => None,
        };
        let roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
            &eng_for_spawn.resolution_roots(project_root),
            &bg_runtime_state_dir,
        );
        thread_lifecycle::spawn_item(thread_lifecycle::SpawnItemParams {
            engine: &eng_for_spawn,
            resolved: &res_for_spawn,
            thread_id: &tid_for_spawn,
            chain_root_id: &crid_for_spawn,
            vault_bindings: vault_for_spawn,
            daemon_callback_env: cb_for_spawn,
            roots,
            thread_state_dir: Some(thread_state_dir.as_path()),
            is_resume,
            original_snapshot_hash: snap_for_spawn.as_deref(),
        })
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
            drop(bg_temp_dir.take());
            return;
        }
        Err(join_err) => {
            tracing::error!(
                phase = log_phase,
                error = %join_err,
                "task panic during spawn"
            );
            fail_thread_static(&bg_state, &bg_thread_id, "task_panic");
            drop(bg_temp_dir.take());
            return;
        }
    };

    // Pin LocalPath native_resume to a snapshot before attach.
    // pre_user_manifest_hash is intentionally None for the same
    // reason as the inline-spawn path: LocalPath runs against the
    // daemon's live app root, not a captured snapshot.
    if !bg_skip_snapshot_lifecycle {
        match pin_localpath_snapshot_if_needed(
            &bg_state,
            &mut spawned.launch_metadata,
            &bg_pre_manifest_hash,
            &None,
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
                let _ = ryeos_app::process::hard_kill_process_group(spawned.pgid);
                fail_thread_static(&bg_state, &bg_thread_id, "snapshot_pin_failed");
                drop(bg_temp_dir.take());
                return;
            }
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
        drop(bg_temp_dir.take());
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
    // Extract the execution dir path while the Arc is still alive.
    let bg_exec_dir_path = bg_temp_dir.as_ref().and_then(|g| g.path());
    match wait_result {
        Ok(completion) => {
            if !bg_skip_snapshot_lifecycle {
                post_execution_foldback(PostExecutionFoldbackParams {
                    state: &bg_state,
                    thread_id: &bg_thread_id,
                    acting_principal: &bg_acting_principal,
                    pre_manifest_hash: &bg_pre_manifest_hash,
                    base_snapshot_hash: &bg_base_snapshot_hash,
                    project_path: bg_project_path.as_deref(),
                    execution_dir: bg_exec_dir_path.as_deref(),
                    completion: &completion,
                });
            }
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

    // Drop the Arc<TempDirGuard>. If this is the last holder, the
    // directory is removed by the TempDirGuard Drop impl.
    drop(bg_temp_dir);
}

/// Fail a thread without a guard (for use inside detached tasks).
/// Note: callback token revocation is handled by CbTokenGuard::drop.
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

/// Revoke a callback token. Called on every exit path of the background task.
fn revoke_token(state: &AppState, thread_id: &str, token: &Option<String>) {
    if let Some(ref t) = token {
        state.callback_tokens.invalidate(t);
    }
    state.callback_tokens.invalidate_for_thread(thread_id);
}

/// Set up deferred token revocation. Returns a guard struct that revokes
/// the token when dropped. Used as the first statement in the detached bg
/// task so every return path (success, error, panic) revokes the token.
struct CbTokenGuard {
    state: AppState,
    thread_id: String,
    token: Option<String>,
}

impl CbTokenGuard {
    fn new(state: AppState, thread_id: String, token: Option<String>) -> Self {
        Self {
            state,
            thread_id,
            token,
        }
    }
}

impl Drop for CbTokenGuard {
    fn drop(&mut self) {
        revoke_token(&self.state, &self.thread_id, &self.token);
    }
}

fn defer_cb_token_revocation(
    state: &AppState,
    thread_id: &str,
    token: &Option<String>,
) -> CbTokenGuard {
    CbTokenGuard::new(state.clone(), thread_id.to_string(), token.clone())
}

/// Deferred thread-auth-token revocation. Symmetric to `CbTokenGuard`:
/// the detached background task installs one of these as its first
/// statement so every exit path (success, error, panic) invalidates
/// the `tat-` token in `ThreadAuthStore`.
///
/// Wave 5.5 audit: previously the `tat` returned by `mint_callback_env`
/// was dropped by the caller (`let (.., _tat) = ...`), so resume/retry
/// would mint a fresh token but the previous one would survive in the
/// store until either its TTL expired or the thread was finalized
/// elsewhere. Now both tokens have lifetimes scoped to the bg task.
fn revoke_tat_token(state: &AppState, thread_id: &str, token: &Option<String>) {
    if let Some(ref t) = token {
        state.thread_auth.invalidate(t);
    }
    state.thread_auth.invalidate_for_thread(thread_id);
}

struct TatTokenGuard {
    state: AppState,
    thread_id: String,
    token: Option<String>,
}

impl TatTokenGuard {
    fn new(state: AppState, thread_id: String, token: Option<String>) -> Self {
        Self {
            state,
            thread_id,
            token,
        }
    }
}

impl Drop for TatTokenGuard {
    fn drop(&mut self) {
        revoke_tat_token(&self.state, &self.thread_id, &self.token);
    }
}

fn defer_tat_token_revocation(
    state: &AppState,
    thread_id: &str,
    token: &Option<String>,
) -> TatTokenGuard {
    TatTokenGuard::new(state.clone(), thread_id.to_string(), token.clone())
}

/// Build `ExecutionParams` from a captured `ResumeContext`.
///
/// Re-runs the engine resolver against the **original**
/// `ProjectContext`. When `original_snapshot_hash` is set, the
/// resolver runs against the pinned snapshot (via
/// `ProjectContext::SnapshotHash`), so the resumed plan matches the
/// project version captured at the original spawn — not the current
/// working-dir head. See `docs/future/native-resume-snapshot-pinning.md`.
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
    // Always use the original project_context (LocalPath) for item
    // resolution. The engine's resolve() only searches project space
    // when project_context is LocalPath; SnapshotHash has no filesystem
    // path for the engine to walk.
    //
    // The snapshot hash is carried separately via
    // `ExecutionParams::snapshot_hash` and used by
    // `prepare_cas_context` inside `run_existing_detached` to check
    // out the pinned snapshot. After checkout the plan_context is
    // updated to point at the materialized checkout directory, so the
    // effective runtime sees the exact project version captured at
    // spawn time.
    let plan_ctx = PlanContext {
        requested_by: resume.requested_by.clone(),
        project_context: resume.project_context.clone(),
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

    // Reconstruct the executor identity for the resumed launch, in priority:
    //   1. captured `executor_ref` (exact identity this thread launched under);
    //   2. captured runtime by-ref — a delegate kind (directive, graph) has NO
    //      item `executor_id`, so its identity is the serving runtime's
    //      `native:<binary>`. A captured-but-bad ref is an error, never a silent
    //      switch to a different runtime;
    //   3. the item's own `metadata.executor_id`;
    //   4. the kind's default runtime.
    let executor_ref = if let Some(er) = resume.executor_ref.clone() {
        er
    } else if let Some(rr) = resume.runtime_ref.as_deref() {
        let runtime = state
            .engine
            .runtimes
            .resolve_for_launch(Some(rr), &resolved_item.kind)
            .map_err(|e| anyhow::anyhow!("resume: {e}"))?;
        let bare = crate::dispatch::strip_binary_ref_prefix(&runtime.yaml.binary_ref)?;
        format!("native:{bare}")
    } else if let Some(eid) = resolved_item.metadata.executor_id.clone() {
        eid
    } else {
        let runtime = state
            .engine
            .runtimes
            .resolve_for_launch(None, &resolved_item.kind)
            .map_err(|e| {
                anyhow::anyhow!(
                    "resume: item {} has neither an executor_id nor a runtime-registry \
                     entry for kind {}: {e}",
                    resume.item_ref,
                    resolved_item.kind,
                )
            })?;
        let bare = crate::dispatch::strip_binary_ref_prefix(&runtime.yaml.binary_ref)?;
        format!("native:{bare}")
    };

    let resolved = ResolvedExecutionRequest {
        kind: resume.kind.clone(),
        item_ref: resume.item_ref.clone(),
        executor_ref,
        launch_mode: resume.launch_mode.clone(),
        current_site_id: resume.current_site_id.clone(),
        origin_site_id: resume.origin_site_id.clone(),
        target_site_id: None,
        requested_by: resume.requested_by_name(),
        usage_subject: None,
        usage_subject_asserted_by: None,
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

    let acting_principal = resume
        .requested_by_name()
        .unwrap_or_else(|| "fp:resume".to_string());

    // TODO(resume-overlay): resume still uses the daemon startup
    // engine and a LiveFs root provenance until the resume path can
    // rebuild per-snapshot overlay engines and temp-dir lifelines.
    let provenance = ExecutionProvenance::root_live_fs(
        project_path.clone().unwrap_or_else(|| PathBuf::from(".")),
        state.engine.clone(),
    );

    // NOTE: read_required_secrets and envelope-field preflight are NOT
    // called here. They run later, inside run_existing_detached(), AFTER
    // prepare_cas_context() returns the effective_path — so dotenv overlay
    // and provider resolution see the snapshot checkout, not the live tree.

    Ok(ExecutionParams {
        resolved,
        acting_principal,
        vault_bindings: HashMap::new(), // populated later in run_existing_detached
        parameters: resume.parameters.clone(),
        // Resume reuses the existing thread row's id by going through
        // `attach_to_existing_thread` rather than `create_root_thread`,
        // so `pre_minted_thread_id` is irrelevant on this path.
        pre_minted_thread_id: None,
        // V5.5 P2: carry the effective_caps captured at original
        // spawn time. The reconciler restores them verbatim so
        // resumed callbacks are enforced under the same set the
        // pre-crash run had.
        effective_caps: resume.effective_caps.clone(),
        provenance,
        // Resolve the SAME runtime this thread launched under on resume.
        runtime_ref: resume.runtime_ref.clone(),
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
/// semantics. See `docs/future/native-resume-snapshot-pinning.md`
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
) -> Result<(), ResumeError> {
    let mut guard = ExecutionGuard::new(state.clone());
    guard.track_thread(&thread_id);

    // Prepare CAS context.
    let (effective_path, pre_manifest_hash, base_snapshot_hash) =
        match prepare_cas_context(&state, &params.provenance, &thread_id, &mut guard) {
            Ok(ctx) => ctx,
            Err(err) => {
                guard.fail_thread("cas_context_failed");
                guard.cleanup();
                return Err(ResumeError::CasContext(err));
            }
        };

    // Update plan_context to point at the materialized path so the
    // engine resolves item refs from there.
    if effective_path != params.provenance.effective_path() {
        params.resolved.plan_context.project_context = ProjectContext::LocalPath {
            path: effective_path.clone(),
        };
    }

    // ── Vault + envelope-field preflight (post-CAS) ─────────────────
    // These MUST run after prepare_cas_context so dotenv overlay and
    // provider resolution see the snapshot checkout, not the live tree.
    {
        let engine = params.provenance.request_engine();
        // Resolve via the captured runtime ref (the runtime this thread launched
        // under) rather than the kind's current default, consistent with every
        // other runtime-resolution site. A captured-but-bad ref (malformed,
        // unregistered, or serving the wrong kind) is an error — fail the resume
        // rather than silently proceeding with no envelope requirements. Only the
        // `None` case (no captured ref, no registry entry) degrades to empty.
        let required_envelope_fields = match engine.runtimes.resolve_for_launch(
            params.runtime_ref.as_deref(),
            &params.resolved.resolved_item.kind,
        ) {
            Ok(runtime) => runtime.yaml.required_envelope_fields.clone(),
            Err(_) if params.runtime_ref.is_none() => Vec::new(),
            Err(e) => {
                guard.fail_thread("preflight_failed");
                guard.cleanup();
                return Err(ResumeError::Other(anyhow::anyhow!(
                    "resume: captured runtime_ref unresolvable: {e}"
                )));
            }
        };

        let provider_preflight =
            if crate::execution::launch::requires_provider_snapshot(&required_envelope_fields) {
                let engine_roots = engine.resolution_roots(Some(effective_path.clone()));
                let effective_parsers = engine
                    .effective_parser_dispatcher(Some(&effective_path))
                    .map_err(|e| {
                    guard.fail_thread("preflight_failed");
                    guard.cleanup();
                    ResumeError::Other(anyhow::anyhow!("resume: effective parser dispatcher: {e}"))
                })?;

                let resolution = ryeos_engine::resolution::run_resolution_pipeline(
                    &params.resolved.resolved_item.canonical_ref,
                    &engine.kinds,
                    &effective_parsers,
                    &engine_roots,
                    &engine.trust_store,
                    &engine.composers,
                )
                .map_err(|e| {
                    guard.fail_thread("preflight_failed");
                    guard.cleanup();
                    ResumeError::Other(anyhow::anyhow!(
                        "resume: resolution pipeline for preflight: {e}"
                    ))
                })?;

                let operator_trusted_keys_dir = state.config.runtime_root().trusted_keys_dir();
                Some(
                    crate::execution::launch::resolve_provider_preflight(
                        &resolution.composed,
                        &engine_roots,
                        &operator_trusted_keys_dir,
                    )
                    .map_err(|e| {
                        guard.fail_thread("preflight_failed");
                        guard.cleanup();
                        ResumeError::Preflight(e)
                    })?,
                )
            } else {
                None
            };

        let secret_requirements = crate::execution::launch::build_secret_requirements(
            &params.resolved.resolved_item.metadata.required_secrets,
            provider_preflight.as_ref(),
        );
        let secret_names: Vec<String> = secret_requirements
            .iter()
            .map(|req| req.name.clone())
            .collect();
        let dotenv_dirs = ryeos_app::vault::dotenv_search_dirs(Some(&effective_path));
        let vault_bindings = ryeos_app::vault::read_required_secrets(
            state.vault.as_ref(),
            &params.acting_principal,
            &secret_names,
            &dotenv_dirs,
        )
        .map_err(|e| match e {
            ryeos_app::vault::VaultReadError::MissingSecrets { names, .. } => {
                let missing = crate::execution::launch::missing_secrets_from_requirements(
                    &names,
                    &secret_requirements,
                );
                if let Some(first) = missing.first() {
                    let payload = crate::execution::launch::required_secret_missing_payload(
                        &params.resolved.item_ref,
                        first,
                    );
                    guard.fail_thread_with_error("required_secret_missing", payload);
                } else {
                    guard.fail_thread("vault_read_failed");
                }
                guard.cleanup();
                ResumeError::VaultRead(
                    ryeos_app::vault::VaultReadError::MissingSecrets {
                        principal: params.acting_principal.clone(),
                        names,
                    }
                    .into(),
                )
            }
            ryeos_app::vault::VaultReadError::Internal(e) => {
                guard.fail_thread("vault_read_failed");
                guard.cleanup();
                ResumeError::VaultRead(ryeos_app::vault::VaultReadError::Internal(e).into())
            }
        })?;

        params.vault_bindings = vault_bindings;
    }

    // Mint FRESH callback + thread-auth tokens for the resumed
    // subprocess (never reuse the originals — they were revoked when
    // the prior process exited via the bg task's CbTokenGuard /
    // TatTokenGuard drops).
    let child_provenance = params.provenance.clone_for_borrowed_child();
    let (cb_bindings, cb_token, tat_token) = mint_callback_env(
        &state,
        &thread_id,
        Some(effective_path.as_path()),
        None,
        params.effective_caps.clone(),
        &params.acting_principal,
        vec!["execute".to_string()],
        child_provenance,
        &params.resolved.item_ref,
        effective_bundle_id_for_request(&params.resolved),
    )?;
    guard.track_callback_token(cb_token);
    guard.track_thread_auth_token(tat_token);

    let parts = guard.into_detached_parts();
    let bg_state = parts.state;
    let bg_temp_dir = parts.temp_dir;
    let bg_cb_token = parts.callback_token;
    let bg_tat_token = parts.thread_auth_token;
    let bg_thread_id = thread_id.clone();
    let bg_chain_root_id = chain_root_id.clone();
    let bg_resolved = params.resolved.clone();
    // Per-request engine (resume path uses daemon engine via
    // execution_params_from_resume_context).
    let bg_engine = params.provenance.request_engine().clone();
    let bg_vault = params.vault_bindings.clone();
    let bg_cb_bindings = cb_bindings;
    let bg_acting_principal = params.acting_principal.clone();
    let bg_pre_manifest_hash = pre_manifest_hash;
    let bg_base_snapshot_hash = base_snapshot_hash;
    let bg_project_path = Some(params.provenance.original_project_path().to_path_buf());
    let bg_skip_snapshot_lifecycle = params.provenance.is_borrowed_child();
    let bg_runtime_state_dir = state.config.app_root.clone();

    tokio::spawn(dispatch_detached_bg_task(
        bg_state,
        bg_thread_id,
        bg_chain_root_id,
        bg_resolved,
        bg_engine,
        bg_vault,
        bg_cb_bindings,
        bg_acting_principal,
        bg_pre_manifest_hash,
        bg_base_snapshot_hash,
        bg_project_path,
        bg_temp_dir,
        bg_skip_snapshot_lifecycle,
        bg_runtime_state_dir,
        true, // is_resume
        Some(prior_status),
        bg_cb_token,
        bg_tat_token,
    ));

    Ok(())
}
