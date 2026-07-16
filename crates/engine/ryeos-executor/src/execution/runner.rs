//! Unified execution runner.
//!
//! Both `/execute` and inbound webhooks use this to run the full
//! execution lifecycle: CAS context → snapshot → spawn →
//! fold-back → finalize → cleanup.
//!
//! The `ExecutionGuard` struct tracks transient state (temp dir,
//! thread row and any protocol-requested callback/thread-auth tokens) and exposes
//! `cleanup()` / `fail_thread()` for callers to invoke on their
//! return paths. Its Drop fallback always revokes transient authority
//! and either conditionally finalizes a never-launched row or durably
//! tombstones, exact-kills, and settles an advanced inline execution tree.
//! Shutdown gate closure transfers that cleanup to the coordinator. The
//! detached background path
//! additionally installs `CbTokenGuard` and `TatTokenGuard` inside
//! the spawned task so background-task panic, error, and success exits
//! all revoke the per-thread tokens.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::task;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{ExecutionCompletion, PlanContext, ProjectContext};
use ryeos_engine::protocol_vocabulary::{produce_env_value, CallbackChannel, EnvInjectionSource};
use ryeos_engine::subprocess_spec::SubprocessBuildRequest;

use ryeos_app::callback_token::effective_bundle_id_for_request;
use ryeos_app::callback_token::launch_token_ttl;
use ryeos_app::env_contract::{EnvBinding, EnvSourceDetail};
use ryeos_app::execution_provenance::ExecutionProvenance;
use ryeos_app::launch_metadata::ResumeContext;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{
    is_terminal_status, StopIfAdmissionOpenOutcome, StopIntent, ThreadDetail,
};
use ryeos_app::temp_dir_guard::TempDirGuard;
use ryeos_app::thread_lifecycle::{
    self, ResolvedExecutionRequest, SealedRootExecutionRequest, ThreadAttachProcessParams,
    ThreadFinalizeParams,
};

use super::launch::RecoveryLaunchOutcome;
use super::launch_claim::{ThreadLaunchClaim, ThreadLaunchClaimOutcome};

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
    /// Validated callback parent, carried out-of-band from action params. Direct
    /// tool subprocesses use it to persist operational lineage before spawn so
    /// parent stop/kill cascades cannot miss them.
    pub parent_thread_id: Option<String>,
}

/// Tracks execution state for explicit cleanup, with a conservative Drop net.
///
/// Callers still invoke `cleanup()` (success/normal-error returns) or
/// `fail_thread()` (pre-spawn errors) so failures can be reported at their
/// source. Drop covers panic/cancellation: it revokes tokens and releases the
/// temp-dir lifeline, then durably stops, exact-kills, and settles the owned
/// inline execution tree. A closed shutdown gate transfers that work to the
/// shutdown coordinator; detached handoff explicitly disarms this guard.
struct ExecutionGuard {
    state: AppState,
    thread_id: Option<String>,
    temp_dir: Option<Arc<TempDirGuard>>,
    thread_finalized: bool,
    callback_token: Option<String>,
    thread_auth_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionCleanupOutcome {
    AlreadyFinalized,
    Finalized,
    AlreadyTerminal,
    DurableStopSettled,
    PreservedForShutdown,
    Failed,
}

impl ExecutionCleanupOutcome {
    fn disarms_guard(self) -> bool {
        !matches!(self, Self::Failed)
    }
}

#[derive(Debug)]
struct ExecutionCleanupFailure {
    operation: &'static str,
    operation_error: anyhow::Error,
    cleanup: Result<ExecutionCleanupOutcome>,
}

impl ExecutionCleanupFailure {
    fn cleanup_disarms_guard(&self) -> bool {
        self.cleanup
            .as_ref()
            .is_ok_and(|outcome| outcome.disarms_guard())
    }
}

impl std::fmt::Display for ExecutionCleanupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} failed: {}",
            self.operation, self.operation_error
        )?;
        match &self.cleanup {
            Ok(outcome) => write!(formatter, "; cleanup outcome: {outcome:?}"),
            Err(error) => write!(formatter, "; terminal cleanup also failed: {error:#}"),
        }
    }
}

impl std::error::Error for ExecutionCleanupFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.operation_error.as_ref())
    }
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

    /// Track a protocol-requested thread-auth token for revocation on cleanup.
    /// The detached background task acquires a `TatTokenGuard` so revocation is
    /// symmetric with an optional callback token.
    fn track_thread_auth_token(&mut self, token: String) {
        self.thread_auth_token = Some(token);
    }

    /// Fail the tracked thread if it hasn't been finalized yet.
    /// Also revokes the callback and thread-auth tokens.
    fn fail_thread(&mut self, outcome_code: &str) -> ExecutionCleanupOutcome {
        self.fail_thread_with_error(outcome_code, json!({ "code": outcome_code }))
    }

    /// Like [`fail_thread`] but with a custom error JSON body.
    fn fail_thread_with_error(
        &mut self,
        outcome_code: &str,
        error: serde_json::Value,
    ) -> ExecutionCleanupOutcome {
        self.revoke_callback_token();
        self.revoke_thread_auth_token();
        if self.thread_finalized {
            return ExecutionCleanupOutcome::AlreadyFinalized;
        }
        if let Some(ref tid) = self.thread_id {
            match super::process_attachment::finalize_requested_stop_if_present(&self.state, tid) {
                Ok(true) => {
                    self.thread_finalized = true;
                    return ExecutionCleanupOutcome::DurableStopSettled;
                }
                Ok(false) => {}
                Err(settle_error) => {
                    tracing::error!(
                        thread_id = %tid,
                        error = %settle_error,
                        "failed to settle durable stop while failing execution"
                    );
                    return ExecutionCleanupOutcome::Failed;
                }
            }
            if !self
                .state
                .state_store
                .process_attachment_admission_is_open()
            {
                if let Err(reset_error) = self.state.state_store.reset_resume_attempts(tid) {
                    tracing::error!(
                        thread_id = %tid,
                        error = %reset_error,
                        "failed to re-arm preserved execution during shutdown"
                    );
                    return ExecutionCleanupOutcome::Failed;
                }
                // `thread_finalized` also disarms Drop. Shutdown deliberately
                // preserves this nonterminal row for coordinator ownership.
                self.thread_finalized = true;
                return ExecutionCleanupOutcome::PreservedForShutdown;
            }
            let finalize = self.state.threads.finalize_thread(&ThreadFinalizeParams {
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
            match finalize {
                Ok(_) => {
                    self.thread_finalized = true;
                    return ExecutionCleanupOutcome::Finalized;
                }
                Err(finalize_error) => {
                    match self.state.threads.get_thread(tid) {
                        Ok(Some(thread))
                            if ryeos_app::state_store::is_terminal_status(&thread.status) =>
                        {
                            self.thread_finalized = true;
                            return ExecutionCleanupOutcome::AlreadyTerminal;
                        }
                        Ok(_) => {}
                        Err(read_error) => tracing::error!(
                            thread_id = %tid,
                            error = %read_error,
                            "failed to verify lifecycle after terminal cleanup error"
                        ),
                    }
                    tracing::error!(
                        thread_id = %tid,
                        error = %finalize_error,
                        "failed to persist execution terminal cleanup"
                    );
                    return ExecutionCleanupOutcome::Failed;
                }
            }
        }
        ExecutionCleanupOutcome::Failed
    }

    fn finalize_child_link_failure_if_current(
        &mut self,
        error: serde_json::Value,
    ) -> anyhow::Result<ryeos_app::thread_lifecycle::FinalizeCreatedUnattachedOutcome> {
        self.revoke_callback_token();
        self.revoke_thread_auth_token();
        let thread_id = self
            .thread_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("child-link cleanup has no tracked thread"))?;
        let outcome =
            crate::dispatch::finalize_child_link_failure_if_current(&self.state, thread_id, error)?;
        if outcome.is_settled() {
            self.thread_finalized = true;
        }
        Ok(outcome)
    }

    /// Mark thread as finalized (by external code).
    fn mark_finalized(&mut self) {
        self.thread_finalized = true;
    }

    /// Revoke the callback token if one was tracked.
    fn revoke_callback_token(&mut self) {
        if let Some(token) = self.callback_token.take() {
            self.state.callback_tokens.invalidate(&token);
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
        if let Some(token) = self.thread_auth_token.take() {
            self.state.thread_auth.invalidate(&token);
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
        // Ownership moves to the detached task and its token Drop guards. Disarm
        // this guard's lifecycle fallback before `self` drops at function exit;
        // otherwise it could race the background launcher and conditionally
        // fail the row before that task claims/attaches it.
        self.thread_finalized = true;
        self.thread_id = None;
        ExecutionGuardParts {
            state: self.state.clone(),
            temp_dir: self.temp_dir.take(),
            callback_token: self.callback_token.take(),
            thread_auth_token: self.thread_auth_token.take(),
        }
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        // Revoke callback authority immediately, but retain the workspace
        // lifeline until the owned process tree has stopped. An inline runtime
        // may still have its cwd inside that directory while cancellation is
        // synchronously killing/reaping it.
        self.revoke_callback_token();
        self.revoke_thread_auth_token();
        if self.thread_finalized {
            self.temp_dir = None;
            return;
        }
        let Some(thread_id) = self.thread_id.clone() else {
            self.temp_dir = None;
            return;
        };

        match stop_owner_dropped_execution_tree(&self.state, &thread_id) {
            Ok(OwnerDropStopOutcome::Settled) => {
                self.thread_finalized = true;
            }
            Ok(OwnerDropStopOutcome::PreservedForShutdown) => tracing::info!(
                thread_id,
                "execution guard drop preserved row for shutdown coordinator"
            ),
            Err(error) => tracing::error!(
                thread_id,
                error = %error,
                "execution guard drop could not fully stop and settle owned execution tree"
            ),
        }
        self.temp_dir = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerDropStopOutcome {
    Settled,
    PreservedForShutdown,
}

/// Cancellation/panic fallback for an inline owner.
///
/// Tombstoning under the admission gate closes the pre-attach race. For an
/// attached identity, the exact process group is synchronously hard-killed and
/// compare-cleared before the durable Kill is finalized. Descendants receive
/// the same treatment so dropping a parent request cannot leave callback-spawned
/// work alive. Shutdown gate closure wins atomically and leaves all remaining
/// work to the coordinator.
pub(crate) fn stop_owner_dropped_execution_tree(
    state: &AppState,
    root_thread_id: &str,
) -> Result<OwnerDropStopOutcome> {
    match stop_owner_dropped_thread(state, root_thread_id)? {
        OwnerDropStopOutcome::PreservedForShutdown => {
            return Ok(OwnerDropStopOutcome::PreservedForShutdown)
        }
        OwnerDropStopOutcome::Settled => {}
    }

    const MAX_DESCENDANT_FIXED_POINT_PASSES: usize = 16;
    let mut seen = BTreeSet::from([root_thread_id.to_string()]);
    let mut failures = Vec::new();
    let mut reached_fixed_point = false;
    for _ in 0..MAX_DESCENDANT_FIXED_POINT_PASSES {
        let descendants = state.state_store.descendant_thread_ids(root_thread_id)?;
        let new_descendants = descendants
            .into_iter()
            .filter(|thread_id| seen.insert(thread_id.clone()))
            .collect::<Vec<_>>();
        if new_descendants.is_empty() {
            reached_fixed_point = true;
            break;
        }
        for thread_id in new_descendants {
            match stop_owner_dropped_thread(state, &thread_id) {
                Ok(OwnerDropStopOutcome::Settled) => {}
                Ok(OwnerDropStopOutcome::PreservedForShutdown) => {
                    tracing::info!(
                        thread_id,
                        root_thread_id,
                        "shutdown coordinator took ownership before descendant cancellation"
                    );
                }
                Err(error) => failures.push(format!("{thread_id}: {error:#}")),
            }
        }
    }
    if !reached_fixed_point {
        failures.push(format!(
            "descendant fixed-point did not converge within \
             {MAX_DESCENDANT_FIXED_POINT_PASSES} passes"
        ));
    }
    if !failures.is_empty() {
        anyhow::bail!(
            "failed to settle {} owner-dropped descendant(s): {}",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok(OwnerDropStopOutcome::Settled)
}

fn stop_owner_dropped_thread(state: &AppState, thread_id: &str) -> Result<OwnerDropStopOutcome> {
    let runtime = match state
        .state_store
        .request_thread_stop_if_admission_open(thread_id, StopIntent::Kill)?
    {
        StopIfAdmissionOpenOutcome::Requested(runtime) => runtime,
        StopIfAdmissionOpenOutcome::AlreadyTerminal => return Ok(OwnerDropStopOutcome::Settled),
        StopIfAdmissionOpenOutcome::PreservedForShutdown => {
            return Ok(OwnerDropStopOutcome::PreservedForShutdown)
        }
    };

    let mut clear_error = None;
    if let Some(identity) = runtime.process_identity.as_ref() {
        let killed =
            ryeos_app::process::kill_by_action(identity, ryeos_app::process::ShutdownAction::Hard);
        if !killed.success {
            anyhow::bail!(
                "identity-verified hard kill did not confirm process-group exit ({})",
                killed.method
            );
        }
        match state
            .state_store
            .clear_thread_process_if_matches(thread_id, identity)
        {
            Ok(true) => {}
            Ok(false) => {
                let current_identity = state
                    .threads
                    .get_thread(thread_id)?
                    .and_then(|thread| thread.runtime.process_identity);
                if current_identity.is_some() {
                    clear_error = Some(anyhow::anyhow!(
                        "killed process identity changed before compare-and-clear"
                    ));
                }
            }
            Err(error) => {
                clear_error = Some(error.context("compare-clear killed process identity"));
            }
        }
    }

    if !super::process_attachment::finalize_requested_stop_if_present(state, thread_id)? {
        anyhow::bail!("owner-drop Kill tombstone disappeared before finalization");
    }
    let terminal = state
        .threads
        .get_thread(thread_id)?
        .is_some_and(|thread| is_terminal_status(&thread.status));
    if !terminal {
        anyhow::bail!("owner-dropped thread did not reach a terminal status");
    }
    if let Some(error) = clear_error {
        return Err(error);
    }
    Ok(OwnerDropStopOutcome::Settled)
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

/// Prepared CAS execution context from canonical provenance.
///
/// A live-tree source manifest is not reachable from a durable CAS root until
/// it is promoted to a project snapshot and published in launch metadata.
/// Keep its original write permit alongside the hash until that publication or
/// the execution's final release, so online GC cannot run in the window.
struct PreparedCasContext {
    effective_path: PathBuf,
    pre_manifest_hash: Option<String>,
    /// Snapshot authority captured into launch metadata for native resume.
    /// A live-fs execution may acquire this only after spawn, when its source
    /// manifest is promoted to a durable resume pin.
    resume_snapshot_hash: Option<String>,
    /// Snapshot that was the verified project HEAD when execution began.
    /// Only `RootPushedHead` owns this authority; a resume-only pin must never
    /// be reused as the expected HEAD for foldback publication.
    head_base_snapshot_hash: Option<String>,
    manifest_publication: Option<super::PendingCasPublication>,
}

/// Prepare CAS execution context from canonical provenance.
fn prepare_cas_context(
    state: &AppState,
    provenance: &ExecutionProvenance,
    thread_id: &str,
    guard: &mut ExecutionGuard,
) -> Result<PreparedCasContext> {
    match provenance {
        ExecutionProvenance::BorrowedChildLiveFs {
            project_path,
            workspace_lifeline,
            ..
        } => {
            if let Some(lifeline) = workspace_lifeline {
                guard.track_temp_dir(lifeline.clone());
            }
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
            Ok(PreparedCasContext {
                effective_path: project_path.clone(),
                pre_manifest_hash: None,
                resume_snapshot_hash: None,
                head_base_snapshot_hash: None,
                manifest_publication: None,
            })
        }
        ExecutionProvenance::BorrowedChildPushedHead {
            effective_path,
            workspace_lifeline,
            ..
        } => {
            guard.track_temp_dir(workspace_lifeline.clone());
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
            Ok(PreparedCasContext {
                effective_path: effective_path.clone(),
                pre_manifest_hash: None,
                resume_snapshot_hash: None,
                head_base_snapshot_hash: None,
                manifest_publication: None,
            })
        }
        ExecutionProvenance::RootLiveFs {
            project_path,
            workspace_lifeline,
            ..
        } => {
            if let Some(lifeline) = workspace_lifeline {
                guard.track_temp_dir(lifeline.clone());
            }
            let pending = super::capture_live_project_manifest(
                state,
                project_path,
                "live_execution_manifest",
            )?;
            let manifest_hash = pending.hash;
            tracing::trace!(
                thread_id = %thread_id,
                effective_path = %project_path.display(),
                "live CAS context prepared"
            );

            Ok(PreparedCasContext {
                effective_path: project_path.clone(),
                pre_manifest_hash: Some(manifest_hash),
                resume_snapshot_hash: None,
                head_base_snapshot_hash: None,
                manifest_publication: Some(pending.publication),
            })
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
            Ok(PreparedCasContext {
                effective_path: effective_path.clone(),
                pre_manifest_hash: Some(manifest_hash),
                resume_snapshot_hash: Some(snapshot_hash.clone()),
                head_base_snapshot_hash: Some(snapshot_hash.clone()),
                manifest_publication: None,
            })
        }
    }
}

fn read_pre_manifest_for_snapshot(state: &AppState, snap_hash: &str) -> Result<String> {
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
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
    pub pre_manifest_hash: &'a str,
    pub head_base_snapshot_hash: &'a str,
    pub project_path: &'a std::path::Path,
    pub execution_dir: Option<&'a std::path::Path>,
    pub completion: &'a ExecutionCompletion,
}

fn post_execution_foldback(params: PostExecutionFoldbackParams<'_>) {
    let PostExecutionFoldbackParams {
        state,
        thread_id: _thread_id,
        acting_principal,
        pre_manifest_hash,
        head_base_snapshot_hash,
        project_path,
        execution_dir,
        completion: _completion,
    } = params;
    let authority = match super::pinned_state_authority(state) {
        Ok(authority) => authority,
        Err(err) => {
            tracing::warn!(error = %err, "failed to pin state authority for fold-back");
            return;
        }
    };
    // Need a working dir for fold-back. If neither an exec checkout
    // nor a LocalPath project_path is available (resume of a non-
    // LocalPath thread), nothing to fold back into.
    let working_dir = execution_dir.unwrap_or(project_path);

    // The shared CAS guard is the outer mutation lock. Keep it live from the
    // first fold-back object write through the signed HEAD publication so GC
    // cannot sweep an unpublished intermediate closure.
    let cas_mutation_guard = match authority.acquire_shared_guard() {
        Ok(guard) => guard,
        Err(error) => {
            tracing::warn!(%error, "cannot acquire pinned CAS mutation guard for fold-back, skipping");
            return;
        }
    };

    // Acquire write barrier for CAS mutations (fold-back + head advance).
    let _permit = match state.write_barrier.try_acquire() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "cannot acquire CAS write permit for fold-back, skipping");
            return;
        }
    };

    // Fold back changes
    let output_manifest_hash = match crate::execution::fold_back_outputs(
        &authority,
        &cas_mutation_guard,
        working_dir,
        pre_manifest_hash,
        &state.ignore_matcher,
    ) {
        Ok(hash) => hash,
        Err(err) => {
            tracing::warn!(error = %err, "fold-back failed");
            None
        }
    };

    // This function is callable only with RootPushedHead's authoritative
    // lineage, so every changed manifest advances that exact LocalPath HEAD.
    if let Some(ref new_manifest_hash) = output_manifest_hash {
        let Some(project_str) = project_path.to_str() else {
            tracing::warn!(
                project_path = %project_path.display(),
                "cannot advance fold-back HEAD for a non-UTF-8 project identity"
            );
            return;
        };
        let project_hash = lillux::cas::sha256_hex(project_str.as_bytes());
        let principal_key = match ryeos_state::refs::principal_storage_key(acting_principal) {
            Ok(principal_key) => principal_key,
            Err(error) => {
                tracing::warn!(%error, "cannot advance fold-back HEAD for invalid principal identity");
                return;
            }
        };
        let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
        let advanced = state.state_store.with_state_db(|db| {
            crate::execution::advance_after_foldback(
                &authority,
                &cas_mutation_guard,
                db,
                &signer,
                principal_key,
                &project_hash,
                new_manifest_hash,
                head_base_snapshot_hash,
            )
        });
        match advanced {
            Ok(_new_snap) => {}
            Err(err) => {
                tracing::warn!(error = %err, "advance_after_foldback failed");
            }
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
/// Returns a pending snapshot publication guard on success, `None` if no
/// pinning was needed (no `native_resume`, or already pinned via a
/// caller-supplied snapshot). The guard owns the source manifest's original
/// write permit and must survive until launch metadata persistence succeeds.
fn pin_localpath_snapshot_if_needed(
    state: &AppState,
    launch_metadata: &mut ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    pre_manifest_hash: &Option<String>,
    _pre_user_manifest_hash: &Option<String>,
    resume_snapshot_hash: &Option<String>,
    manifest_publication: &mut Option<super::PendingCasPublication>,
) -> Result<Option<super::PendingProjectSnapshot>> {
    if launch_metadata.native_resume.is_none() {
        return Ok(None);
    }
    if resume_snapshot_hash.is_some() {
        return Ok(None);
    }
    if launch_metadata.resume_context.is_none() {
        anyhow::bail!("cannot pin native-resume source manifest without durable resume metadata");
    }
    let manifest_hash = pre_manifest_hash.clone().ok_or_else(|| {
        anyhow::anyhow!("cannot pin native-resume launch without a source manifest")
    })?;
    let publication = manifest_publication.take().ok_or_else(|| {
        anyhow::anyhow!("cannot pin native-resume source manifest without its publication permit")
    })?;
    let publication = super::capture_manifest_project_snapshot(
        state,
        manifest_hash,
        "native_resume_pin",
        publication,
    )?;
    launch_metadata
        .resume_context
        .as_mut()
        .expect("resume context checked above")
        .original_snapshot_hash = Some(publication.hash.clone());
    Ok(Some(publication))
}

fn release_manifest_publication(
    publication: Option<super::PendingCasPublication>,
    context: &'static str,
) {
    if let Some(publication) = publication {
        if let Err(error) = publication.publish() {
            tracing::warn!(%error, context, "failed to release staged CAS publication roots");
        }
    }
}

fn release_snapshot_publication(
    publication: Option<super::PendingProjectSnapshot>,
    context: &'static str,
) {
    if let Some(publication) = publication {
        if let Err(error) = publication.publish() {
            tracing::warn!(%error, context, "failed to release staged CAS snapshot roots");
        }
    }
}

/// Attach a freshly-spawned process to the daemon's runtime ledger.
/// On failure, hard-kill the spawned process group and settle the durable
/// lifecycle intent. Explicit Cancel/Kill and daemon shutdown must not be
/// overwritten by a generic attachment failure.
fn attach_or_kill(
    state: &AppState,
    thread_id: &str,
    spawned_pid: u32,
    spawned_pgid: i64,
    process_identity: &ryeos_app::process::ExecutionProcessIdentity,
    launch_metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    failed_outcome_code: &str,
) -> std::result::Result<(), ExecutionCleanupFailure> {
    if let Err(err) = state.threads.attach_process(&ThreadAttachProcessParams {
        thread_id: thread_id.to_string(),
        pid: spawned_pid as i64,
        pgid: spawned_pgid,
        process_identity: Some(process_identity.clone()),
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
        let kill = ryeos_app::process::kill_by_action(
            process_identity,
            ryeos_app::process::ShutdownAction::Hard,
        );
        if !kill.success {
            return Err(ExecutionCleanupFailure {
                operation: "attach process",
                operation_error: err,
                cleanup: Err(anyhow::anyhow!(
                    "identity-verified hard kill did not confirm process-group exit ({})",
                    kill.method
                )),
            });
        }
        if let Err(clear_error) = state
            .state_store
            .clear_thread_process_if_matches(thread_id, process_identity)
        {
            tracing::error!(
                thread_id,
                error = %clear_error,
                "failed to compare-clear process identity after confirmed attach-failure kill"
            );
        }
        return Err(ExecutionCleanupFailure {
            operation: "attach process",
            operation_error: err,
            cleanup: fail_thread_static(state, thread_id, failed_outcome_code),
        });
    }
    Ok(())
}

fn clear_finished_process(
    state: &AppState,
    thread_id: &str,
    process_identity: &ryeos_app::process::ExecutionProcessIdentity,
) {
    match state
        .state_store
        .clear_thread_process_if_matches(thread_id, process_identity)
    {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            thread_id,
            "finished process identity was no longer attached during compare-and-clear"
        ),
        Err(error) => tracing::error!(
            thread_id,
            error = %error,
            "failed to clear finished process identity"
        ),
    }
}

/// Finalize a thread from its completion result.
fn finalize_completion(
    state: &AppState,
    thread_id: &str,
    completion: ExecutionCompletion,
) -> std::result::Result<ThreadDetail, ExecutionCleanupFailure> {
    match state
        .threads
        .finalize_from_completion(thread_id, &completion, None)
    {
        Ok(thread) => Ok(thread),
        Err(err) => {
            tracing::error!(error = %err, "invalid completion during finalization");
            Err(ExecutionCleanupFailure {
                operation: "finalize completion",
                operation_error: err,
                cleanup: fail_thread_static(state, thread_id, "invalid_completion"),
            })
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

struct ProtocolLaunchEnv {
    bindings: Vec<EnvBinding>,
    callback_token: Option<String>,
    thread_auth_token: Option<String>,
    sandbox_daemon_socket_path: Option<PathBuf>,
}

/// Resolve the signed subprocess protocol declared by the item's actual kind.
/// Ordinary runner execution owns only `detached_ok` terminators; managed
/// protocols are launched through the runtime path instead.
fn resolved_terminator_protocol<'a>(
    engine: &'a ryeos_engine::engine::Engine,
    resolved: &ResolvedExecutionRequest,
) -> Result<&'a ryeos_engine::protocols::VerifiedProtocol> {
    let kind = &resolved.resolved_item.kind;
    let schema = engine
        .kinds
        .get(kind)
        .ok_or_else(|| anyhow::anyhow!("execution kind schema not registered: {kind}"))?;
    let terminator = schema
        .execution()
        .and_then(|execution| execution.terminator.as_ref())
        .ok_or_else(|| anyhow::anyhow!("execution kind '{kind}' has no terminator"))?;
    let protocol_ref = match terminator {
        ryeos_engine::kind_registry::TerminatorDecl::Subprocess { protocol_ref } => protocol_ref,
        ryeos_engine::kind_registry::TerminatorDecl::InProcess { .. } => {
            anyhow::bail!("execution kind '{kind}' has an in-process terminator")
        }
    };
    let protocol = engine
        .protocols
        .require(protocol_ref)
        .map_err(|error| anyhow::anyhow!("protocol lookup failed for '{protocol_ref}': {error}"))?;
    crate::dispatch::validate_ordinary_protocol_contract(protocol, kind)
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok(protocol)
}

/// Build only the environment declared by the verified protocol. Callback and
/// thread-auth authority are minted lazily from typed descriptor requirements;
/// callback-free tools therefore receive neither credentials nor daemon-socket
/// sandbox access.
// Execution plumbing: each argument is a distinct leg of the thread's
// auth/provenance context, threaded verbatim — a struct would rename,
// not simplify. Restructure with a compiler in the loop, not here.
#[allow(clippy::too_many_arguments)]
fn build_protocol_launch_env(
    state: &AppState,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    thread_id: &str,
    project_path: &std::path::Path,
    callback_project_path: &std::path::Path,
    duration_seconds: Option<u64>,
    effective_caps: Vec<String>,
    acting_principal: &str,
    caller_scopes: Vec<String>,
    provenance: ExecutionProvenance,
    item_ref: &str,
    root_content_digest: String,
    // Bundle identity for the token, derived once from the resolved canonical
    // ref by the caller via `effective_bundle_id_for_request` so it matches the
    // identity the runtime-cap minter used. `item_ref` stays the requested ref
    // for provenance/display.
    effective_bundle_id: Option<String>,
) -> Result<ProtocolLaunchEnv> {
    let callback_socket_requested = protocol
        .descriptor
        .env_injections
        .iter()
        .any(|injection| injection.source == EnvInjectionSource::CallbackSocketPath);
    let callback_ipc_requested =
        protocol.descriptor.callback_channel != CallbackChannel::None || callback_socket_requested;
    let callback_token_requested = callback_ipc_requested
        || protocol
            .descriptor
            .env_injections
            .iter()
            .any(|injection| matches!(injection.source, EnvInjectionSource::CallbackToken));
    let thread_auth_requested = protocol
        .descriptor
        .env_injections
        .iter()
        .any(|injection| injection.source == EnvInjectionSource::ThreadAuthToken);

    // Complete every fallible non-credential input before registering transient
    // authority so parse/CAS failures cannot leak an untracked token.
    let item_ref = CanonicalRef::parse(item_ref)
        .map_err(|error| anyhow::anyhow!("canonical item ref parse: {error}"))?;
    let authority = super::pinned_state_authority(state)?;
    let cas_root = authority.cas_directory().path().to_path_buf();

    // Run-scoped credentials cover the run's full duration plus finalization.
    let ttl = launch_token_ttl(duration_seconds);
    let callback_token = callback_token_requested.then(|| {
        state
            .callback_tokens
            .generate_with_context(
                thread_id,
                callback_project_path.to_path_buf(),
                ttl,
                effective_caps,
                provenance,
                effective_bundle_id,
                Some(item_ref.to_string()),
                root_content_digest,
                serde_json::Value::Null,
                0,
            )
            .token
    });
    let thread_auth_token = thread_auth_requested.then(|| {
        state
            .thread_auth
            .mint(thread_id, acting_principal.to_string(), caller_scopes, ttl)
            .token
    });

    let sandbox_daemon_socket_path = callback_ipc_requested.then(|| state.config.uds_path.clone());
    let callback_socket_path = if callback_socket_requested {
        Some(
            state
                .config
                .uds_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("callback socket path is not valid UTF-8"))?
                .to_owned(),
        )
    } else {
        None
    };

    let request = SubprocessBuildRequest {
        cmd: PathBuf::new(),
        args: Vec::new(),
        cwd: project_path.to_path_buf(),
        timeout: std::time::Duration::from_secs(0),
        item_ref,
        thread_id: thread_id.to_string(),
        project_path: project_path.to_path_buf(),
        acting_principal: acting_principal.to_string(),
        cas_root,
        callback_token: callback_token.clone(),
        callback_socket_path,
        callback_project_path: Some(callback_project_path.to_path_buf()),
        thread_auth_token: thread_auth_token.clone(),
        params: json!({}),
        resolution_output: None,
    };

    let mut bindings = Vec::with_capacity(protocol.descriptor.env_injections.len());
    for injection in &protocol.descriptor.env_injections {
        let value = match produce_env_value(injection.source, &request) {
            Ok(value) => value,
            Err(error) => {
                if let Some(token) = callback_token.as_deref() {
                    state.callback_tokens.invalidate(token);
                    state.callback_tokens.invalidate_for_thread(thread_id);
                }
                if let Some(token) = thread_auth_token.as_deref() {
                    state.thread_auth.invalidate(token);
                    state.thread_auth.invalidate_for_thread(thread_id);
                }
                return Err(anyhow::anyhow!(
                    "protocol '{}' env injection '{}': {error}",
                    protocol.canonical_ref,
                    injection.name
                ));
            }
        };
        bindings.push(EnvBinding::new(
            injection.name.clone(),
            value,
            EnvSourceDetail::ProtocolInjection {
                source: injection.source,
            },
        ));
    }

    Ok(ProtocolLaunchEnv {
        bindings,
        callback_token,
        thread_auth_token,
        sandbox_daemon_socket_path,
    })
}

fn verify_fresh_root_admission(params: &ExecutionParams) -> Result<()> {
    let Some(admission) = params.resolved.root_admission.as_ref() else {
        // Existing rows and continuation successors inherit the authoritative
        // root policy; they never reinterpret mutable source content here.
        return Ok(());
    };
    let engine = params.provenance.request_engine();
    admission.ensure_matches_request(&params.resolved)?;
    admission.ensure_matches_subject(engine, admission.verified_subject(), &params.resolved.kind)
}

/// Run an execution inline (blocking until completion).
///
/// Handles the full lifecycle: CAS context, snapshot, spawn,
/// fold-back, finalize, cleanup. On any handled error, the thread
/// is finalized as failed and `guard.cleanup()` is invoked
/// explicitly. Panic or future cancellation falls back to the guard's
/// owner-drop stop path, which revokes transient authority and either
/// synchronously kills/settles the owned execution tree or preserves it
/// for an already-active shutdown coordinator.
#[tracing::instrument(
    name = "thread:execute",
    skip(state, params, launch_handoff),
    fields(
        thread_id = tracing::field::Empty,
        item_ref = %params.resolved.item_ref,
    )
)]
pub async fn run_inline(
    state: AppState,
    mut params: ExecutionParams,
    launch_handoff: Option<&super::launch::LaunchHandoff>,
) -> Result<InlineResult> {
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
    guard.track_thread(&created.thread_id);
    if let Some(parent_thread_id) = params.parent_thread_id.as_deref() {
        let inherited_stop = match state.state_store.record_child_link(
            parent_thread_id,
            &created.thread_id,
            "dispatch",
        ) {
            Ok(inherited_stop) => inherited_stop,
            Err(error) => {
                let cleanup = guard
                    .finalize_child_link_failure_if_current(
                    json!({
                        "code": "child_link_failed",
                        "reason": error.to_string(),
                    }),
                )
                    .map_err(|cleanup_error| {
                        anyhow::anyhow!(
                            "record inline child lineage for {parent_thread_id} failed: {error}; conditional cleanup also failed: {cleanup_error:#}"
                        )
                    })?;
                if !cleanup.is_settled() {
                    tracing::warn!(
                        thread_id = %created.thread_id,
                        parent_thread_id,
                        "inline child-link cleanup refused because another launch owner advanced the row"
                    );
                }
                guard.cleanup();
                return Err(anyhow::anyhow!(
                    "record inline child lineage for {parent_thread_id}: {error}"
                ));
            }
        };
        if inherited_stop.is_some() {
            super::process_attachment::finalize_requested_stop_if_present(
                &state,
                &created.thread_id,
            )?;
            guard.mark_finalized();
            guard.cleanup();
            anyhow::bail!("parent {parent_thread_id} was stop-requested before tool launch");
        }
    }
    let running = state
        .threads
        .mark_running(&created.thread_id)
        .inspect_err(|_e| {
            guard.fail_thread("create_failed");
            guard.cleanup();
        })?;
    tracing::Span::current().record("thread_id", running.thread_id.as_str());

    // Prepare CAS context — if this fails, finalize thread as failed
    let PreparedCasContext {
        effective_path,
        pre_manifest_hash,
        resume_snapshot_hash,
        head_base_snapshot_hash,
        mut manifest_publication,
    } = match prepare_cas_context(&state, &params.provenance, &running.thread_id, &mut guard) {
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
    if let Err(error) = verify_fresh_root_admission(&params) {
        guard.fail_thread("history_policy_changed");
        guard.cleanup();
        return Err(error);
    }

    // Spawn — use the per-request engine (pushed_head overlay or
    // daemon startup engine), NOT state.engine directly.
    let engine = params.provenance.request_engine().clone();
    let prepared_plan = thread_lifecycle::prepare_item_plan(&engine, &params.resolved)
        .inspect_err(|_| {
            guard.fail_thread("plan_build_failed");
            guard.cleanup();
        })?;
    let launch_timeout_secs = prepared_plan.timeout_secs;
    let tid = running.thread_id.clone();
    let crid = running.chain_root_id.clone();
    let resolved = params.resolved.clone();
    let vault = params.vault_bindings.clone();

    // Resolve the terminator's signed protocol and materialize exactly its env
    // contract. Callback authority exists only when that descriptor asks for
    // it; the guard owns any credentials that were actually minted.
    let child_provenance = params.provenance.clone_for_borrowed_child();
    // Callback-state project path: the deliberate `state_root` override when
    // requested, otherwise the effective project path. Protocol `project_path`
    // injections remain anchored at `effective_path`; only callback authority
    // moves to the isolated state anchor.
    let runtime_state_root = params
        .provenance
        .state_root_override()
        .unwrap_or(effective_path.as_path())
        .to_path_buf();
    tracing::info!(
        source_root = %effective_path.display(),
        state_root = %runtime_state_root.display(),
        "execution roots resolved"
    );
    let protocol = resolved_terminator_protocol(&engine, &params.resolved).inspect_err(|_| {
        guard.fail_thread("protocol_contract_failed");
        guard.cleanup();
    })?;
    let ProtocolLaunchEnv {
        bindings: protocol_env_bindings,
        callback_token,
        thread_auth_token,
        sandbox_daemon_socket_path,
    } = build_protocol_launch_env(
        &state,
        protocol,
        &tid,
        &effective_path,
        &runtime_state_root,
        Some(launch_timeout_secs),
        params.effective_caps.clone(),
        &params.acting_principal,
        vec!["execute".to_string()],
        child_provenance,
        &params.resolved.item_ref,
        params.resolved.root_raw_content_digest.clone(),
        effective_bundle_id_for_request(&params.resolved),
    )
    .inspect_err(|_| {
        guard.fail_thread("protocol_contract_failed");
        guard.cleanup();
    })?;
    if let Some(token) = callback_token {
        guard.track_callback_token(token);
    }
    if let Some(token) = thread_auth_token {
        guard.track_thread_auth_token(token);
    }

    // Daemon-owned per-thread state dir under config.app_root — does
    // NOT live under the (ephemeral, CAS-checkout) working directory,
    // so checkpoints survive working-dir cleanup and daemon restart.
    // See `launch_metadata::daemon_thread_state_dir`.
    let thread_state_dir =
        ryeos_app::launch_metadata::daemon_thread_state_dir(&state.config.app_root, &tid);
    let inline_snapshot = resume_snapshot_hash.clone();
    let inline_pushed_head_ref =
        ryeos_app::launch_metadata::OriginalPushedHeadRef::from_provenance(&params.provenance);
    let inline_state_root = params
        .provenance
        .state_root_override()
        .map(std::path::Path::to_path_buf);
    let inline_sandbox_project_authority = params.provenance.sandbox_project_authority();
    let inline_roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
        &engine.resolution_roots(Some(effective_path.clone())),
        &state.config.app_root,
    )?;
    let inline_sandbox = state.sandbox.clone();
    let inline_sandbox_daemon_socket_path = sandbox_daemon_socket_path;
    let spawn_workspace_lifeline = guard.temp_dir.clone();
    let spawn_handle = task::spawn_blocking(move || {
        let _spawn_workspace_lifeline = spawn_workspace_lifeline;
        thread_lifecycle::spawn_item(thread_lifecycle::SpawnItemParams {
            engine: &engine,
            resolved: &resolved,
            prepared_plan,
            thread_id: &tid,
            chain_root_id: &crid,
            vault_bindings: vault,
            protocol_env_bindings,
            roots: inline_roots,
            sandbox: inline_sandbox,
            sandbox_project_authority: inline_sandbox_project_authority,
            sandbox_daemon_socket_path: inline_sandbox_daemon_socket_path.as_deref(),
            thread_state_dir: Some(thread_state_dir.as_path()),
            is_resume: false,
            original_snapshot_hash: inline_snapshot.as_deref(),
            original_pushed_head_ref: inline_pushed_head_ref.as_ref(),
            state_root: inline_state_root.as_deref(),
        })
    });

    // The durable row and complete execution request are now owned by the
    // scheduled spawn task. Accepted launch may expose its pre-minted id at
    // this point; spawn/attach/runtime failures remain inspectable on that row.
    if let Some(handoff) = launch_handoff {
        handoff.publish(running.thread_id.clone());
    }

    let mut spawned = match spawn_handle.await {
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
    // LIFECYCLE-INVARIANT: a root live-fs native-resume launch may promote its
    // source manifest to a resume-only snapshot pin, but only RootPushedHead
    // owns authoritative HEAD lineage and may fold back. Borrowed children
    // inherit their parent's execution authority and never pin or fold back.
    let snapshot_publication = if !params.provenance.is_borrowed_child() {
        match pin_localpath_snapshot_if_needed(
            &state,
            &mut spawned.launch_metadata,
            &pre_manifest_hash,
            &None,
            &resume_snapshot_hash,
            &mut manifest_publication,
        ) {
            Ok(Some(publication)) => Some(publication),
            Ok(None) => None,
            Err(err) => {
                tracing::error!(error = %err, "failed to pin LocalPath native_resume snapshot");
                // `SpawnedExecution` owns a fail-safe RunningProcess Drop that
                // terminates and reaps every supervised group. Drop it before
                // terminalizing so the lifecycle never claims a live child is
                // settled merely because a separate signal attempt returned.
                drop(spawned);
                let failure = ExecutionCleanupFailure {
                    operation: "pin LocalPath native-resume snapshot",
                    operation_error: err,
                    cleanup: fail_thread_static(&state, &running.thread_id, "snapshot_pin_failed"),
                };
                if failure.cleanup_disarms_guard() {
                    guard.mark_finalized();
                }
                guard.cleanup();
                return Err(anyhow::Error::new(failure));
            }
        }
    } else {
        None
    };

    // Attach process — on failure, kill the child + finalize.
    if let Err(failure) = attach_or_kill(
        &state,
        &running.thread_id,
        spawned.pid,
        spawned.pgid,
        &spawned.process_identity,
        &spawned.launch_metadata,
        "attach_failed",
    ) {
        if failure.cleanup_disarms_guard() {
            guard.mark_finalized();
        }
        guard.cleanup();
        return Err(anyhow::Error::new(failure));
    }
    release_snapshot_publication(snapshot_publication, "inline launch metadata attachment");

    // Wait
    let wait_workspace_lifeline = guard.temp_dir.clone();
    let waited_identity = spawned.process_identity.clone();
    let completion = match task::spawn_blocking(move || {
        // A cancelled HTTP future must not drop an ephemeral cwd while the
        // blocking wait and its child process are still alive.
        let _wait_workspace_lifeline = wait_workspace_lifeline;
        spawned.wait()
    })
    .await
    {
        Ok(c) => {
            clear_finished_process(&state, &running.thread_id, &waited_identity);
            c
        }
        Err(join_err) => {
            clear_finished_process(&state, &running.thread_id, &waited_identity);
            tracing::error!(error = %join_err, "task panic during inline wait");
            guard.fail_thread("task_panic");
            guard.cleanup();
            return Err(anyhow::anyhow!("wait task panic: {join_err}"));
        }
    };

    if !state.state_store.process_attachment_admission_is_open() {
        let _ = state.state_store.reset_resume_attempts(&running.thread_id);
        release_manifest_publication(
            manifest_publication,
            "inline shutdown without CAS publication",
        );
        guard.cleanup();
        anyhow::bail!("execution interrupted by daemon shutdown; row preserved for recovery");
    }

    // Lift the `--debug-raw` block out of the completion metadata before
    // finalization consumes the completion. `None` on the normal path.
    let debug_block = completion
        .metadata
        .as_ref()
        .and_then(|m| m.get("debug"))
        .cloned();

    if matches!(
        &params.provenance,
        ExecutionProvenance::RootPushedHead { .. }
    ) {
        match (
            pre_manifest_hash.as_deref(),
            head_base_snapshot_hash.as_deref(),
        ) {
            (Some(pre_manifest_hash), Some(head_base_snapshot_hash)) => {
                let guard_exec_dir = guard.temp_dir.as_ref().and_then(|g| g.path());
                post_execution_foldback(PostExecutionFoldbackParams {
                    state: &state,
                    thread_id: &running.thread_id,
                    acting_principal: &params.acting_principal,
                    pre_manifest_hash,
                    head_base_snapshot_hash,
                    project_path: params.provenance.original_project_path(),
                    execution_dir: guard_exec_dir.as_deref(),
                    completion: &completion,
                });
            }
            _ => tracing::error!(
                thread_id = %running.thread_id,
                "RootPushedHead execution lost its authoritative foldback lineage"
            ),
        }
    }
    // A non-resumable live-tree execution has no durable snapshot root. Keep
    // its source manifest protected through execution, then release it here.
    // Native-resume pinning transferred the same permit into
    // `snapshot_publication` and released it after launch-metadata attachment.
    release_manifest_publication(manifest_publication, "inline execution completion");

    // Finalize
    let finalized = match finalize_completion(&state, &running.thread_id, completion) {
        Ok(t) => {
            guard.mark_finalized();
            t
        }
        Err(failure) => {
            if failure.cleanup_disarms_guard() {
                guard.mark_finalized();
            }
            guard.cleanup();
            return Err(anyhow::Error::new(failure));
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
/// pre-spawn synchronous setup uses the same cancellation-safe
/// `ExecutionGuard` discipline as `run_inline`; once ownership is
/// transferred, that guard is disarmed and the deferred
/// `CbTokenGuard` / `TatTokenGuard` inside it cover token revocation
/// on success, error, and panic.
#[tracing::instrument(
    name = "thread:execute",
    skip(state, params, launch_handoff),
    fields(
        thread_id = tracing::field::Empty,
        item_ref = %params.resolved.item_ref,
    )
)]
pub async fn run_detached(
    state: AppState,
    mut params: ExecutionParams,
    launch_handoff: Option<&super::launch::LaunchHandoff>,
) -> Result<DetachedResult> {
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
    guard.track_thread(&created.thread_id);
    if let Some(parent_thread_id) = params.parent_thread_id.as_deref() {
        let inherited_stop = match state.state_store.record_child_link(
            parent_thread_id,
            &created.thread_id,
            "dispatch",
        ) {
            Ok(inherited_stop) => inherited_stop,
            Err(error) => {
                let cleanup = guard
                    .finalize_child_link_failure_if_current(
                    json!({
                        "code": "child_link_failed",
                        "reason": error.to_string(),
                    }),
                )
                    .map_err(|cleanup_error| {
                        anyhow::anyhow!(
                            "record detached child lineage for {parent_thread_id} failed: {error}; conditional cleanup also failed: {cleanup_error:#}"
                        )
                    })?;
                if !cleanup.is_settled() {
                    tracing::warn!(
                        thread_id = %created.thread_id,
                        parent_thread_id,
                        "detached child-link cleanup refused because another launch owner advanced the row"
                    );
                }
                guard.cleanup();
                return Err(anyhow::anyhow!(
                    "record detached child lineage for {parent_thread_id}: {error}"
                ));
            }
        };
        if inherited_stop.is_some() {
            super::process_attachment::finalize_requested_stop_if_present(
                &state,
                &created.thread_id,
            )?;
            guard.mark_finalized();
            guard.cleanup();
            anyhow::bail!("parent {parent_thread_id} was stop-requested before tool launch");
        }
    }
    let running = state
        .threads
        .mark_running(&created.thread_id)
        .inspect_err(|_e| {
            guard.fail_thread("create_failed");
            guard.cleanup();
        })?;
    tracing::Span::current().record("thread_id", running.thread_id.as_str());

    // Prepare CAS context
    let PreparedCasContext {
        effective_path,
        pre_manifest_hash,
        resume_snapshot_hash,
        head_base_snapshot_hash,
        manifest_publication,
    } = match prepare_cas_context(&state, &params.provenance, &running.thread_id, &mut guard) {
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
    if let Err(error) = verify_fresh_root_admission(&params) {
        guard.fail_thread("history_policy_changed");
        guard.cleanup();
        return Err(error);
    }

    // Capture thread details before moving guard
    let running_thread_id = running.thread_id.clone();

    // Build the exact signed protocol env. Any minted credentials transfer to
    // the background task's revocation guards; callback-free protocols mint
    // none and do not receive sandbox access to the daemon socket.
    let child_provenance = params.provenance.clone_for_borrowed_child();
    // Same runtime-state root selection as `run_inline` (see comment there).
    let runtime_state_root = params
        .provenance
        .state_root_override()
        .unwrap_or(effective_path.as_path())
        .to_path_buf();
    tracing::info!(
        source_root = %effective_path.display(),
        state_root = %runtime_state_root.display(),
        "execution roots resolved"
    );
    let engine = params.provenance.request_engine().clone();
    let prepared_plan = thread_lifecycle::prepare_item_plan(&engine, &params.resolved)
        .inspect_err(|_| {
            guard.fail_thread("plan_build_failed");
            guard.cleanup();
        })?;
    let launch_timeout_secs = prepared_plan.timeout_secs;
    let protocol = resolved_terminator_protocol(&engine, &params.resolved).inspect_err(|_| {
        guard.fail_thread("protocol_contract_failed");
        guard.cleanup();
    })?;
    let ProtocolLaunchEnv {
        bindings: protocol_env_bindings,
        callback_token,
        thread_auth_token,
        sandbox_daemon_socket_path,
    } = build_protocol_launch_env(
        &state,
        protocol,
        &running.thread_id,
        &effective_path,
        &runtime_state_root,
        Some(launch_timeout_secs),
        params.effective_caps.clone(),
        &params.acting_principal,
        vec!["execute".to_string()],
        child_provenance,
        &params.resolved.item_ref,
        params.resolved.root_raw_content_digest.clone(),
        effective_bundle_id_for_request(&params.resolved),
    )
    .inspect_err(|_| {
        guard.fail_thread("protocol_contract_failed");
        guard.cleanup();
    })?;
    if let Some(token) = callback_token {
        guard.track_callback_token(token);
    }
    if let Some(token) = thread_auth_token {
        guard.track_thread_auth_token(token);
    }

    // Move guard parts into the background task
    let parts = guard.into_detached_parts();
    let bg_state = parts.state;
    let bg_temp_dir = parts.temp_dir;
    let bg_cb_token = parts.callback_token;
    let bg_tat_token = parts.thread_auth_token;
    let bg_thread_id = running.thread_id.clone();
    let bg_chain_root_id = running.chain_root_id.clone();
    let bg_resolved = params.resolved.clone();
    let bg_prepared_plan = prepared_plan;
    // Per-request engine (pushed_head overlay or daemon startup engine).
    let bg_engine = engine;
    let bg_vault = params.vault_bindings.clone();
    let bg_protocol_env_bindings = protocol_env_bindings;
    let bg_acting_principal = params.acting_principal.clone();
    let bg_pre_manifest_hash = pre_manifest_hash;
    let bg_resume_snapshot_hash = resume_snapshot_hash;
    let bg_head_base_snapshot_hash = head_base_snapshot_hash;
    let bg_manifest_publication = manifest_publication;
    let bg_project_path = Some(params.provenance.original_project_path().to_path_buf());
    let bg_skip_resume_snapshot_pin = params.provenance.is_borrowed_child();
    let bg_owns_pushed_head_lineage = matches!(
        &params.provenance,
        ExecutionProvenance::RootPushedHead { .. }
    );
    let bg_pushed_head_ref =
        ryeos_app::launch_metadata::OriginalPushedHeadRef::from_provenance(&params.provenance);
    let bg_state_root = params
        .provenance
        .state_root_override()
        .map(std::path::Path::to_path_buf);
    let bg_sandbox_project_authority = params.provenance.sandbox_project_authority();
    let bg_runtime_state_dir = state.config.app_root.clone();

    tokio::spawn(dispatch_detached_bg_task(
        bg_state,
        bg_thread_id,
        bg_chain_root_id,
        bg_resolved,
        bg_prepared_plan,
        bg_engine,
        bg_vault,
        bg_protocol_env_bindings,
        bg_acting_principal,
        bg_pre_manifest_hash,
        bg_resume_snapshot_hash,
        bg_head_base_snapshot_hash,
        bg_manifest_publication,
        bg_project_path,
        bg_pushed_head_ref,
        bg_state_root,
        bg_sandbox_project_authority,
        sandbox_daemon_socket_path,
        bg_temp_dir,
        bg_skip_resume_snapshot_pin,
        bg_owns_pushed_head_lineage,
        bg_runtime_state_dir,
        false, // is_resume
        None,  // prior_status_for_mark_running
        bg_cb_token,
        bg_tat_token,
        None,
    ));

    // Every execution input and cleanup guard is now owned by the scheduled
    // detached task. This is the terminal-subprocess acknowledgement boundary.
    if let Some(handoff) = launch_handoff {
        handoff.publish(running_thread_id.clone());
    }

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
        bg_state, bg_chain_root_id, bg_resolved, bg_prepared_plan, bg_engine, bg_vault,
        bg_protocol_env_bindings, bg_acting_principal, bg_pre_manifest_hash,
        bg_resume_snapshot_hash, bg_head_base_snapshot_hash, bg_manifest_publication,
        bg_project_path, bg_original_pushed_head_ref, bg_state_root,
        bg_sandbox_project_authority, bg_sandbox_daemon_socket_path, bg_temp_dir,
        bg_skip_resume_snapshot_pin, bg_owns_pushed_head_lineage, bg_runtime_state_dir,
        prior_status_for_mark_running,
        bg_cb_token, bg_tat_token, launch_claim
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
    bg_prepared_plan: thread_lifecycle::PreparedItemPlan,
    bg_engine: std::sync::Arc<ryeos_engine::engine::Engine>,
    bg_vault: HashMap<String, String>,
    bg_protocol_env_bindings: Vec<EnvBinding>,
    bg_acting_principal: String,
    bg_pre_manifest_hash: Option<String>,
    bg_resume_snapshot_hash: Option<String>,
    bg_head_base_snapshot_hash: Option<String>,
    mut bg_manifest_publication: Option<super::PendingCasPublication>,
    bg_project_path: Option<PathBuf>,
    bg_original_pushed_head_ref: Option<ryeos_app::launch_metadata::OriginalPushedHeadRef>,
    bg_state_root: Option<PathBuf>,
    bg_sandbox_project_authority: ryeos_engine::sandbox::SandboxProjectAuthority,
    bg_sandbox_daemon_socket_path: Option<PathBuf>,
    mut bg_temp_dir: Option<Arc<TempDirGuard>>,
    bg_skip_resume_snapshot_pin: bool,
    bg_owns_pushed_head_lineage: bool,
    bg_runtime_state_dir: PathBuf,
    is_resume: bool,
    prior_status_for_mark_running: Option<String>,
    bg_cb_token: Option<String>,
    bg_tat_token: Option<String>,
    launch_claim: Option<ThreadLaunchClaim>,
) {
    // Keep recovery's durable spawn authorization alive through spawn, attach,
    // running, wait, and failure/finalization. Every early return drops it; a
    // completed task releases it at the function boundary.
    let _launch_claim_guard = launch_claim;
    // Revoke every protocol-requested credential on every exit path. A
    // callback-free protocol passes `None` and installs inert guards.
    let _cb_guard = defer_cb_token_revocation(&bg_state, &bg_thread_id, &bg_cb_token);
    let _tat_guard = defer_tat_token_revocation(&bg_state, &bg_thread_id, &bg_tat_token);
    if is_resume && !ryeos_app::recovery_execution_gate::wait_if_armed().await {
        return;
    }

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
    let protocol_env_for_spawn = bg_protocol_env_bindings;
    let snap_for_spawn = bg_resume_snapshot_hash.clone();
    let pushed_head_ref_for_spawn = bg_original_pushed_head_ref;
    let state_root_for_spawn = bg_state_root;
    let sandbox_for_spawn = bg_state.sandbox.clone();
    let sandbox_daemon_socket_path_for_spawn = bg_sandbox_daemon_socket_path;
    let spawn_workspace_lifeline = bg_temp_dir.clone();

    let spawn_result = task::spawn_blocking(move || {
        let _spawn_workspace_lifeline = spawn_workspace_lifeline;
        let project_root = match &res_for_spawn.plan_context.project_context {
            ryeos_engine::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
            _ => None,
        };
        let roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
            &eng_for_spawn.resolution_roots(project_root),
            &bg_runtime_state_dir,
        )?;
        thread_lifecycle::spawn_item(thread_lifecycle::SpawnItemParams {
            engine: &eng_for_spawn,
            resolved: &res_for_spawn,
            prepared_plan: bg_prepared_plan,
            thread_id: &tid_for_spawn,
            chain_root_id: &crid_for_spawn,
            vault_bindings: vault_for_spawn,
            protocol_env_bindings: protocol_env_for_spawn,
            roots,
            sandbox: sandbox_for_spawn,
            sandbox_project_authority: bg_sandbox_project_authority,
            sandbox_daemon_socket_path: sandbox_daemon_socket_path_for_spawn.as_deref(),
            thread_state_dir: Some(thread_state_dir.as_path()),
            is_resume,
            original_snapshot_hash: snap_for_spawn.as_deref(),
            original_pushed_head_ref: pushed_head_ref_for_spawn.as_ref(),
            state_root: state_root_for_spawn.as_deref(),
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
            if let Err(cleanup_error) = fail_thread_static(&bg_state, &bg_thread_id, "engine_error")
            {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %cleanup_error,
                    "engine spawn and terminal cleanup both failed"
                );
            }
            drop(bg_temp_dir.take());
            return;
        }
        Err(join_err) => {
            tracing::error!(
                phase = log_phase,
                error = %join_err,
                "task panic during spawn"
            );
            if let Err(cleanup_error) = fail_thread_static(&bg_state, &bg_thread_id, "task_panic") {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %cleanup_error,
                    "spawn-task panic and terminal cleanup both failed"
                );
            }
            drop(bg_temp_dir.take());
            return;
        }
    };

    // Pin LocalPath native_resume to a snapshot before attach.
    // pre_user_manifest_hash is intentionally None for the same
    // reason as the inline-spawn path: LocalPath runs against the
    // daemon's live app root, not a captured snapshot.
    let snapshot_publication = if !bg_skip_resume_snapshot_pin {
        match pin_localpath_snapshot_if_needed(
            &bg_state,
            &mut spawned.launch_metadata,
            &bg_pre_manifest_hash,
            &None,
            &bg_resume_snapshot_hash,
            &mut bg_manifest_publication,
        ) {
            Ok(Some(publication)) => Some(publication),
            Ok(None) => None,
            Err(err) => {
                tracing::error!(
                    phase = log_phase,
                    error = %err,
                    "failed to pin LocalPath native_resume snapshot"
                );
                // Explicit drop invokes the supervised process handle's
                // terminate-and-reap fallback before lifecycle settlement.
                drop(spawned);
                let cleanup = fail_thread_static(&bg_state, &bg_thread_id, "snapshot_pin_failed");
                if let Err(cleanup_error) = cleanup {
                    tracing::error!(
                        phase = log_phase,
                        thread_id = %bg_thread_id,
                        error = %cleanup_error,
                        "snapshot pin failure cleanup did not settle"
                    );
                }
                drop(bg_temp_dir.take());
                return;
            }
        }
    } else {
        None
    };

    // Attach process — on failure, kill the child + finalize.
    if let Err(failure) = attach_or_kill(
        &bg_state,
        &bg_thread_id,
        spawned.pid,
        spawned.pgid,
        &spawned.process_identity,
        &spawned.launch_metadata,
        attach_outcome_code,
    ) {
        tracing::error!(
            phase = log_phase,
            thread_id = %bg_thread_id,
            error = %failure,
            "{}: attach cleanup failed or settled with the reported outcome",
            attach_outcome_code,
        );
        drop(bg_temp_dir.take());
        return;
    }
    release_snapshot_publication(snapshot_publication, "detached launch metadata attachment");

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

    let wait_workspace_lifeline = bg_temp_dir.clone();
    let waited_identity = spawned.process_identity.clone();
    let wait_result = task::spawn_blocking(move || {
        let _wait_workspace_lifeline = wait_workspace_lifeline;
        spawned.wait()
    })
    .await;
    clear_finished_process(&bg_state, &bg_thread_id, &waited_identity);
    if !bg_state.state_store.process_attachment_admission_is_open() {
        let _ = bg_state.state_store.reset_resume_attempts(&bg_thread_id);
        release_manifest_publication(
            bg_manifest_publication,
            "detached shutdown without CAS publication",
        );
        drop(bg_temp_dir);
        tracing::info!(
            phase = log_phase,
            thread_id = %bg_thread_id,
            "preserving execution row after shutdown-owned interruption"
        );
        return;
    }
    // Extract the execution dir path while the Arc is still alive.
    let bg_exec_dir_path = bg_temp_dir.as_ref().and_then(|g| g.path());
    match wait_result {
        Ok(completion) => {
            if bg_owns_pushed_head_lineage {
                match (
                    bg_pre_manifest_hash.as_deref(),
                    bg_head_base_snapshot_hash.as_deref(),
                    bg_project_path.as_deref(),
                ) {
                    (
                        Some(pre_manifest_hash),
                        Some(head_base_snapshot_hash),
                        Some(project_path),
                    ) => {
                        post_execution_foldback(PostExecutionFoldbackParams {
                            state: &bg_state,
                            thread_id: &bg_thread_id,
                            acting_principal: &bg_acting_principal,
                            pre_manifest_hash,
                            head_base_snapshot_hash,
                            project_path,
                            execution_dir: bg_exec_dir_path.as_deref(),
                            completion: &completion,
                        });
                    }
                    _ => tracing::error!(
                        thread_id = %bg_thread_id,
                        "RootPushedHead execution lost its authoritative foldback lineage"
                    ),
                }
            }
            if let Err(err) = finalize_completion(&bg_state, &bg_thread_id, completion) {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %err,
                    "completion finalization failed; terminal cleanup outcome is included"
                );
            }
        }
        Err(join_err) => {
            tracing::error!(
                phase = log_phase,
                error = %join_err,
                "task panic during wait"
            );
            if let Err(cleanup_error) = fail_thread_static(&bg_state, &bg_thread_id, "task_panic") {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %cleanup_error,
                    "wait-task panic and terminal cleanup both failed"
                );
            }
        }
    }
    // If no resumable snapshot was published, retain the live source manifest
    // through execution and release its publication permit now.
    release_manifest_publication(bg_manifest_publication, "detached execution completion");

    // Drop the Arc<TempDirGuard>. If this is the last holder, the
    // directory is removed by the TempDirGuard Drop impl.
    drop(bg_temp_dir);
}

/// Fail a thread without a guard (for use inside detached tasks).
/// Note: callback token revocation is handled by CbTokenGuard::drop.
fn fail_thread_static(
    state: &AppState,
    thread_id: &str,
    outcome_code: &str,
) -> Result<ExecutionCleanupOutcome> {
    match super::process_attachment::finalize_requested_stop_if_present(state, thread_id) {
        Ok(true) => return Ok(ExecutionCleanupOutcome::DurableStopSettled),
        Ok(false) => {}
        Err(error) => {
            return Err(error.context("settle durable stop before static failure finalization"))
        }
    }
    if !state.state_store.process_attachment_admission_is_open() {
        state.state_store.reset_resume_attempts(thread_id)?;
        tracing::info!(
            thread_id,
            "preserving execution row after shutdown-owned interruption"
        );
        return Ok(ExecutionCleanupOutcome::PreservedForShutdown);
    }
    let finalize = state.threads.finalize_thread(&ThreadFinalizeParams {
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
    match finalize {
        Ok(_) => Ok(ExecutionCleanupOutcome::Finalized),
        Err(finalize_error) => {
            let terminal = state
                .threads
                .get_thread(thread_id)?
                .is_some_and(|thread| is_terminal_status(&thread.status));
            if terminal {
                Ok(ExecutionCleanupOutcome::AlreadyTerminal)
            } else {
                Err(finalize_error.context("persist static terminal cleanup"))
            }
        }
    }
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
/// Credential-bearing protocols scope each fresh token to the background task;
/// callback-free protocols install this guard with `None`.
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

/// Provenance policy for a resume, decided purely from the persisted
/// record so it is unit-testable without an `AppState`.
#[derive(Debug)]
enum ResumeProvenanceDecision<'a> {
    /// Original spawn was a pushed-head root: rebuild the pinned
    /// checkout + snapshot-scoped overlay engine and resume under
    /// `root_pushed_head`.
    PinnedPushedHead(&'a ryeos_app::launch_metadata::OriginalPushedHeadRef),
    /// A live-fs native-resume spawn was pinned before attach. Rebuild a fresh
    /// daemon-owned checkout, but retain live-fs lineage semantics (no pushed
    /// HEAD ownership/foldback).
    PinnedLocalSnapshot {
        snapshot_hash: &'a str,
        original_path: &'a std::path::Path,
    },
    /// Original spawn ran against the live tree: resume against the
    /// live tree and the daemon's current engine.
    LiveFs(&'a std::path::Path),
    /// The record's project_context carries no working tree and no
    /// pushed-head identity was captured: the overlay engine cannot be
    /// rebuilt. Refuse — never silently fall back to the live tree.
    MissingPushedHeadRef(&'a ProjectContext),
}

fn decide_resume_provenance(resume: &ResumeContext) -> ResumeProvenanceDecision<'_> {
    match (
        &resume.original_pushed_head_ref,
        &resume.original_snapshot_hash,
        &resume.project_context,
    ) {
        (Some(pinned), _, _) => ResumeProvenanceDecision::PinnedPushedHead(pinned),
        (None, Some(snapshot_hash), ProjectContext::LocalPath { path }) => {
            ResumeProvenanceDecision::PinnedLocalSnapshot {
                snapshot_hash,
                original_path: path,
            }
        }
        (None, None, ProjectContext::LocalPath { path }) => ResumeProvenanceDecision::LiveFs(path),
        (None, _, other) => ResumeProvenanceDecision::MissingPushedHeadRef(other),
    }
}

fn execution_provenance_from_resume_context(
    state: &AppState,
    resume: &ResumeContext,
) -> Result<(ExecutionProvenance, ProjectContext)> {
    match decide_resume_provenance(resume) {
        ResumeProvenanceDecision::PinnedPushedHead(pinned) => {
            let checkout_id = format!(
                "resume-{}-{:08x}",
                lillux::time::timestamp_millis(),
                rand::random::<u32>()
            );
            let ctx = super::project_source::resolve_pinned_snapshot_context(
                state,
                &pinned.snapshot_hash,
                pinned.original_project_path.clone(),
                &checkout_id,
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "resume: pinned snapshot {} could not be rebuilt for {}: {e}",
                    pinned.snapshot_hash,
                    resume.item_ref,
                )
            })?;
            let lifeline = ctx.temp_dir.clone().expect(
                "resolve_pinned_snapshot_context must return a request-owned checkout guard",
            );
            let effective_path = ctx.effective_path.clone();
            let provenance = ExecutionProvenance::root_pushed_head(
                effective_path.clone(),
                pinned.original_project_path.clone(),
                ctx.request_engine,
                lifeline,
                pinned.snapshot_hash.clone(),
            );
            tracing::info!(
                snapshot_hash = %pinned.snapshot_hash,
                effective_path = %effective_path.display(),
                "resume: rebuilt pushed-head checkout + overlay engine"
            );
            Ok((
                provenance,
                ProjectContext::LocalPath {
                    path: effective_path,
                },
            ))
        }
        ResumeProvenanceDecision::PinnedLocalSnapshot {
            snapshot_hash,
            original_path,
        } => {
            let checkout_id = format!(
                "resume-local-{}-{:08x}",
                lillux::time::timestamp_millis(),
                rand::random::<u32>()
            );
            let ctx = super::project_source::resolve_pinned_snapshot_context(
                state,
                snapshot_hash,
                original_path.to_path_buf(),
                &checkout_id,
            )
            .map_err(|error| {
                anyhow::anyhow!(
                    "resume: pinned local snapshot {snapshot_hash} could not be rebuilt for {}: {error}",
                    resume.item_ref,
                )
            })?;
            let lifeline = ctx.temp_dir.clone().expect(
                "resolve_pinned_snapshot_context must return a request-owned checkout guard",
            );
            let effective_path = ctx.effective_path.clone();
            let provenance =
                ExecutionProvenance::root_live_fs(effective_path.clone(), ctx.request_engine)
                    .with_workspace_lifeline(Some(lifeline))
                    .with_state_root(resume.state_root.clone());
            tracing::info!(
                snapshot_hash,
                effective_path = %effective_path.display(),
                "resume: rebuilt pinned local snapshot as a daemon runtime workspace"
            );
            Ok((
                provenance,
                ProjectContext::LocalPath {
                    path: effective_path,
                },
            ))
        }
        // LocalPath resume: live tree + the daemon's CURRENT engine, by
        // intent — the original spawn also resolved against the
        // then-current live engine. Accepted residual drift: an install-
        // generation change between spawn and resume means item/bundle
        // resolution here can differ from what the pre-crash run saw.
        // The persisted state-root override is re-applied so a resumed
        // overridden run keeps its state/callback anchor (the freshly
        // minted token must match what the runtime advertises).
        ResumeProvenanceDecision::LiveFs(path) => Ok((
            ExecutionProvenance::root_live_fs(path.to_path_buf(), state.engine.clone())
                .with_state_root(resume.state_root.clone()),
            resume.project_context.clone(),
        )),
        ResumeProvenanceDecision::MissingPushedHeadRef(other) => {
            anyhow::bail!(
                "resume: record for {} has project_context {other:?} but no \
                 original_pushed_head_ref, so the snapshot-scoped overlay engine \
                 cannot be rebuilt; refusing to resume against the live tree. \
                 Re-spawn the thread from its original pushed head instead.",
                resume.item_ref,
            );
        }
    }
}

/// Reconstruct a created root from its exact, already-admitted authority.
///
/// Unlike ordinary crash resume, this path must not resolve an item ref or
/// re-run admission: the slot and launch metadata were durably committed only
/// after the complete verified request was sealed. The resume context remains
/// the independently persisted launch envelope identity and parent authority;
/// every overlapping field must agree before the request can be used.
pub(crate) fn execution_params_from_sealed_root_request(
    state: &AppState,
    resume: &ResumeContext,
    sealed: &SealedRootExecutionRequest,
    provenance_override: Option<ExecutionProvenance>,
) -> Result<ExecutionParams> {
    let provenance = match provenance_override {
        Some(provenance) => provenance,
        None => execution_provenance_from_resume_context(state, resume)?.0,
    };
    let resolved = sealed.restore(provenance.request_engine())?;
    let acting_principal = resume.principal_identifier().to_string();

    if resolved.kind != resume.kind
        || resolved.item_ref != resume.item_ref
        || resolved.launch_mode != resume.launch_mode
        || resolved.parameters != resume.parameters
        || resolved.ref_bindings != resume.ref_bindings
        || resolved.current_site_id != resume.current_site_id
        || resolved.origin_site_id != resume.origin_site_id
        || resolved.requested_by.as_deref() != Some(acting_principal.as_str())
        || resolved.plan_context.requested_by != resume.requested_by
        || resolved.plan_context.project_context != resume.project_context
        || resolved.plan_context.execution_hints != resume.execution_hints
        || resume.executor_ref.as_deref() != Some(sealed.executor_ref())
        || resume.runtime_ref.as_deref() != Some(sealed.runtime_ref())
    {
        anyhow::bail!(
            "created-root launch identity does not match its sealed execution request for {}",
            resume.item_ref
        );
    }

    Ok(ExecutionParams {
        parameters: resolved.parameters.clone(),
        resolved,
        acting_principal,
        vault_bindings: HashMap::new(),
        pre_minted_thread_id: None,
        effective_caps: resume.effective_caps.clone(),
        provenance,
        runtime_ref: Some(sealed.runtime_ref().to_string()),
        // The created row already carries any operational parent link. This
        // reconstruction must not try to attach it a second time at launch.
        parent_thread_id: None,
    })
}

/// Build `ExecutionParams` from a captured `ResumeContext`.
///
/// Provenance is selected by original spawn type BEFORE resolution, so
/// a pushed-head resume resolves items/bundles against the pinned
/// snapshot's overlay engine — not the daemon's live engine. See
/// `decide_resume_provenance` and
/// `docs/future/native-resume-snapshot-pinning.md`.
#[tracing::instrument(
    name = "thread:resume_params",
    skip(state, resume),
    fields(
        item_ref = %resume.item_ref,
        kind = %resume.kind,
        snapshot_pinned = resume.original_snapshot_hash.is_some(),
        pushed_head_pinned = resume.original_pushed_head_ref.is_some(),
    )
)]
pub fn execution_params_from_resume_context(
    state: &AppState,
    resume: &ResumeContext,
) -> Result<ExecutionParams> {
    let (provenance, project_context) = execution_provenance_from_resume_context(state, resume)?;

    let plan_ctx = PlanContext {
        requested_by: resume.requested_by.clone(),
        project_context,
        current_site_id: resume.current_site_id.clone(),
        origin_site_id: resume.origin_site_id.clone(),
        execution_hints: resume.execution_hints.clone(),
        validate_only: false,
    };

    // All resolution below goes through the provenance-selected engine
    // (overlay for pushed-head, daemon engine for live-fs).
    let engine = provenance.request_engine();

    let canonical = CanonicalRef::parse(&resume.item_ref)
        .map_err(|e| anyhow::anyhow!("resume: invalid item ref {}: {e}", resume.item_ref))?;

    let resolved_item = engine
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
        let runtime = engine
            .runtimes
            .resolve_for_launch(Some(rr), &resolved_item.kind)
            .map_err(|e| anyhow::anyhow!("resume: {e}"))?;
        let bare = crate::dispatch::strip_binary_ref_prefix(&runtime.yaml.binary_ref)?;
        format!("native:{bare}")
    } else if let Some(eid) = resolved_item.metadata.executor_id.clone() {
        eid
    } else {
        let runtime = engine
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

    let acting_principal = resume.principal_identifier().to_string();
    let resolved = ResolvedExecutionRequest {
        kind: resume.kind.clone(),
        item_ref: resume.item_ref.clone(),
        executor_ref,
        launch_mode: resume.launch_mode.clone(),
        current_site_id: resume.current_site_id.clone(),
        origin_site_id: resume.origin_site_id.clone(),
        target_site_id: None,
        requested_by: Some(acting_principal.clone()),
        usage_subject: None,
        usage_subject_asserted_by: None,
        parameters: resume.parameters.clone(),
        root_raw_content_digest: resolved_item.raw_content_digest.clone(),
        ref_bindings: resume.ref_bindings.clone(),
        resolved_item,
        plan_context: plan_ctx,
        // The row already exists and its chain root owns the immutable
        // captured policy. A resume must not reinterpret mutable item/config
        // content as fresh destructive authority.
        root_admission: None,
    };

    // NOTE: read_required_secrets and envelope-field preflight are NOT
    // called here. They run later, inside run_existing_detached(), AFTER
    // prepare_cas_context() returns the effective_path — so dotenv overlay
    // and launch-contract configuration see the snapshot checkout, not the live tree.

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
        // Operational lineage was persisted by the original launch and is not
        // recreated during recovery.
        parent_thread_id: None,
    })
}

/// Re-spawn an existing thread under its original `thread_id` after a
/// daemon restart. Used by the reconciler's auto-resume path.
///
/// Mirrors `run_detached` from spawn onward but does NOT call
/// `create_root_thread` — the thread row already exists from the
/// pre-crash spawn. Returns `Enqueued` only after an owned SQLite launch claim
/// has been transferred into the background task; duplicate/settled work is a
/// classified `Skipped`.
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
) -> Result<RecoveryLaunchOutcome, ResumeError> {
    // Claim before any fallible pre-spawn work. The claim is moved into the
    // detached background task below, making a successful return a durable
    // claimed-and-enqueued boundary rather than an in-memory scheduling hint.
    let resume_claim = match ThreadLaunchClaim::acquire(&state, &thread_id)? {
        ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
        ThreadLaunchClaimOutcome::AlreadyClaimed => {
            return Ok(RecoveryLaunchOutcome::Skipped("already_claimed"));
        }
    };
    let thread = state.threads.get_thread(&thread_id)?.ok_or_else(|| {
        ResumeError::Other(anyhow::anyhow!(
            "resume: thread not found after claiming launch: {thread_id}"
        ))
    })?;
    if thread.chain_root_id != chain_root_id {
        return Err(ResumeError::Other(anyhow::anyhow!(
            "resume: thread {thread_id} belongs to chain {} rather than requested chain {chain_root_id}",
            thread.chain_root_id
        )));
    }
    if ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
        .is_some_and(|status| status.is_terminal())
    {
        return Ok(RecoveryLaunchOutcome::Skipped("terminal"));
    }
    // Process attach precedes the `created -> running` transition, so liveness
    // is checked for every nonterminal status. A duplicate recovery must never
    // spawn beside an already-attached tool subprocess.
    if thread
        .runtime
        .pgid
        .is_some_and(ryeos_app::process::pgid_alive)
    {
        return Ok(RecoveryLaunchOutcome::Skipped("live_process"));
    }
    let mut guard = ExecutionGuard::new(state.clone());
    guard.track_thread(&thread_id);

    // Prepare CAS context.
    let PreparedCasContext {
        effective_path,
        pre_manifest_hash,
        resume_snapshot_hash,
        head_base_snapshot_hash,
        manifest_publication,
    } = match prepare_cas_context(&state, &params.provenance, &thread_id, &mut guard) {
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

    // ── Vault preflight (post-CAS) ──────────────────────────────────
    // Run after prepare_cas_context so dotenv overlay follows the exact
    // materialized project used by this spawn.
    {
        let secret_requirements = crate::execution::launch::build_secret_requirements(
            &params.resolved.resolved_item.metadata.required_secrets,
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

    // Rebuild the signed protocol environment for the resumed subprocess.
    // Credentials are fresh when declared and absent for callback-free tools;
    // originals, if any, were revoked with the prior background owner.
    let child_provenance = params.provenance.clone_for_borrowed_child();
    // Same runtime-state root selection as `run_inline`. The override is
    // persisted on the resume context and re-applied by
    // `execution_params_from_resume_context`, so a resumed overridden run
    // keeps its state/callback anchor.
    let runtime_state_root = params
        .provenance
        .state_root_override()
        .unwrap_or(effective_path.as_path())
        .to_path_buf();
    let engine = params.provenance.request_engine().clone();
    let prepared_plan = thread_lifecycle::prepare_item_plan(&engine, &params.resolved)
        .inspect_err(|_| {
            guard.fail_thread("plan_build_failed");
            guard.cleanup();
        })?;
    let launch_timeout_secs = prepared_plan.timeout_secs;
    let protocol = resolved_terminator_protocol(&engine, &params.resolved).inspect_err(|_| {
        guard.fail_thread("protocol_contract_failed");
        guard.cleanup();
    })?;
    let ProtocolLaunchEnv {
        bindings: protocol_env_bindings,
        callback_token,
        thread_auth_token,
        sandbox_daemon_socket_path,
    } = build_protocol_launch_env(
        &state,
        protocol,
        &thread_id,
        &effective_path,
        &runtime_state_root,
        Some(launch_timeout_secs),
        params.effective_caps.clone(),
        &params.acting_principal,
        vec!["execute".to_string()],
        child_provenance,
        &params.resolved.item_ref,
        params.resolved.root_raw_content_digest.clone(),
        effective_bundle_id_for_request(&params.resolved),
    )
    .inspect_err(|_| {
        guard.fail_thread("protocol_contract_failed");
        guard.cleanup();
    })?;
    if let Some(token) = callback_token {
        guard.track_callback_token(token);
    }
    if let Some(token) = thread_auth_token {
        guard.track_thread_auth_token(token);
    }

    let parts = guard.into_detached_parts();
    let bg_state = parts.state;
    let bg_temp_dir = parts.temp_dir;
    let bg_cb_token = parts.callback_token;
    let bg_tat_token = parts.thread_auth_token;
    let bg_thread_id = thread_id.clone();
    let bg_chain_root_id = chain_root_id.clone();
    let bg_resolved = params.resolved.clone();
    let bg_prepared_plan = prepared_plan;
    // Per-request engine (overlay engine for a pushed-head resume,
    // daemon engine for live-fs — selected in
    // execution_params_from_resume_context).
    let bg_engine = engine;
    let bg_vault = params.vault_bindings.clone();
    let bg_protocol_env_bindings = protocol_env_bindings;
    let bg_acting_principal = params.acting_principal.clone();
    let bg_pre_manifest_hash = pre_manifest_hash;
    let bg_resume_snapshot_hash = resume_snapshot_hash;
    let bg_head_base_snapshot_hash = head_base_snapshot_hash;
    let bg_manifest_publication = manifest_publication;
    let bg_project_path = Some(params.provenance.original_project_path().to_path_buf());
    let bg_skip_resume_snapshot_pin = params.provenance.is_borrowed_child();
    let bg_owns_pushed_head_lineage = matches!(
        &params.provenance,
        ExecutionProvenance::RootPushedHead { .. }
    );
    let bg_pushed_head_ref =
        ryeos_app::launch_metadata::OriginalPushedHeadRef::from_provenance(&params.provenance);
    let bg_state_root = params
        .provenance
        .state_root_override()
        .map(std::path::Path::to_path_buf);
    let bg_sandbox_project_authority = params.provenance.sandbox_project_authority();
    let bg_runtime_state_dir = state.config.app_root.clone();

    tokio::spawn(dispatch_detached_bg_task(
        bg_state,
        bg_thread_id,
        bg_chain_root_id,
        bg_resolved,
        bg_prepared_plan,
        bg_engine,
        bg_vault,
        bg_protocol_env_bindings,
        bg_acting_principal,
        bg_pre_manifest_hash,
        bg_resume_snapshot_hash,
        bg_head_base_snapshot_hash,
        bg_manifest_publication,
        bg_project_path,
        bg_pushed_head_ref,
        bg_state_root,
        bg_sandbox_project_authority,
        sandbox_daemon_socket_path,
        bg_temp_dir,
        bg_skip_resume_snapshot_pin,
        bg_owns_pushed_head_lineage,
        bg_runtime_state_dir,
        true, // is_resume
        Some(prior_status),
        bg_cb_token,
        bg_tat_token,
        Some(resume_claim),
    ));

    Ok(RecoveryLaunchOutcome::Enqueued)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::launch_metadata::OriginalPushedHeadRef;
    use ryeos_engine::contracts::{EffectivePrincipal, ExecutionHints, Principal};

    fn resume_record(
        project_context: ProjectContext,
        pushed: Option<OriginalPushedHeadRef>,
    ) -> ResumeContext {
        ResumeContext {
            kind: "graph".into(),
            item_ref: "graph:test/item".into(),
            ref_bindings: std::collections::BTreeMap::new(),
            launch_mode: "detached".into(),
            parameters: json!({}),
            project_context,
            original_snapshot_hash: None,
            original_pushed_head_ref: pushed,
            state_root: None,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            execution_hints: ExecutionHints::default(),
            effective_caps: Vec::new(),
            executor_ref: None,
            runtime_ref: None,
        }
    }

    #[test]
    fn pushed_head_record_selects_pinned_rebuild_over_stale_checkout_path() {
        // Realistic pushed-head record shape: project_context is the
        // (ephemeral, long-gone) LocalPath checkout the spawn ran in,
        // and the pin identity lives in `original_pushed_head_ref`. The
        // pin must win — resolution must never target the stale path.
        let resume = resume_record(
            ProjectContext::LocalPath {
                path: PathBuf::from("/var/cache/executions/pre-123"),
            },
            Some(OriginalPushedHeadRef {
                snapshot_hash: "snap-abc".into(),
                original_project_path: PathBuf::from("/laptop/proj"),
            }),
        );
        match decide_resume_provenance(&resume) {
            ResumeProvenanceDecision::PinnedPushedHead(pinned) => {
                assert_eq!(pinned.snapshot_hash, "snap-abc");
                assert_eq!(pinned.original_project_path, PathBuf::from("/laptop/proj"));
            }
            other => panic!("expected PinnedPushedHead, got {other:?}"),
        }
    }

    #[test]
    fn localpath_record_without_pushed_ref_selects_live_fs() {
        let resume = resume_record(
            ProjectContext::LocalPath {
                path: PathBuf::from("/home/op/proj"),
            },
            None,
        );
        match decide_resume_provenance(&resume) {
            ResumeProvenanceDecision::LiveFs(path) => {
                assert_eq!(path, std::path::Path::new("/home/op/proj"));
            }
            other => panic!("expected LiveFs, got {other:?}"),
        }
    }

    #[test]
    fn pinned_localpath_record_selects_a_fresh_snapshot_checkout() {
        let mut resume = resume_record(
            ProjectContext::LocalPath {
                path: PathBuf::from("/home/op/proj"),
            },
            None,
        );
        resume.original_snapshot_hash = Some("snap-local".into());

        match decide_resume_provenance(&resume) {
            ResumeProvenanceDecision::PinnedLocalSnapshot {
                snapshot_hash,
                original_path,
            } => {
                assert_eq!(snapshot_hash, "snap-local");
                assert_eq!(original_path, std::path::Path::new("/home/op/proj"));
            }
            other => panic!("expected PinnedLocalSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_scoped_record_without_pushed_ref_is_refused() {
        // No working tree + no pushed-head identity ⇒ the overlay cannot
        // be rebuilt; the decision must be a refusal, never a live-fs
        // fallback.
        let contexts = [
            ProjectContext::SnapshotHash {
                hash: "snap-xyz".into(),
            },
            ProjectContext::ProjectRef {
                principal: "fp:test".into(),
                ref_name: "head".into(),
            },
            ProjectContext::None,
        ];
        for pc in contexts {
            let resume = resume_record(pc.clone(), None);
            match decide_resume_provenance(&resume) {
                ResumeProvenanceDecision::MissingPushedHeadRef(_) => {}
                other => panic!("expected MissingPushedHeadRef for {pc:?}, got {other:?}"),
            }
        }
    }
}
