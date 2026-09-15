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
//! tombstones, exact-kills, and settles an advanced wait-owned execution tree.
//! Shutdown gate closure transfers that cleanup to the coordinator. The
//! detached background path
//! additionally installs `CbTokenGuard` and `TatTokenGuard` inside
//! the spawned task so background-task panic, error, and success exits
//! all revoke the per-thread tokens.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::task;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{ExecutionCompletion, ProjectContext};
use ryeos_engine::protocol_vocabulary::{CallbackChannel, EnvInjectionSource, produce_env_value};
use ryeos_engine::subprocess_spec::SubprocessBuildRequest;

use ryeos_app::callback_token::effective_bundle_id_for_request;
use ryeos_app::callback_token::launch_token_ttl;
use ryeos_app::env_contract::{EnvBinding, EnvSourceDetail};
use ryeos_app::execution_provenance::ExecutionProvenance;
use ryeos_app::launch_metadata::ResumeContext;
use ryeos_app::runtime_db::WorkspaceState;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{
    StopIfAdmissionOpenOutcome, StopIntent, ThreadDetail, is_terminal_status,
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
    /// Exact ingress-authenticated handler authority, if the execution entered
    /// through such a boundary. This is invocation authority, not program
    /// identity, and remains absent for node-internal launches.
    pub handler_context: Option<ryeos_app::handler_context::HandlerContext>,
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
    pub lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    /// Captured runtime ref (`runtime:<name>`) the thread launched under, so the
    /// resume path resolves the SAME runtime by-ref rather than the kind's
    /// current default. `None` for fresh launches and non-runtime-registry kinds.
    pub runtime_ref: Option<String>,
    /// Validated callback parent, carried out-of-band from action params. Direct
    /// tool subprocesses use it to persist operational lineage before spawn so
    /// parent stop/kill cascades cannot miss them.
    pub parent_thread_id: Option<String>,
    /// Kind-neutral durable-effect authority selected and bounded before this
    /// exact terminal launch was prepared.
    pub effect_authority: Option<ryeos_effect_contract::PreparedEffectDispatchAuthority>,
    /// Fresh direct-program evidence captured before capability projection,
    /// vault access, executor compilation, or callback credential minting.
    pub(crate) finalized_direct: Option<FinalizedDirectAdmission>,
}

/// Tracks execution state for explicit cleanup, with a conservative Drop net.
///
/// Callers still invoke `cleanup()` (success/normal-error returns) or
/// `fail_thread()` (pre-spawn errors) so failures can be reported at their
/// source. Drop covers panic/cancellation: it revokes tokens and releases the
/// temp-dir lifeline, then durably stops, exact-kills, and settles the owned
/// wait-owned execution tree. A closed shutdown gate transfers that work to the
/// shutdown coordinator; detached handoff explicitly disarms this guard.
struct ExecutionGuard {
    state: AppState,
    thread_id: Option<String>,
    /// Owned project workspace used for journal/foldback authority.
    temp_dir: Option<Arc<TempDirGuard>>,
    /// Optional disposable process-input workspace. This is deliberately
    /// separate from `temp_dir`: candidate integration executes from a private
    /// copy while daemon authoring and terminal capture retain the owned COW.
    process_input_dir: Option<Arc<TempDirGuard>>,
    thread_finalized: bool,
    callback_token: Option<String>,
    thread_auth_token: Option<String>,
    launch_owner: Option<String>,
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
            process_input_dir: None,
            thread_finalized: false,
            callback_token: None,
            thread_auth_token: None,
            launch_owner: None,
        }
    }

    /// Mark thread as tracked by this guard.
    fn track_thread(&mut self, thread_id: &str) {
        self.thread_id = Some(thread_id.to_string());
    }

    fn track_launch_owner(&mut self, launch_owner: String) {
        self.launch_owner = Some(launch_owner);
    }

    /// Mark temp dir for cleanup. The Arc is cloned; the dir is removed
    /// when the last Arc holder drops.
    fn track_temp_dir(&mut self, guard: Arc<TempDirGuard>) {
        self.temp_dir = Some(guard);
    }

    fn track_process_input_dir(&mut self, guard: Arc<TempDirGuard>) {
        self.process_input_dir = Some(guard);
    }

    fn process_workspace_lifeline(&self) -> Option<Arc<TempDirGuard>> {
        self.process_input_dir
            .clone()
            .or_else(|| self.temp_dir.clone())
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

    /// An explicit preparation error path, never an unconditional Drop hook.
    /// Call only before scheduling a spawn: once handed to a blocking task,
    /// cancellation or a generic error cannot prove absence of process contact.
    fn fail_before_spawn(&mut self, error: anyhow::Error) -> anyhow::Error {
        let cleanup = match (&self.thread_id, &self.launch_owner) {
            (Some(thread_id), Some(owner)) => fail_settled_unattached_thread(
                &self.state,
                thread_id,
                "launch_preparation_failed",
                owner,
            ),
            _ => Err(anyhow::anyhow!(
                "preparation cleanup lacks exact launch ownership"
            )),
        };
        match cleanup {
            Ok(outcome) => {
                if outcome.disarms_guard() {
                    self.mark_finalized();
                }
                self.cleanup();
                error
            }
            Err(cleanup) => error.context(format!("precontact settlement failed: {cleanup:#}")),
        }
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
            let params = ThreadFinalizeParams {
                thread_id: tid.clone(),
                status: "failed".to_string(),
                outcome_code: Some(outcome_code.to_string()),
                result: None,
                error: Some(error),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            };
            let finalize = match self.launch_owner.as_deref() {
                Some(owner) => self.state.threads.finalize_thread_owned(&params, owner),
                None => self.state.threads.finalize_thread(&params),
            };
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
    ) -> anyhow::Result<crate::dispatch::MethodFinalizeOutcome> {
        self.revoke_callback_token();
        self.revoke_thread_auth_token();
        let thread_id = self
            .thread_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("child-link cleanup has no tracked thread"))?;
        let launch_owner = self
            .launch_owner
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("child-link cleanup has no launch owner"))?;
        let outcome = crate::dispatch::finalize_method_thread_if_needed(
            &self.state,
            thread_id,
            launch_owner,
            "failed",
            Some(error),
        )?;
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
        self.process_input_dir = None;
        if let Some(thread_id) = self.thread_id.as_deref()
            && let Err(error) =
                close_aborted_owned_workspace(&self.state, self.temp_dir.as_ref(), thread_id)
        {
            tracing::warn!(thread_id, %error, "cleanup retains unresolved workspace journal");
        }
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
            process_input_dir: self.process_input_dir.take(),
            callback_token: self.callback_token.take(),
            thread_auth_token: self.thread_auth_token.take(),
        }
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        // Revoke callback authority immediately, but retain the workspace
        // lifeline until the owned process tree has stopped. A waited runtime
        // may still have its cwd inside that directory while cancellation is
        // synchronously killing/reaping it.
        self.revoke_callback_token();
        self.revoke_thread_auth_token();
        if self.thread_finalized {
            // Terminal history alone does not close the retained live view.
            // Use the same original-owner cleanup, including its shutdown and
            // unsettled-member refusals, before dropping the last lifeline.
            self.cleanup();
            return;
        }
        let Some(thread_id) = self.thread_id.clone() else {
            self.process_input_dir = None;
            self.temp_dir = None;
            return;
        };

        match stop_owner_dropped_execution_tree(&self.state, &thread_id) {
            Ok(OwnerDropStopOutcome::Settled) => {
                self.thread_finalized = true;
                if let Err(error) =
                    close_aborted_owned_workspace(&self.state, self.temp_dir.as_ref(), &thread_id)
                {
                    tracing::error!(thread_id, %error, "owner-drop preserved unresolved original workspace");
                }
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
        self.process_input_dir = None;
        self.temp_dir = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerDropStopOutcome {
    Settled,
    PreservedForShutdown,
}

/// Cancellation/panic fallback for a waiting request owner.
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
    if !state.state_store.process_attachment_admission_is_open() {
        return Ok(OwnerDropStopOutcome::PreservedForShutdown);
    }
    // Fence new hosted mutations and wait for every pre-existing root-chain
    // lease to settle before terminalization. This is a pushed ownership
    // barrier, not a SQLite polling loop.
    let mut root_terminalization = ryeos_app::hosted_operation::begin_hosted_root_terminalization(
        &state.state_store,
        root_thread_id,
    )?;
    // These are independent ownership domains. A projection/cleanup error in
    // the subordinate session must never prevent the root process tree from
    // being stopped. Preserve both errors and report them only after the root
    // stop has been attempted.
    let mut failures = Vec::new();
    let mut scan_descendants = true;
    match stop_owner_dropped_thread(state, root_thread_id) {
        Err(error) => failures.push(format!("root {root_thread_id}: {error:#}")),
        Ok(OwnerDropThreadOutcome::AlreadyTerminal) => {
            // A terminal root no longer belongs to the request task. In
            // particular, `continued` means the follow callback has already
            // committed ownership to its child/successor chain. The terminal
            // event can reach an SSE client before that callback finishes its
            // spawn handoffs; cancelling descendants here would turn a normal
            // stream close into a durable chain kill.
            scan_descendants = false;
        }
        Ok(OwnerDropThreadOutcome::PreservedForShutdown) => {
            return Ok(OwnerDropStopOutcome::PreservedForShutdown);
        }
        Ok(OwnerDropThreadOutcome::Settled) => {}
    }

    const MAX_DESCENDANT_FIXED_POINT_PASSES: usize = 16;
    let mut seen = BTreeSet::from([root_thread_id.to_string()]);
    let mut reached_fixed_point = !scan_descendants;
    for _ in 0..MAX_DESCENDANT_FIXED_POINT_PASSES {
        if !scan_descendants {
            break;
        }
        let descendants = match state.state_store.descendant_thread_ids(root_thread_id) {
            Ok(descendants) => descendants,
            Err(error) => {
                failures.push(format!("enumerate descendants: {error:#}"));
                break;
            }
        };
        let new_descendants = descendants
            .into_iter()
            .filter(|thread_id| seen.insert(thread_id.clone()))
            .collect::<Vec<_>>();
        if new_descendants.is_empty() {
            reached_fixed_point = true;
            break;
        }
        for thread_id in new_descendants {
            let stop_outcome = stop_owner_dropped_thread(state, &thread_id);
            match stop_outcome {
                Ok(OwnerDropThreadOutcome::Settled | OwnerDropThreadOutcome::AlreadyTerminal) => {}
                Ok(OwnerDropThreadOutcome::PreservedForShutdown) => {
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
    if let Err(error) =
        ryeos_app::dedicated_session_service::abort_session_for_root_stop(state, root_thread_id)
            .context("settle session-bound worker after root-owner cancellation")
    {
        failures.push(format!("session-bound worker: {error:#}"));
    }
    if scan_descendants && reached_fixed_point {
        // The existing lineage query is breadth-first. Settle in reverse so
        // descendants cannot be hidden by clearing the root's attachment first.
        // This is one bounded tree pass, not status polling or another registry.
        let mut order = state.state_store.descendant_thread_ids(root_thread_id)?;
        order.reverse();
        order.push(root_thread_id.to_owned());
        for thread_id in order {
            if !seen.contains(&thread_id) {
                continue;
            }
            if let Err(error) = settle_stopped_workspace_member(state, &thread_id) {
                failures.push(format!("workspace member {thread_id}: {error:#}"));
            }
        }
    }
    if state
        .threads
        .get_thread(root_thread_id)?
        .is_some_and(|thread| is_terminal_status(&thread.status))
    {
        root_terminalization.commit();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerDropThreadOutcome {
    Settled,
    AlreadyTerminal,
    PreservedForShutdown,
}

fn stop_owner_dropped_thread(state: &AppState, thread_id: &str) -> Result<OwnerDropThreadOutcome> {
    let runtime = match state
        .state_store
        .request_thread_stop_if_admission_open(thread_id, StopIntent::Kill)?
    {
        StopIfAdmissionOpenOutcome::Requested(runtime) => runtime,
        StopIfAdmissionOpenOutcome::AlreadyTerminal => {
            return Ok(OwnerDropThreadOutcome::AlreadyTerminal);
        }
        StopIfAdmissionOpenOutcome::PreservedForFollow => {
            tracing::debug!(
                thread_id,
                "execution owner transferred to durable follow waiter"
            );
            return Ok(OwnerDropThreadOutcome::AlreadyTerminal);
        }
        StopIfAdmissionOpenOutcome::PreservedForShutdown => {
            return Ok(OwnerDropThreadOutcome::PreservedForShutdown);
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
        // Shared-view membership and exact attachment must settle atomically,
        // after descendants stop. Keep both until that child-first pass.
        if state
            .state_store
            .thread_workspace_binding(thread_id)?
            .is_none()
        {
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
    Ok(OwnerDropThreadOutcome::Settled)
}

fn settle_stopped_workspace_member(state: &AppState, thread_id: &str) -> Result<()> {
    let Some(binding) = state.state_store.thread_workspace_binding(thread_id)? else {
        return Ok(());
    };
    let identity = state
        .threads
        .get_thread(thread_id)?
        .and_then(|thread| thread.runtime.process_identity)
        .ok_or_else(|| {
            anyhow::anyhow!("stopped member retains uncertain pre-attachment contact")
        })?;
    ryeos_app::process::assert_reaped_process_group_absent(&identity)?;
    let owner = lillux::canonical_json(&serde_json::to_value(&binding.borrower_launch_owner)?)?;
    let settled = if state.state_store.is_launch_owner_active(&owner) {
        state
            .state_store
            .settle_reaped_thread_workspace_owned(thread_id, &binding, &identity)?
    } else {
        state
            .state_store
            .settle_dead_thread_workspace_if_matches(thread_id, &binding, &identity)?
    };
    if !settled {
        anyhow::bail!("stopped workspace member still has unsettled launch authority");
    }
    Ok(())
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
    process_input_dir: Option<Arc<TempDirGuard>>,
    callback_token: Option<String>,
    thread_auth_token: Option<String>,
}

/// Prepared CAS execution context from canonical provenance.
///
/// Pinned project preparation facts derived from admitted provenance. Live
/// provenance carries none of these fields and never enters CAS publication.
struct PreparedCasContext {
    effective_path: PathBuf,
    pre_tree_hash: Option<String>,
    pre_policy_hash: Option<String>,
    /// Snapshot authority captured into launch metadata for native resume.
    /// A live-fs execution may acquire this only after spawn, when its source
    /// manifest is promoted to a durable resume pin.
    resume_snapshot_hash: Option<String>,
    tree_publication: Option<super::PendingCasPublication>,
}

/// Prepare CAS execution context from canonical provenance.
fn prepare_cas_context(
    state: &AppState,
    provenance: &ExecutionProvenance,
    thread_id: &str,
    guard: &mut ExecutionGuard,
) -> Result<PreparedCasContext> {
    let prepared_result: Result<PreparedCasContext> = match provenance {
        ExecutionProvenance::Projectless {
            effective_path,
            workspace_lifeline,
            ..
        } => {
            guard.track_temp_dir(workspace_lifeline.clone());
            if !effective_path.is_dir() {
                anyhow::bail!(
                    "projectless execution workspace does not exist or is not a directory: {}",
                    effective_path.display()
                );
            }
            Ok(PreparedCasContext {
                effective_path: effective_path.clone(),
                pre_tree_hash: None,
                pre_policy_hash: None,
                resume_snapshot_hash: None,
                tree_publication: None,
            })
        }
        ExecutionProvenance::ChildLiveProject {
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
                pre_tree_hash: None,
                pre_policy_hash: None,
                resume_snapshot_hash: None,
                tree_publication: None,
            })
        }
        ExecutionProvenance::ChildPinnedGeneration {
            effective_path,
            workspace_lifeline,
            base_snapshot_hash,
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
            let (tree_hash, policy_hash) = read_pre_tree_for_snapshot(state, base_snapshot_hash)?;
            Ok(PreparedCasContext {
                effective_path: effective_path.clone(),
                pre_tree_hash: Some(tree_hash),
                pre_policy_hash: Some(policy_hash),
                resume_snapshot_hash: Some(base_snapshot_hash.clone()),
                tree_publication: None,
            })
        }
        ExecutionProvenance::ChildImmutableWorkspaceInput {
            effective_path,
            workspace_lifeline,
            input_snapshot_hash,
            ..
        } => {
            guard.track_temp_dir(workspace_lifeline.clone());
            if !effective_path.is_dir() {
                anyhow::bail!(
                    "immutable child input does not exist or is not a directory: {}",
                    effective_path.display()
                );
            }
            let (tree_hash, policy_hash) = read_pre_tree_for_snapshot(state, input_snapshot_hash)?;
            tracing::trace!(
                thread_id = %thread_id,
                effective_path = %effective_path.display(),
                input_snapshot_hash = %input_snapshot_hash,
                "immutable shared-workspace input prepared"
            );
            Ok(PreparedCasContext {
                effective_path: effective_path.clone(),
                pre_tree_hash: Some(tree_hash),
                pre_policy_hash: Some(policy_hash),
                // Workload-operation recovery kills and settles the child; it
                // never promotes transient input capture to resume authority.
                resume_snapshot_hash: provenance.pinned_snapshot_hash().map(str::to_owned),
                tree_publication: None,
            })
        }
        ExecutionProvenance::RootLiveProject {
            project_path,
            workspace_lifeline,
            ..
        } => {
            if let Some(lifeline) = workspace_lifeline {
                guard.track_temp_dir(lifeline.clone());
            }
            tracing::trace!(
                thread_id = %thread_id,
                effective_path = %project_path.display(),
                "live project context prepared without snapshot materialization"
            );

            Ok(PreparedCasContext {
                effective_path: project_path.clone(),
                pre_tree_hash: None,
                pre_policy_hash: None,
                resume_snapshot_hash: None,
                tree_publication: None,
            })
        }
        ExecutionProvenance::RootPinnedGeneration {
            effective_path,
            workspace_lifeline,
            snapshot_hash,
            ..
        } => {
            guard.track_temp_dir(workspace_lifeline.clone());
            let (tree_hash, policy_hash) = read_pre_tree_for_snapshot(state, snapshot_hash)?;
            tracing::trace!(
                thread_id = %thread_id,
                effective_path = %effective_path.display(),
                snapshot_hash = %snapshot_hash,
                "pushed CAS context prepared"
            );
            Ok(PreparedCasContext {
                effective_path: effective_path.clone(),
                pre_tree_hash: Some(tree_hash),
                pre_policy_hash: Some(policy_hash),
                resume_snapshot_hash: Some(snapshot_hash.clone()),
                tree_publication: None,
            })
        }
    };
    prepared_result
}

/// Bind a daemon-owned workspace only after its durable thread birth.
///
/// Callback children borrow their parent's operational workspace. They may
/// execute within it, but must never adopt its journal row or independently
/// drive its create/freeze/destroy lifecycle.
pub(crate) fn bind_owned_workspace_after_thread_birth(
    state: &AppState,
    provenance: &ExecutionProvenance,
    thread_id: &str,
    launch_owner: &str,
) -> Result<()> {
    prepare_owned_workspace_after_thread_birth(
        state,
        provenance,
        thread_id,
        launch_owner,
        WorkspaceConstructionInput::PristineSource,
    )?;
    if let Some(lifeline) = provenance.workspace_lifeline()
        && let Some(lifeline) = lifeline.owned_workspace_lifeline()?
        && let Some((workspace_id, view_identity)) = lifeline.workspace_view_identity()?
    {
        state.state_store.bind_thread_workspace(
            thread_id,
            &ryeos_app::runtime_db::RuntimeWorkspaceBinding {
                workspace_id,
                view_identity,
                borrower_launch_owner: serde_json::from_str(launch_owner)
                    .context("decode exact workspace borrower launch owner")?,
            },
        )?;
    }
    Ok(())
}

/// A new descriptor/view is not necessarily new backing content. Cold recovery
/// carries the exact pre-rebind journal, after process settlement and retained
/// root verification; it must never reapply the initial input generation.
enum WorkspaceConstructionInput<'a> {
    PristineSource,
    RetainedBacking(&'a ryeos_app::runtime_db::WorkspaceRecord),
}

impl WorkspaceConstructionInput<'_> {
    fn preserves_retained_contents(
        &self,
        current: &ryeos_app::runtime_db::WorkspaceRecord,
        thread_id: &str,
    ) -> Result<bool> {
        let Self::RetainedBacking(previous) = self else {
            return Ok(false);
        };
        if !matches!(
            previous.state,
            WorkspaceState::Ready | WorkspaceState::Active | WorkspaceState::Freezing
        ) || current.state != WorkspaceState::Constructing
            || previous.thread_id.as_deref() != Some(thread_id)
            || current.thread_id.as_deref() != Some(thread_id)
            || previous.workspace_id != current.workspace_id
            || previous.root_path != current.root_path
            || previous.base_generation()? != current.base_generation()?
            || previous.frozen_generation()? != current.frozen_generation()?
            || previous.workspace_output_partition_identity
                != current.workspace_output_partition_identity
            || previous.backend_id.is_none()
            || previous.backend_id != current.backend_id
            || previous.backend_version.is_none()
            || previous.backend_version != current.backend_version
            || previous.pinned_root_identities.is_none()
            || previous.pinned_root_identities != current.pinned_root_identities
        {
            anyhow::bail!(
                "retained workspace reconstruction contradicts its exact pre-rebind journal"
            );
        }
        Ok(true)
    }
}

fn prepare_owned_workspace_after_thread_birth(
    state: &AppState,
    provenance: &ExecutionProvenance,
    thread_id: &str,
    launch_owner: &str,
    construction_input: WorkspaceConstructionInput<'_>,
) -> Result<()> {
    if provenance.is_borrowed_child() || !provenance.project_authority().requires_project_foldback()
    {
        return Ok(());
    }
    let Some(lifeline) = provenance.workspace_lifeline() else {
        return Ok(());
    };
    let workspace_outputs = provenance.project_authority().workspace_outputs();
    let Some(root) = lifeline.path() else {
        anyhow::bail!("execution workspace was released before launch birth");
    };
    let workspace_id = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("execution workspace id is not valid UTF-8"))?;
    let Some(record) = state.state_store.execution_workspace(workspace_id)? else {
        anyhow::bail!("execution workspace journal row is missing: {workspace_id}");
    };
    validate_retained_workspace_output_coordinates(
        record.state,
        record.workspace_output_partition_identity.as_deref(),
        record.base_output_capture_hash.as_deref(),
        workspace_outputs.map(|outputs| outputs.partition.partition_identity.as_str()),
        workspace_outputs.and_then(|outputs| outputs.capture_hash.as_deref()),
    )?;
    if record.state == WorkspaceState::Constructing
        && (record.thread_id.is_none()
            || (record.thread_id.as_deref() == Some(thread_id)
                && record.launch_owner.as_deref() == Some(launch_owner)))
    {
        let layout = super::workspace::WorkspaceLayout::from_root(root.clone());
        let preserve_retained_contents =
            construction_input.preserves_retained_contents(&record, thread_id)?;
        if preserve_retained_contents {
            let retained = provenance.pinned_materialization().ok_or_else(|| {
                anyhow::anyhow!("retained workspace reconstruction lost its root proof")
            })?;
            if retained.snapshot_hash() != record.base_snapshot
                || !retained.owns_path(&layout.project)?
            {
                anyhow::bail!(
                    "retained workspace reconstruction changed its proven root or source generation"
                );
            }
            retained.ensure_root_binding()?;
        }
        state.state_store.claim_execution_workspace_construction(
            workspace_id,
            thread_id,
            launch_owner,
        )?;
        let (backend_id, backend_version) = state
            .isolation
            .workspace_backend_identity()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        state.state_store.prepare_execution_workspace_backend(
            workspace_id,
            thread_id,
            launch_owner,
            backend_id,
            backend_version,
        )?;
        if let Some(outputs) = workspace_outputs {
            super::workspace_outputs::admission::validate_current_bounds(
                state,
                &outputs.partition,
            )?;
            if !preserve_retained_contents {
                let authority = super::pinned_state_authority(state)?;
                let guard = authority.acquire_shared_guard()?;
                // The source proof must precede output restoration. Restored
                // outputs are a separately admitted partition and therefore
                // intentionally make the resulting project root differ from
                // the source-only ProjectSnapshot.
                let source_materialization = ryeos_state::PinnedProjectMaterialization::verify(
                    &authority,
                    &guard,
                    &record.base_snapshot,
                    &layout.project,
                )?;
                if let Some(capture_hash) = outputs.capture_hash.as_deref() {
                    let budget = super::external_content::private_materialization_budget()?;
                    restore_process_workspace_output_generation(
                        state,
                        &authority,
                        &guard,
                        &source_materialization,
                        &outputs.partition,
                        capture_hash,
                        &budget,
                    )?;
                    budget.emit_metrics(thread_id)?;
                }
            }
        }
        let created = state
            .isolation
            .create_workspace(
                ryeos_engine::isolation::WorkspaceLifecycleInvocation {
                    operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create,
                    workspace_id,
                    launch_owner,
                    base_snapshot: &record.base_snapshot,
                    project_path: &layout.project,
                    mount_identity: None,
                },
                &|held| {
                    #[cfg(target_os = "linux")]
                    {
                        let identity =
                            ryeos_app::process::capture_execution_process_identity_from_pidfd(
                                i64::from(held.pid()),
                                Some(i64::from(held.pgid())),
                                held.pidfd(),
                            )
                            .map_err(|error| error.to_string())?;
                        state
                            .state_store
                            .attach_workspace_creator(
                                workspace_id,
                                thread_id,
                                launch_owner,
                                &identity,
                            )
                            .map_err(|error| error.to_string())
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        let _ = held;
                        Err(
                            "workspace creator attachment requires exact process identity support"
                                .to_owned(),
                        )
                    }
                },
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let evidence = created.evidence;
        lifeline.install_workspace_view(
            &evidence,
            created
                .created_view
                .ok_or_else(|| anyhow::anyhow!("workspace Create omitted its retained view"))?,
        )?;
        let pinned_root_identities =
            lillux::canonical_json(&serde_json::to_value(&evidence.pinned_root_identities)?)?;
        if record
            .pinned_root_identities
            .as_deref()
            .is_some_and(|expected| expected != pinned_root_identities)
            || record
                .backend_id
                .as_deref()
                .is_some_and(|expected| expected != evidence.backend_id)
            || record
                .backend_version
                .as_deref()
                .is_some_and(|expected| expected != evidence.backend_version)
        {
            anyhow::bail!(
                "workspace reconstruction changed its retained backend or pinned backing roots"
            );
        }
        if provenance
            .project_authority()
            .operational_snapshot_projection()
            != Some(record.base_snapshot.as_str())
        {
            anyhow::bail!("workspace source generation contradicts admitted project authority");
        }
        if state.isolation.is_enforced() {
            state
                .state_store
                .assert_execution_workspace_creator_reaped(workspace_id, thread_id, launch_owner)?;
        }
        state
            .state_store
            .bind_execution_workspace(ryeos_app::runtime_db::WorkspaceBinding {
                workspace_id,
                thread_id,
                launch_owner: Some(launch_owner),
                backend_id: Some(&evidence.backend_id),
                backend_version: Some(&evidence.backend_version),
                pinned_root_identities: Some(&pinned_root_identities),
                mount_identity: evidence.mount_identity.as_deref(),
                workspace_output_partition_identity: workspace_outputs
                    .map(|outputs| outputs.partition.partition_identity.as_str()),
                base_output_capture_hash: workspace_outputs
                    .and_then(|outputs| outputs.capture_hash.as_deref()),
            })?;
    } else if record.state != WorkspaceState::Ready
        || record.thread_id.as_deref() != Some(thread_id)
        || record.launch_owner.as_deref() != Some(launch_owner)
    {
        anyhow::bail!(
            "execution workspace {workspace_id} cannot be adopted from state {}",
            record.state
        );
    }
    Ok(())
}

fn validate_retained_workspace_output_coordinates(
    state: WorkspaceState,
    retained_partition_identity: Option<&str>,
    retained_base_capture_hash: Option<&str>,
    admitted_partition_identity: Option<&str>,
    admitted_base_capture_hash: Option<&str>,
) -> Result<()> {
    let retained_is_unbound =
        retained_partition_identity.is_none() && retained_base_capture_hash.is_none();
    if state == WorkspaceState::Constructing && retained_is_unbound {
        return Ok(());
    }
    if retained_partition_identity != admitted_partition_identity
        || retained_base_capture_hash != admitted_base_capture_hash
    {
        anyhow::bail!("retained workspace output generation contradicts admitted provenance");
    }
    Ok(())
}

/// Retrieve the original retained view only through its already admitted
/// per-launch membership. A path, chain relationship or absent runtime row
/// must never manufacture borrow authority. Process attachment repeats the
/// journal gate before releasing the held target.
pub(crate) fn borrow_bound_workspace_view(
    state: &AppState,
    lifeline: Option<&Arc<TempDirGuard>>,
    thread_id: &str,
) -> Result<Option<lillux::InheritedDescriptorAuthority>> {
    let original = lifeline
        .map(|lifeline| lifeline.owned_workspace_lifeline())
        .transpose()?
        .flatten();
    let Some(lifeline) = original else {
        if state
            .state_store
            .thread_workspace_binding(thread_id)?
            .is_some()
        {
            anyhow::bail!("workspace borrower lost its original lifeline");
        }
        return Ok(None);
    };
    let Some((workspace_id, view_identity)) = lifeline.workspace_view_identity()? else {
        if state
            .state_store
            .thread_workspace_binding(thread_id)?
            .is_some()
        {
            anyhow::bail!("ordinary materialization cannot replace a bound workspace view");
        }
        return Ok(None);
    };
    let binding = state
        .state_store
        .thread_workspace_binding(thread_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace view has no admitted borrower"))?;
    if binding.workspace_id != workspace_id || binding.view_identity != view_identity {
        anyhow::bail!("workspace borrower no longer names its original view");
    }
    // Exact replay checks the current owner, admission barrier and root state.
    state
        .state_store
        .authorize_thread_workspace_contact(thread_id, &binding)?;
    lifeline.borrow_workspace_view(&workspace_id, &view_identity)
}

/// Activate an owner-bound workspace only after the held process identity is
/// durable. Borrowed children pass `owns_workspace = false` and leave the
/// parent's workspace lifecycle untouched.
pub(crate) fn activate_workspace_after_process_attachment(
    state: &AppState,
    workspace_lifeline: Option<&Arc<TempDirGuard>>,
    owns_workspace: bool,
    thread_id: &str,
    launch_owner: &str,
    process_identity: &ryeos_app::process::ExecutionProcessIdentity,
) -> Result<()> {
    if !owns_workspace {
        return Ok(());
    }
    let Some(root) = workspace_lifeline.and_then(|guard| guard.path()) else {
        anyhow::bail!("owned execution workspace was released before process attachment");
    };
    let workspace_id = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("workspace id is not valid UTF-8"))?;
    let record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("owned execution workspace journal row is missing"))?;
    if record.thread_id.as_deref() != Some(thread_id)
        || record.launch_owner.as_deref() != Some(launch_owner)
    {
        anyhow::bail!("execution workspace {workspace_id} is not owned by this launch");
    }
    let identity = serde_json::to_string(process_identity)?;
    state.state_store.transition_execution_workspace_owned(
        workspace_id,
        thread_id,
        launch_owner,
        &[WorkspaceState::Ready],
        WorkspaceState::Active,
        Some(&identity),
    )
}

fn transition_owned_workspace(
    state: &AppState,
    lifeline: Option<&Arc<TempDirGuard>>,
    thread_id: &str,
    expected: &[WorkspaceState],
    next: WorkspaceState,
    process_identity: Option<&ryeos_app::process::ExecutionProcessIdentity>,
) -> Result<()> {
    let Some(root) = lifeline.and_then(|guard| guard.path()) else {
        return Ok(());
    };
    let workspace_id = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("execution workspace id is not valid UTF-8"))?;
    let Some(record) = state.state_store.execution_workspace(workspace_id)? else {
        anyhow::bail!("execution workspace journal row is missing: {workspace_id}");
    };
    if record.thread_id.as_deref() != Some(thread_id) {
        anyhow::bail!("execution workspace {workspace_id} is owned by another thread");
    }
    let launch_owner = record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("execution workspace has no launch owner"))?;
    let identity = process_identity
        .map(serde_json::to_string)
        .transpose()
        .context("serialize workspace process identity")?;
    state.state_store.transition_execution_workspace_owned(
        workspace_id,
        thread_id,
        launch_owner,
        expected,
        next,
        identity.as_deref(),
    )
}

fn close_owned_workspace(
    state: &AppState,
    lifeline: Option<&Arc<TempDirGuard>>,
    thread_id: &str,
) -> Result<()> {
    close_owned_workspace_from_states(state, lifeline, thread_id, &[WorkspaceState::Freezing])
}

/// Final/abnormal cleanup through the launch's retained original lifeline.
/// Projectless controllers can own a separate confined worker workspace; a
/// borrowed child can never close its parent's journal. Unknown pre-attachment
/// contact deliberately remains fenced for reconciliation.
pub(crate) fn close_aborted_owned_workspace(
    state: &AppState,
    lifeline: Option<&Arc<TempDirGuard>>,
    thread_id: &str,
) -> Result<()> {
    if !state.state_store.process_attachment_admission_is_open() {
        // Shutdown explicitly transfers recovery to the durable journal;
        // a request Drop must not race the coordinator's process teardown.
        return Ok(());
    }
    let original = lifeline
        .map(|guard| guard.owned_workspace_lifeline())
        .transpose()?
        .flatten();
    let Some(guard) = original else {
        return Ok(());
    };
    let Some(root) = guard.path() else {
        return Ok(());
    };
    let id = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("owned workspace ID is not UTF-8"))?;
    let Some(record) = state.state_store.execution_workspace(id)? else {
        return Ok(());
    };
    if record.thread_id.as_deref() != Some(thread_id) || record.state == WorkspaceState::Closed {
        return Ok(());
    }
    let thread = state
        .threads
        .get_thread(thread_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace cleanup root disappeared"))?;
    if !ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
        .is_some_and(|status| status.is_terminal())
    {
        anyhow::bail!("workspace cleanup requires settled terminal root authority");
    }
    if thread.runtime.process_identity.is_some() {
        anyhow::bail!("workspace cleanup still has an attached root process");
    }
    close_owned_workspace_from_states(
        state,
        Some(&guard),
        thread_id,
        &[
            WorkspaceState::Ready,
            WorkspaceState::Active,
            WorkspaceState::Freezing,
            WorkspaceState::Destroying,
            WorkspaceState::Closing,
        ],
    )
}

/// Destroy an execution's owned workspace only after its process has
/// exited and terminal state is authoritative. Retained/advanced generations
/// must already be frozen and named by terminal state; `Discard` deliberately
/// skips capture and may close directly from `active`. Direct subprocesses
/// and managed runtimes share this publication owner; don't infer `Freezing`
/// merely because a process has finished.
pub(crate) fn close_terminal_workspace(
    state: &AppState,
    lifeline: Option<&Arc<TempDirGuard>>,
    thread_id: &str,
    terminal_publication: &ryeos_state::objects::PinnedTerminalPublication,
    result_project_snapshot_hash: Option<&str>,
) -> Result<()> {
    let Some(guard) = lifeline else {
        return Ok(());
    };
    let Some(root) = guard.path() else {
        return Ok(());
    };
    let workspace_id = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("execution workspace id is not valid UTF-8"))?;
    let record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("execution workspace journal row is missing"))?;
    let expected = terminal_workspace_close_source(
        terminal_publication,
        record.state,
        record.frozen_snapshot_hash.as_deref(),
        result_project_snapshot_hash,
    )?;
    close_owned_workspace_from_states(state, lifeline, thread_id, &[expected])
}

fn terminal_workspace_close_source(
    terminal_publication: &ryeos_state::objects::PinnedTerminalPublication,
    workspace_state: WorkspaceState,
    frozen_snapshot_hash: Option<&str>,
    result_project_snapshot_hash: Option<&str>,
) -> Result<WorkspaceState> {
    match terminal_publication {
        ryeos_state::objects::PinnedTerminalPublication::Discard => match workspace_state {
            WorkspaceState::Ready => Ok(WorkspaceState::Ready),
            WorkspaceState::Active => Ok(WorkspaceState::Active),
            WorkspaceState::Freezing => Ok(WorkspaceState::Freezing),
            state => anyhow::bail!("discarded managed workspace cannot close from state {state}"),
        },
        ryeos_state::objects::PinnedTerminalPublication::RetainResult
        | ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead { .. }
        | ryeos_state::objects::PinnedTerminalPublication::AdvanceHead { .. } => {
            if workspace_state != WorkspaceState::Freezing {
                anyhow::bail!("retained managed workspace was not frozen before terminal commit");
            }
            let frozen = frozen_snapshot_hash.ok_or_else(|| {
                anyhow::anyhow!("retained managed workspace has no frozen result generation")
            })?;
            if result_project_snapshot_hash != Some(frozen) {
                anyhow::bail!(
                    "retained managed workspace result generation disagrees with terminal state"
                );
            }
            Ok(WorkspaceState::Freezing)
        }
    }
}

fn close_owned_workspace_from_states(
    state: &AppState,
    lifeline: Option<&Arc<TempDirGuard>>,
    thread_id: &str,
    expected_states: &[WorkspaceState],
) -> Result<()> {
    let Some(guard) = lifeline else {
        return Ok(());
    };
    let Some(root) = guard.path() else {
        return Ok(());
    };
    let layout = super::workspace::WorkspaceLayout::from_root(root.clone());
    let workspace_id = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("execution workspace id is not valid UTF-8"))?;
    let record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("execution workspace journal row is missing"))?;
    if record.thread_id.as_deref() != Some(thread_id) {
        anyhow::bail!("execution workspace is owned by another thread");
    }
    let launch_owner = record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("execution workspace has no launch owner"))?;
    if record.state == WorkspaceState::Closed {
        return Ok(());
    }
    if !expected_states.contains(&record.state) {
        anyhow::bail!(
            "workspace close encountered unexpected state {}",
            record.state
        );
    }
    if !matches!(
        record.state,
        WorkspaceState::Destroying | WorkspaceState::Closing
    ) {
        transition_owned_workspace(
            state,
            Some(guard),
            thread_id,
            expected_states,
            WorkspaceState::Destroying,
            None,
        )?;
    }
    // The no-new-contact cut precedes the complete existing-owner check.
    // Refresh the exact journal coordinate after the state transition so the
    // worker/pool owner can detect any concurrent change without using stale
    // state as evidence. Root membership alone cannot cover a failed worker
    // start whose target identity was never reported.
    let record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace disappeared during close"))?;
    let view_identity = record
        .mount_identity
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace close has no created view identity"))?;
    super::assert_workspace_capture_processes_settled(state, &record)?;
    guard.close_workspace_view(
        workspace_id,
        view_identity,
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
    )?;
    if record.state == WorkspaceState::Closing {
        guard.remove_now()?;
        return state.state_store.transition_execution_workspace_owned(
            workspace_id,
            thread_id,
            launch_owner,
            &[WorkspaceState::Closing],
            WorkspaceState::Closed,
            None,
        );
    }
    let destroyed = state
        .isolation
        .workspace_lifecycle(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
            operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Destroy,
            workspace_id,
            launch_owner,
            base_snapshot: &record.base_snapshot,
            project_path: &layout.project,
            mount_identity: record.mount_identity.as_deref(),
        })
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let pinned = lillux::canonical_json(&serde_json::to_value(&destroyed.pinned_root_identities)?)?;
    if record.backend_id.as_deref() != Some(destroyed.backend_id.as_str())
        || record.backend_version.as_deref() != Some(destroyed.backend_version.as_str())
        || record.pinned_root_identities.as_deref() != Some(pinned.as_str())
        || record.mount_identity != destroyed.mount_identity
    {
        anyhow::bail!("workspace destroy evidence does not match the durable journal");
    }
    transition_owned_workspace(
        state,
        Some(guard),
        thread_id,
        &[WorkspaceState::Destroying],
        WorkspaceState::Closing,
        None,
    )?;
    guard.remove_now()?;
    state.state_store.transition_execution_workspace_owned(
        workspace_id,
        thread_id,
        launch_owner,
        &[WorkspaceState::Closing],
        WorkspaceState::Closed,
        None,
    )
}

fn read_pre_tree_for_snapshot(state: &AppState, snap_hash: &str) -> Result<(String, String)> {
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let snapshot =
        ryeos_state::project_materialization::load_project_snapshot_bounded(&cas, snap_hash)?
            .ok_or_else(|| anyhow::anyhow!("snapshot {} not found in CAS", snap_hash))?;
    Ok((snapshot.project_tree_hash, snapshot.effective_policy_hash))
}

fn record_candidate_integration_process_completion(
    state: &AppState,
    thread_id: &str,
    authority: Option<&ryeos_app::thread_lifecycle::CandidateEvaluationAuthority>,
    completion: &ExecutionCompletion,
) -> Result<Option<ryeos_app::thread_lifecycle::CandidateIntegrationProcessCompletionFact>> {
    let Some(authority) = authority else {
        return Ok(None);
    };
    if !matches!(
        &authority.purpose,
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. }
    ) {
        return Ok(None);
    }
    if completion.status != ryeos_engine::contracts::ThreadTerminalStatus::Completed {
        return Ok(None);
    }
    if completion
        .outcome_code
        .as_deref()
        .is_some_and(|code| code != "success")
        || completion.error.is_some()
        || !completion.artifacts.is_empty()
        || completion.final_cost.is_some()
        || completion.continuation_request.is_some()
    {
        bail!("successful candidate integration returned a non-canonical terminal contract");
    }
    let thread = state
        .state_store
        .get_thread(thread_id)?
        .ok_or_else(|| anyhow::anyhow!("candidate integration thread disappeared"))?;
    if thread.thread_id != thread_id || thread.chain_root_id != thread_id {
        bail!("candidate integration completion is not attached to its sealed root");
    }
    let capsule_hash = thread
        .admitted_launch_capsule_hash
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("candidate integration root has no admitted capsule"))?;
    let process_completion_digest =
        ryeos_state::objects::canonical_value_digest(&serde_json::to_value(completion)?)?;
    let fact = ryeos_app::thread_lifecycle::CandidateIntegrationProcessCompletionFact::new(
        authority,
        thread_id,
        capsule_hash,
        process_completion_digest,
    )?;
    ryeos_app::authoritative_root_fact::append_once(
        state,
        thread_id,
        ryeos_app::thread_lifecycle::CANDIDATE_INTEGRATION_PROCESS_COMPLETED_EVENT,
        &fact.operation_id,
        serde_json::to_value(&fact)?,
    )?;
    Ok(Some(fact))
}

struct PostExecutionFoldbackParams<'a> {
    pub state: &'a AppState,
    pub contact_fence: &'a ryeos_app::hosted_operation::HostedRootTerminalizationGuard,
    pub thread_id: &'a str,
    pub acting_principal: &'a str,
    pub pre_tree_hash: &'a str,
    pub pre_policy_hash: &'a str,
    pub base_snapshot_hash: &'a str,
    pub terminal_publication: &'a ryeos_state::objects::PinnedTerminalPublication,
    pub project_path: &'a std::path::Path,
    pub execution_dir: Option<&'a std::path::Path>,
    pub completion: &'a ExecutionCompletion,
}

fn post_execution_foldback(
    params: PostExecutionFoldbackParams<'_>,
) -> Result<crate::execution::PendingProjectResult> {
    let PostExecutionFoldbackParams {
        state,
        contact_fence: _contact_fence,
        thread_id,
        acting_principal,
        pre_tree_hash,
        pre_policy_hash,
        base_snapshot_hash,
        terminal_publication,
        project_path,
        execution_dir,
        completion: _completion,
    } = params;
    if matches!(
        terminal_publication,
        ryeos_state::objects::PinnedTerminalPublication::Discard
    ) {
        anyhow::bail!("discarded COW execution cannot publish a fold-back generation");
    }
    let authority = super::pinned_state_authority(state)
        .context("pin state authority for authoritative fold-back")?;
    // Need a working dir for fold-back. If neither an exec checkout
    // nor a LocalPath project_path is available (resume of a non-
    // LocalPath thread), nothing to fold back into.
    let working_dir = execution_dir.unwrap_or(project_path);
    let layout = super::workspace::WorkspaceLayout::from_root(working_dir.to_path_buf());
    let workspace_id = layout
        .root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("workspace id is not valid UTF-8"))?;
    let workspace_record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace journal row is missing: {workspace_id}"))?;
    let launch_owner = workspace_record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace {workspace_id} has no launch owner"))?;
    state
        .state_store
        .assert_launch_owner(thread_id, launch_owner)
        .context("fence authoritative fold-back to current launch owner")?;
    super::assert_workspace_capture_processes_settled(state, &workspace_record)?;

    // The shared CAS guard is the outer mutation lock. Keep it live from the
    // first fold-back object write through the signed HEAD publication so GC
    // cannot sweep an unpublished intermediate closure.
    let cas_mutation_guard = authority
        .acquire_shared_guard()
        .context("acquire pinned CAS mutation guard for authoritative fold-back")?;

    // Acquire write barrier for CAS mutations (fold-back + head advance).
    let permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("acquire CAS write permit for fold-back: {error}"))?;

    // Fold back changes
    let (operational_shadow_paths, output_context) =
        super::admitted_workspace_capture_inputs(state, thread_id, &workspace_record)?;
    let crate::execution::FoldBackCapture {
        tree_hash: output_tree_hash,
        outputs,
        mut publication,
    } = crate::execution::fold_back_outputs(crate::execution::FoldBackOutputsParams {
        authority: &authority,
        cas_mutation_guard: &cas_mutation_guard,
        isolation: &state.isolation,
        workspace_id,
        launch_owner,
        working_dir,
        pre_tree_hash,
        policy_hash: pre_policy_hash,
        base_snapshot_hash,
        workspace_record: &workspace_record,
        operational_shadow_paths: &operational_shadow_paths,
        output_partition: output_context.as_ref().map(|context| &context.partition),
    })
    .context("freeze, validate, and publish authoritative project delta")?;

    let advance_head = if output_tree_hash.is_some() {
        if let ryeos_state::objects::PinnedTerminalPublication::AdvanceHead { .. } =
            terminal_publication
        {
            let ryeos_state::objects::PinnedTerminalPublication::AdvanceHead {
                head_ref,
                expected_hash,
            } = terminal_publication
            else {
                unreachable!("advance-head branch is guarded by the publication variant")
            };
            let project_str = project_path.to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot advance fold-back HEAD for non-UTF-8 project identity {}",
                    project_path.display()
                )
            })?;
            let canonical_project =
                crate::execution::project_source::canonical_project_ref(project_str)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let project_hash = lillux::cas::sha256_hex(canonical_project.as_bytes());
            let principal_key = ryeos_state::refs::principal_storage_key(acting_principal)
                .context("derive fold-back principal storage identity")?;
            let expected_head_ref = format!("projects/{principal_key}/{project_hash}/head");
            if head_ref != &expected_head_ref {
                anyhow::bail!(
                    "sealed terminal publication ref changed before fold-back: expected {expected_head_ref:?}, got {head_ref:?}"
                );
            }
            Some((head_ref, expected_hash))
        } else {
            None
        }
    } else {
        None
    };
    let snapshot_hash = match output_tree_hash {
        Some(tree_hash) => crate::execution::store_foldback_snapshot(
            &authority,
            &cas_mutation_guard,
            &tree_hash,
            base_snapshot_hash,
            &mut publication,
        )?,
        None => base_snapshot_hash.to_owned(),
    };
    let generation = ryeos_state::objects::WorkspaceGenerationPair {
        output_capture_hash: super::store_workspace_output_capture(
            &authority,
            &cas_mutation_guard,
            output_context.as_ref(),
            outputs,
            base_snapshot_hash,
            &snapshot_hash,
            &mut publication,
        )?,
        snapshot_hash,
    };
    // The shared CAS guard and staged-root publication retain the completed
    // closure. Release the online mutation permit before StateStore acquires
    // its own permit for the owner-fenced workspace journal transaction.
    drop(permit);
    state
        .state_store
        .assert_launch_owner(thread_id, launch_owner)?;
    state.state_store.bind_frozen_execution_workspace(
        workspace_id,
        thread_id,
        launch_owner,
        &generation,
    )?;
    if let Some((head_ref, expected_hash)) = advance_head {
        super::advance_head_to_frozen_runtime_result(
            state,
            thread_id,
            launch_owner,
            head_ref,
            expected_hash,
            &generation.snapshot_hash,
        )?;
    }
    Ok(crate::execution::PendingProjectResult {
        generation,
        publication: Some(publication),
        quiesced: None,
    })
}

/// Verify restart metadata against the admitted project authority. Live and
/// projectless executions recover through that exact authority; this boundary
/// must never silently promote them into pinned executions after admission.
fn validate_resume_project_authority(
    _state: &AppState,
    launch_metadata: &mut ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    _pre_tree_hash: &Option<String>,
    _pre_policy_hash: &Option<String>,
    resume_snapshot_hash: &Option<String>,
    _tree_publication: &mut Option<super::PendingCasPublication>,
) -> Result<Option<super::CapturedProjectGeneration>> {
    if launch_metadata.native_resume.is_none() {
        return Ok(None);
    }
    let authority = &launch_metadata
        .resume_context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("native-resume launch is missing durable resume metadata"))?
        .project_authority;
    match authority {
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
        | ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. } => {
            if resume_snapshot_hash.is_some() {
                anyhow::bail!(
                    "live or projectless native-resume authority cannot carry an implicit snapshot"
                );
            }
        }
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash, ..
        } => {
            if resume_snapshot_hash.as_deref() != Some(snapshot_hash.as_str()) {
                anyhow::bail!(
                    "pinned native-resume authority does not match its admitted generation"
                );
            }
        }
    }
    Ok(None)
}

fn release_tree_publication(
    publication: Option<super::PendingCasPublication>,
    context: &'static str,
) {
    if let Some(publication) = publication
        && let Err(error) = publication.publish()
    {
        tracing::warn!(%error, context, "failed to release staged CAS publication roots");
    }
}

fn release_snapshot_publication(
    publication: Option<super::CapturedProjectGeneration>,
    context: &'static str,
) {
    if let Some(publication) = publication
        && let Err(error) = publication.publish()
    {
        tracing::warn!(%error, context, "failed to release staged CAS snapshot roots");
    }
}

/// Attach a held process to the runtime ledger and activate any workspace.
/// This helper never kills or settles lifecycle state: its caller still owns
/// the pending process and must prove abort/reap before terminal settlement.
struct PendingAttachParams<'a> {
    state: &'a AppState,
    thread_id: &'a str,
    spawned_pid: u32,
    spawned_pgid: i64,
    process_identity: &'a ryeos_app::process::ExecutionProcessIdentity,
    launch_metadata: &'a ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    failed_outcome_code: &'a str,
    launch_owner: &'a str,
    workspace_lifeline: Option<&'a Arc<TempDirGuard>>,
    owns_workspace: bool,
}

#[derive(Debug)]
struct PendingAttachFailure {
    operation: &'static str,
    outcome_code: String,
    process_attached: bool,
    error: anyhow::Error,
}

fn attach_pending_process(
    params: PendingAttachParams<'_>,
) -> std::result::Result<(), PendingAttachFailure> {
    let PendingAttachParams {
        state,
        thread_id,
        spawned_pid,
        spawned_pgid,
        process_identity,
        launch_metadata,
        failed_outcome_code,
        launch_owner,
        workspace_lifeline,
        owns_workspace,
    } = params;
    if let Err(err) = state.threads.attach_new_process_owned(
        &ThreadAttachProcessParams {
            thread_id: thread_id.to_string(),
            pid: spawned_pid as i64,
            pgid: spawned_pgid,
            process_identity: Some(process_identity.clone()),
            metadata: None,
            launch_metadata: launch_metadata.clone(),
        },
        launch_owner,
    ) {
        return Err(PendingAttachFailure {
            operation: "attach process",
            outcome_code: failed_outcome_code.to_string(),
            process_attached: false,
            error: err,
        });
    }
    let workspace_activation = activate_workspace_after_process_attachment(
        state,
        workspace_lifeline,
        owns_workspace,
        thread_id,
        launch_owner,
        process_identity,
    );
    if let Err(error) = workspace_activation {
        return Err(PendingAttachFailure {
            operation: "activate workspace",
            outcome_code: "workspace_activation_failed".to_string(),
            process_attached: true,
            error,
        });
    }
    Ok(())
}

fn abort_and_settle_pending_attach_failure(
    state: &AppState,
    thread_id: &str,
    launch_owner: &str,
    spawned: ryeos_app::thread_lifecycle::SpawnedItemAwaitingAttachment,
    failure: PendingAttachFailure,
) -> ExecutionCleanupFailure {
    let identity = spawned.process_identity.clone();
    let cleanup = match spawned.abort_and_reap() {
        Err(error) => {
            Err(error
                .context("abort and reap attachment-pending process before lifecycle settlement"))
        }
        Ok(()) => (|| {
            if failure.process_attached {
                // Settle membership and attachment together. Clearing the PID
                // first would discard the exact reaped-process authority.
                clear_finished_process(state, thread_id, &identity, launch_owner)?;
            }
            fail_settled_unattached_thread(state, thread_id, &failure.outcome_code, launch_owner)
        })(),
    };
    ExecutionCleanupFailure {
        operation: failure.operation,
        operation_error: failure.error,
        cleanup,
    }
}

/// Only call after bounded preparation failed before process contact, or after
/// the held process was successfully aborted and reaped. NULL PID, terminal
/// status, task cancellation and Drop are never substitutes for that proof.
fn fail_settled_unattached_thread(
    state: &AppState,
    thread_id: &str,
    outcome_code: &str,
    launch_owner: &str,
) -> Result<ExecutionCleanupOutcome> {
    let outcome = fail_thread_static_owned(state, thread_id, outcome_code, launch_owner)?;
    if matches!(outcome, ExecutionCleanupOutcome::PreservedForShutdown) {
        return Ok(outcome);
    }
    if let Some(binding) = state.state_store.thread_workspace_binding(thread_id)? {
        let owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(launch_owner)?;
        if binding.borrower_launch_owner != owner {
            anyhow::bail!("settled launch cannot retire another workspace borrower's membership");
        }
        if !state
            .state_store
            .settle_thread_workspace_owned(thread_id, &binding)?
        {
            anyhow::bail!(
                "settled launch retains process contact or unsettled workspace descendants"
            );
        }
    }
    Ok(outcome)
}

pub(crate) fn clear_finished_process(
    state: &AppState,
    thread_id: &str,
    process_identity: &ryeos_app::process::ExecutionProcessIdentity,
    launch_owner: &str,
) -> Result<()> {
    let settled = (|| -> Result<bool> {
        ryeos_app::process::assert_reaped_process_group_absent(process_identity)?;
        if let Some(binding) = state.state_store.thread_workspace_binding(thread_id)? {
            state.state_store.settle_reaped_thread_workspace_owned(
                thread_id,
                &binding,
                process_identity,
            )
        } else {
            state.state_store.clear_thread_process_if_matches_owned(
                thread_id,
                process_identity,
                launch_owner,
            )
        }
    })();
    match settled {
        Ok(true) => Ok(()),
        Ok(false) => anyhow::bail!(
            "finished process retains changed identity or unsettled workspace descendants"
        ),
        Err(error) => Err(error.context("settle exact finished process and workspace membership")),
    }
}

fn release_cleanup_is_settled(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ryeos_engine::error::EngineError>(),
        Some(ryeos_engine::error::EngineError::AttachmentReleaseFailed { source })
            if source.cleanup_is_settled()
    )
}

/// Finalize a thread from its completion result.
fn finalize_completion(
    state: &AppState,
    thread_id: &str,
    completion: ExecutionCompletion,
    result_generation: Option<&ryeos_state::objects::WorkspaceGenerationPair>,
    launch_owner: &str,
) -> std::result::Result<ThreadDetail, ExecutionCleanupFailure> {
    let finalized = state.threads.finalize_from_completion_owned(
        thread_id,
        launch_owner,
        &completion,
        result_generation,
    );
    match finalized {
        Ok(thread) => Ok(thread),
        Err(err) => {
            tracing::error!(error = %err, "invalid completion during finalization");
            Err(ExecutionCleanupFailure {
                operation: "finalize completion",
                operation_error: err,
                cleanup: fail_thread_static_owned(
                    state,
                    thread_id,
                    "invalid_completion",
                    launch_owner,
                ),
            })
        }
    }
}

/// Result returned after waiting for the admitted thread to settle.
pub struct WaitResult {
    pub finalized_thread: ThreadDetail,
    pub result: Value,
    /// Exact terminal project generation committed by this execution, when
    /// its project publication policy retains one.
    pub result_project_snapshot_hash: Option<String>,
    /// The `--debug-raw` block (resolved cmd/args/cwd/env keys + exit code and
    /// size-limited raw stdout/stderr), present only when the flag was set.
    pub debug: Option<Value>,
    /// Present only for an admitted durable callback dispatch. Live callback
    /// provenance is added by the callback boundary, which owns the exact
    /// wire action digest even when no effect authorization exists.
    pub dispatch_effect: Option<ryeos_runtime::callback_contract::RuntimeDispatchEvidence>,
}

/// Waiting for a durable dispatch can either execute a fresh child or return
/// the immutable answer without creating a child thread.
pub enum WaitOutcome {
    Executed(WaitResult),
    Replayed {
        result: Value,
        dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence,
    },
}

fn runtime_effect_class(
    class: ryeos_effect_contract::EffectClass,
) -> ryeos_runtime::callback_contract::RuntimeDispatchEffectClass {
    match class {
        ryeos_effect_contract::EffectClass::Recorded => {
            ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Recorded
        }
        ryeos_effect_contract::EffectClass::Sealed => {
            ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Sealed
        }
    }
}

fn verify_dispatch_effect_record(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    indexed: &ryeos_state::ReplayIndexRecord,
) -> ryeos_state::ReplayRecordVerification {
    let _guard = match authority.acquire_shared_guard() {
        Ok(guard) => guard,
        Err(error) => {
            return ryeos_state::ReplayRecordVerification::Unavailable {
                reason: format!("dispatch effect CAS authority unavailable: {error:#}"),
            };
        }
    };
    let value = match authority
        .cas_store()
        .and_then(|cas| cas.get_object(&indexed.record_hash))
    {
        Ok(Some(value)) => value,
        Ok(None) => {
            return ryeos_state::ReplayRecordVerification::IntegrityFailure {
                reason: format!(
                    "indexed dispatch-effect record {} is missing from CAS",
                    indexed.record_hash
                ),
            };
        }
        Err(error) => {
            return ryeos_state::ReplayRecordVerification::Unavailable {
                reason: format!(
                    "could not read indexed dispatch-effect record {}: {error:#}",
                    indexed.record_hash
                ),
            };
        }
    };
    let record = match ryeos_effect_contract::DispatchEffectRecord::from_current_value(&value) {
        Ok(record) => record,
        Err(error) => {
            return ryeos_state::ReplayRecordVerification::IntegrityFailure {
                reason: format!(
                    "indexed dispatch-effect record {} is invalid: {error:#}",
                    indexed.record_hash
                ),
            };
        }
    };
    if record.cache_key != indexed.cache_key || record.answer_digest != indexed.answer_digest {
        return ryeos_state::ReplayRecordVerification::IntegrityFailure {
            reason: format!(
                "indexed dispatch-effect row contradicts record {}",
                indexed.record_hash
            ),
        };
    }
    if let ryeos_effect_contract::DispatchEffectAnswer::Retained {
        result,
        retained_result,
    } = &record.answer
    {
        let verified = (|| -> Result<()> {
            let limits = state.node_policy.require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?.closure_limits()?;
            let cas = authority.cas_store()?;
            let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                &cas,
                [indexed.record_hash.clone()],
                limits,
            )?;
            if !closure.is_complete() {
                anyhow::bail!(
                    "retained dispatch record has an incomplete or malformed typed closure"
                );
            }
            if !closure.large_object_hashes.is_empty() {
                let large_store = authority.large_object_store()?;
                for hash in &closure.large_object_hashes {
                    let sidecar = large_store
                        .sidecar(hash)?
                        .context("retained dispatch record large object has no sidecar")?;
                    large_store.verify_resident_object(hash, sidecar.size)?;
                }
            }
            // This is typed-closure residency, not a second full product byte
            // scrub. Current-product admission below owns byte verification.
            let retained = ryeos_state::object_closure::load_exact_cas_object_with_cas(
                &cas, retained_result.object_hash(),
                (ryeos_state::external_content::products::accepted_result::MAX_PRODUCT_BUILD_ACCEPTED_RESULT_BYTES as u64).min(limits.max_object_bytes),
            )?;
            if retained != *result {
                anyhow::bail!("retained dispatch answer contradicts its exact accepted object");
            }
            Ok(())
        })();
        if let Err(error) = verified {
            return ryeos_state::ReplayRecordVerification::IntegrityFailure {
                reason: format!("retained dispatch record closure is invalid: {error:#}"),
            };
        }
    }
    let evidence_hash = &record.admission_evidence_hash;
    let evidence = match authority
        .cas_store()
        .and_then(|cas| cas.get_object(evidence_hash))
    {
        Ok(Some(value)) => value,
        Ok(None) => {
            return ryeos_state::ReplayRecordVerification::IntegrityFailure {
                reason: format!(
                    "dispatch-effect record {} names missing admission evidence {evidence_hash}",
                    indexed.record_hash
                ),
            };
        }
        Err(error) => {
            return ryeos_state::ReplayRecordVerification::Unavailable {
                reason: format!("could not read admission evidence {evidence_hash}: {error:#}"),
            };
        }
    };
    match ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(evidence).and_then(
        |capsule| {
            let observed = capsule.content_hash()?;
            if observed != *evidence_hash {
                anyhow::bail!(
                    "admission evidence content hash {observed} contradicts {evidence_hash}"
                );
            }
            let (subject_ref, launch_authority_digest, caller_authority_digest) =
                dispatch_subject_components_from_capsule(&capsule)?;
            if subject_ref != record.identity.subject.subject_ref
                || launch_authority_digest != record.identity.subject.launch_authority_digest
                || caller_authority_digest != record.identity.subject.caller_authority_digest
            {
                anyhow::bail!(
                    "dispatch-effect admission evidence contradicts its admitted subject"
                );
            }
            Ok(())
        },
    ) {
        Ok(()) => ryeos_state::ReplayRecordVerification::Verified,
        Err(error) => ryeos_state::ReplayRecordVerification::IntegrityFailure {
            reason: format!("dispatch-effect admission evidence is invalid: {error:#}"),
        },
    }
}

fn dispatch_subject_components_from_capsule(
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
) -> Result<(String, String, String)> {
    if capsule.requires_unversioned_secret_input()? {
        anyhow::bail!(
            "durable dispatch subject requires late-bound secret input with no sealed generation authority"
        );
    }
    let subject_ref = capsule
        .exact_program
        .get("item_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("admitted dispatch capsule has no subject ref"))?
        .to_string();
    // An accepted build consumes the entire exact producer source generation,
    // not only a dispatched leaf's definition closure. Keep this distinction
    // in the subject identity; output hashes and attempt IDs remain answers.
    let launch_authority_digest = if let Some(outputs) =
        capsule.project_authority.workspace_outputs()
    {
        let producer =
            ryeos_state::external_content::products::admission::admitted_product_producer(capsule)?;
        ryeos_state::external_content::products::admission::admitted_product_recipe(
            capsule,
            &outputs.partition.recipe_binding,
        )?;
        product_build_subject_digest(
            &producer,
            &outputs.partition.partition_identity,
            &capsule.launch_authority().product_build_replay_digest()?,
        )?
    } else {
        capsule.launch_authority_digest()?
    };
    Ok((
        subject_ref,
        launch_authority_digest,
        capsule.dispatch_effect_caller_authority_digest()?,
    ))
}

fn product_build_subject_digest(
    producer: &ryeos_state::external_content::products::ProductProducerAdmission,
    partition_identity: &str,
    replay_authority_digest: &str,
) -> Result<String> {
    producer.validate()?;
    ryeos_effect_contract::require_hex64("producer partition identity", partition_identity)?;
    ryeos_effect_contract::require_hex64("producer replay authority", replay_authority_digest)?;
    ryeos_state::objects::canonical_value_digest(&json!({
        "schema": "ryeos.product_build_dispatch_subject.v2",
        "replay_authority_digest": replay_authority_digest,
        "producer_project_snapshot_hash": producer.producer_project_snapshot_hash,
        "producer_effective_definition_digest": producer.effective_definition_digest,
        "producer_parameters_digest": producer.admitted_parameters_digest,
        "producer_partition_identity": partition_identity,
    }))
}

pub(crate) enum PreparedManagedDispatchEffect {
    Execute {
        identity: ryeos_effect_contract::DispatchEffectIdentity,
    },
    Replay {
        result: Value,
        dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence,
    },
}

/// Called only after independent output authority and the exact producer
/// capsule have been admitted, and before the reserved child is born.
pub(crate) fn prepare_managed_dispatch_effect(
    state: &AppState,
    resolved: &ResolvedExecutionRequest,
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    prepared: &ryeos_effect_contract::PreparedEffectDispatchAuthority,
) -> Result<PreparedManagedDispatchEffect> {
    prepared.validate()?;
    capsule
        .project_authority
        .workspace_outputs()
        .context("managed durable action requires admitted retained products")?;
    let (subject_ref, launch_authority_digest, caller_authority_digest) =
        dispatch_subject_components_from_capsule(capsule)?;
    if subject_ref != resolved.item_ref {
        anyhow::bail!("managed effect subject differs from its admitted producer");
    }
    let identity = ryeos_effect_contract::DispatchEffectIdentity {
        authorization: prepared.authorization.clone(),
        action_digest: prepared.action_digest.clone(),
        subject: ryeos_effect_contract::AdmittedDispatchSubject {
            subject_ref,
            launch_authority_digest,
            caller_authority_digest,
            effect_class_ceiling: prepared.subject_effect_class_ceiling,
        },
    };
    let cache_key = identity.cache_key()?;
    let authority = super::pinned_state_authority(state)?;
    let _guard = authority.acquire_shared_guard()?;
    let namespace =
        ryeos_state::ReplayIndexNamespace::new(ryeos_effect_contract::EFFECT_REPLAY_NAMESPACE)?;
    match state
        .state_store
        .lookup_replay_record(&namespace, &cache_key, |indexed| {
            verify_dispatch_effect_record(state, &authority, indexed)
        })? {
        ryeos_state::ReplayLookupOutcome::Absent => {
            Ok(PreparedManagedDispatchEffect::Execute { identity })
        }
        ryeos_state::ReplayLookupOutcome::Present(indexed) => {
            // Current product policy and byte verification happen outside the
            // state-store mutex while this guard prevents concurrent GC.
            let record =
                load_verified_dispatch_effect_record(state, &authority, &indexed, Some(capsule))?;
            if record.identity != identity {
                anyhow::bail!("managed build replay contradicts admitted effect identity");
            }
            let ryeos_effect_contract::DispatchEffectAnswer::Retained { .. } = &record.answer
            else {
                anyhow::bail!("managed build replay has no retained product answer");
            };
            Ok(PreparedManagedDispatchEffect::Replay {
                result: record.answer.replay_leaf_envelope(&indexed.record_hash)?,
                dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
                    source: ryeos_runtime::callback_contract::RuntimeDispatchSource::EffectRecord,
                    effect_class: runtime_effect_class(identity.authorization.class),
                    action_digest: identity.action_digest,
                    effect_identity: Some(cache_key),
                    publication:
                        ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable,
                    record_hash: Some(indexed.record_hash.clone()),
                    replayed_from: Some(indexed.record_hash),
                    result_projection: record.answer.result_projection(),
                },
            })
        }
        ryeos_state::ReplayLookupOutcome::Unavailable { reason } => {
            anyhow::bail!("build replay unavailable: {reason}")
        }
        ryeos_state::ReplayLookupOutcome::IntegrityFailure { reason } => {
            anyhow::bail!("build replay integrity failure: {reason}")
        }
    }
}

fn admitted_subject_from_capsule(
    resolved: &ResolvedExecutionRequest,
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    ceiling: ryeos_effect_contract::EffectClass,
) -> Result<ryeos_effect_contract::AdmittedDispatchSubject> {
    // Current effect answers replay values, not workspace mutations. Returning
    // a recorded builder answer without restoring its outputs would falsely
    // report work as complete. Immutable selected-product consumers have no
    // writable output partition and retain ordinary recorded semantics.
    if capsule.project_authority.workspace_outputs().is_some() {
        anyhow::bail!(
            "recorded or sealed execution with workspace outputs requires an admitted replayable output contract; use live producer steps"
        );
    }
    let (capsule_subject_ref, launch_authority_digest, caller_authority_digest) =
        dispatch_subject_components_from_capsule(capsule)?;
    if capsule_subject_ref != resolved.item_ref {
        anyhow::bail!(
            "admitted dispatch capsule subject `{capsule_subject_ref}` contradicts resolved subject `{}`",
            resolved.item_ref
        );
    }
    let subject = ryeos_effect_contract::AdmittedDispatchSubject {
        subject_ref: resolved.item_ref.clone(),
        launch_authority_digest,
        caller_authority_digest,
        effect_class_ceiling: ceiling,
    };
    subject.validate()?;
    Ok(subject)
}

fn load_verified_dispatch_effect_record(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    indexed: &ryeos_state::ReplayIndexRecord,
    expected_current_capsule: Option<&ryeos_state::objects::AdmittedLaunchCapsule>,
) -> Result<ryeos_effect_contract::DispatchEffectRecord> {
    // Callers have verified the full immutable record outside the index lock.
    // Hold CAS authority before reloading, and compare the exact index row;
    // repeating large closure verification here would scrub every hit twice.
    let guard = authority.acquire_shared_guard()?;
    let value = authority
        .cas_store()?
        .get_object(&indexed.record_hash)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "verified dispatch-effect record {} disappeared",
                indexed.record_hash
            )
        })?;
    let record = ryeos_effect_contract::DispatchEffectRecord::from_current_value(&value)?;
    if record.cache_key != indexed.cache_key || record.answer_digest != indexed.answer_digest {
        anyhow::bail!("dispatch-effect record changed after indexed verification");
    }
    if let ryeos_effect_contract::DispatchEffectAnswer::Retained {
        result,
        retained_result,
    } = &record.answer
    {
        let retained = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &authority.cas_store()?,
            retained_result.object_hash(),
            ryeos_state::external_content::products::accepted_result::MAX_PRODUCT_BUILD_ACCEPTED_RESULT_BYTES as u64,
        )?;
        if retained != *result {
            anyhow::bail!("retained dispatch answer contradicts its exact result object");
        }
        // Product witnesses attest the actual historical producer, including
        // its exact accounting reservation. Reuse proves current equivalence
        // separately; it must not rewrite that testimony to this attempt.
        let historical_capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(
            authority
                .cas_store()?
                .get_object(&record.admission_evidence_hash)?
                .context("retained effect admission evidence disappeared")?,
        )?;
        if let Some(current) = expected_current_capsule {
            let current_subject = dispatch_subject_components_from_capsule(current)?;
            let historical_subject = dispatch_subject_components_from_capsule(&historical_capsule)?;
            if current_subject != historical_subject {
                anyhow::bail!(
                    "retained product effect does not match current admitted reuse authority"
                );
            }
        }
        ryeos_app::operator_external_content::product_build::verify_current(
            state,
            &guard,
            &retained,
            &historical_capsule,
        )?;
    }
    Ok(record)
}

/// Reconstruct a durable callback answer from one already-terminal child.
///
/// The child capsule supplies the exact admitted subject. A previously
/// published effect is replayed without contacting the callee. If the daemon
/// died after terminal settlement but before effect publication, the signed
/// terminal response is sufficient to finish that publication; the action is
/// never executed again.
fn dispatch_effect_admission_capsule(
    state: &AppState,
    terminal_thread_id: &str,
) -> Result<ryeos_state::objects::AdmittedLaunchCapsule> {
    let (chain_root_id, _, terminal_capsule) = state
        .state_store
        .admitted_launch_capsule_with_coordinates(terminal_thread_id)?
        .context("durable action terminal has no admitted capsule")?;
    if terminal_capsule
        .project_authority
        .workspace_outputs()
        .is_none()
    {
        return Ok(terminal_capsule);
    }
    // A managed build's input is its admitted root. Continuation may advance
    // source/output generations, but cannot replace that original request
    // identity. Acceptance separately proves the exact root→terminal lineage.
    let root_capsule = state
        .state_store
        .admitted_launch_capsule(&chain_root_id)?
        .context("retained build root admission is unavailable")?;
    let root_outputs = root_capsule
        .project_authority
        .workspace_outputs()
        .context("retained build root has no admitted output partition")?;
    let terminal_outputs = terminal_capsule
        .project_authority
        .workspace_outputs()
        .context("retained build terminal lost output authority")?;
    if root_outputs.partition != terminal_outputs.partition {
        anyhow::bail!("retained build continuation changed its output partition");
    }
    Ok(root_capsule)
}

pub(crate) fn recover_terminal_dispatch_effect(
    state: &AppState,
    child_thread_id: &str,
    expected_subject_ref: &str,
    prepared: &ryeos_effect_contract::PreparedEffectDispatchAuthority,
    terminal_response: &Value,
) -> Result<Value> {
    prepared.validate()?;
    let authority = super::pinned_state_authority(state)?;
    let _recovery_guard = authority.acquire_shared_guard()?;
    let capsule = dispatch_effect_admission_capsule(state, child_thread_id)?;
    let (subject_ref, launch_authority_digest, caller_authority_digest) =
        dispatch_subject_components_from_capsule(&capsule)?;
    if subject_ref != expected_subject_ref {
        anyhow::bail!(
            "terminal durable action child subject `{subject_ref}` contradicts `{expected_subject_ref}`"
        );
    }
    let subject = ryeos_effect_contract::AdmittedDispatchSubject {
        subject_ref,
        launch_authority_digest,
        caller_authority_digest,
        effect_class_ceiling: prepared.subject_effect_class_ceiling,
    };
    let identity = ryeos_effect_contract::DispatchEffectIdentity {
        authorization: prepared.authorization.clone(),
        action_digest: prepared.action_digest.clone(),
        subject,
    };
    identity.validate()?;
    let cache_key = identity.cache_key()?;
    let namespace =
        ryeos_state::ReplayIndexNamespace::new(ryeos_effect_contract::EFFECT_REPLAY_NAMESPACE)?;
    match state
        .state_store
        .lookup_replay_record(&namespace, &cache_key, |indexed| {
            verify_dispatch_effect_record(state, &authority, indexed)
        })? {
        ryeos_state::ReplayLookupOutcome::Present(indexed) => {
            let record =
                load_verified_dispatch_effect_record(state, &authority, &indexed, Some(&capsule))?;
            if record.identity != identity {
                anyhow::bail!(
                    "verified dispatch-effect record {} contradicts recovered child authority",
                    indexed.record_hash
                );
            }
            let result = record.answer.replay_leaf_envelope(&indexed.record_hash)?;
            Ok(json!({
                "thread": null,
                "result": result,
                "dispatch": ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
                    source: ryeos_runtime::callback_contract::RuntimeDispatchSource::EffectRecord,
                    effect_class: runtime_effect_class(identity.authorization.class),
                    action_digest: identity.action_digest,
                    effect_identity: Some(cache_key),
                    publication: ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable,
                    record_hash: Some(indexed.record_hash.clone()),
                    replayed_from: Some(indexed.record_hash),
                    result_projection: record.answer.result_projection(),
                },
            }))
        }
        ryeos_state::ReplayLookupOutcome::Absent => {
            let publication = publish_dispatch_effect_record(
                state,
                identity.clone(),
                terminal_response,
                child_thread_id,
            )?;
            let mut response = terminal_response.clone();
            let object = response.as_object_mut().ok_or_else(|| {
                anyhow::anyhow!("terminal durable action response is not an object")
            })?;
            if let ryeos_effect_contract::DispatchEffectAnswer::Retained { result, .. } =
                &publication.answer
            {
                object.insert(
                    "result".to_owned(),
                    json!({
                        "outcome_code": null,
                        "result": result,
                        "error": null,
                        "artifacts": [],
                    }),
                );
            }
            object.insert(
                "dispatch".to_string(),
                serde_json::to_value(ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
                    source: ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed,
                    effect_class: runtime_effect_class(identity.authorization.class),
                    action_digest: identity.action_digest,
                    effect_identity: Some(cache_key),
                    publication: publication.publication,
                    record_hash: Some(publication.record_hash),
                    replayed_from: None,
                    result_projection: publication.answer.result_projection(),
                })?,
            );
            Ok(response)
        }
        ryeos_state::ReplayLookupOutcome::Unavailable { reason } => {
            anyhow::bail!("dispatch-effect recovery is unavailable: {reason}")
        }
        ryeos_state::ReplayLookupOutcome::IntegrityFailure { reason } => {
            anyhow::bail!("dispatch-effect recovery integrity failure: {reason}")
        }
    }
}

struct DispatchEffectPublication {
    record_hash: String,
    publication: ryeos_runtime::callback_contract::RuntimeDispatchPublication,
    answer: ryeos_effect_contract::DispatchEffectAnswer,
}

fn publish_dispatch_effect_record(
    state: &AppState,
    identity: ryeos_effect_contract::DispatchEffectIdentity,
    response: &Value,
    produced_by_thread: &str,
) -> Result<DispatchEffectPublication> {
    let cache_key = identity.cache_key()?;
    let state_authority = state.state_store.pinned_state_authority()?;
    let publication_guard = state_authority.acquire_shared_guard()?;
    let observed_capsule = state
        .state_store
        .admitted_launch_capsule(produced_by_thread)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "durable dispatch producer {produced_by_thread} has no admitted launch capsule"
            )
        })?;
    let capsule = dispatch_effect_admission_capsule(state, produced_by_thread)?;
    // Keep the accepted object alive until the existing replay record becomes
    // its durable owner. Product captures use their ordinary immutable heads;
    // this operation introduces no additional cache/head or unguarded gap.
    let (answer, observed_response_digest) =
        if capsule.project_authority.workspace_outputs().is_some() {
            let accepted = ryeos_app::operator_external_content::product_build::accept_terminal(
                state,
                &capsule,
                produced_by_thread,
                &publication_guard,
            )?;
            let result = accepted.to_value()?;
            ryeos_app::operator_external_content::product_build::verify_current(
                state,
                &publication_guard,
                &result,
                &capsule,
            )?;
            let object_hash = state_authority.cas_store()?.store_object(&result)?;
            (
                ryeos_effect_contract::DispatchEffectAnswer::Retained {
                    result,
                    retained_result:
                        ryeos_effect_contract::RetainedEffectResult::ProductBuildAcceptedResult {
                            object_hash,
                        },
                },
                ryeos_state::objects::canonical_value_digest(response)?,
            )
        } else {
            let normalized = ryeos_runtime::normalize_dispatch_effect(response)
                .context("durable dispatch completed without a replay-safe answer")?;
            (normalized.answer, normalized.observed_response_digest)
        };
    let answer_digest = answer.digest()?;
    let realization = observed_capsule.verify_retained_execution_realization(
        &state_authority.cas_store()?,
        &state_authority.large_object_store()?,
        state_authority.trust_store(),
    )?;
    let identity_value = state_authority
        .cas_store()?
        .get_object(&realization.substrate_identity_hash)?
        .ok_or_else(|| anyhow::anyhow!("dispatch execution substrate identity is missing"))?;
    let substrate_identity =
        ryeos_state::objects::ExecutionIdentity::from_current_value(&identity_value)?;
    let record = ryeos_effect_contract::DispatchEffectRecord {
        schema: ryeos_effect_contract::EFFECT_RECORD_SCHEMA_VERSION,
        kind: ryeos_effect_contract::EFFECT_RECORD_KIND.to_string(),
        cache_key: cache_key.clone(),
        identity,
        admission_evidence_hash: capsule.content_hash()?,
        answer_digest: answer_digest.clone(),
        answer: answer.clone(),
        first_observation: ryeos_effect_contract::EffectFirstObservation {
            produced_by_thread: produced_by_thread.to_string(),
            response_digest: observed_response_digest,
            observed_at: ryeos_effect_contract::canonical_observation_timestamp_now(),
            execution_identity_digest: Some(substrate_identity.identity_digest()?),
            execution_identity_attestation_hash: Some(
                realization.substrate_attestation_hash.clone(),
            ),
            // The first observation belongs to the final executing placement.
            // This is still an admitted realization, not an observed-realization
            // object. The original admission remains owned by the record's
            // separate admission_evidence_hash above.
            admitted_execution_realization_hash: Some(observed_capsule.execution_realization_hash),
            observed_execution_realization_hash: None,
        },
    };
    let value = record.to_value()?;
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let record_hash = authority
        .cas_store()?
        .store_object(&value)
        .context("dispatch-effect CAS publication is unavailable")?;
    authority.ensure_guard(&guard)?;
    let candidate = ryeos_state::ReplayIndexRecord {
        cache_key,
        answer_digest,
        record_hash,
    };
    let replay_namespace =
        ryeos_state::ReplayIndexNamespace::new(ryeos_effect_contract::EFFECT_REPLAY_NAMESPACE)?;
    // Verify immutable bytes before acquiring the global state mutex. The
    // publication callback may only recognize these exact previously checked
    // rows; a concurrently appearing incumbent is unavailable, not trusted.
    match verify_dispatch_effect_record(state, &authority, &candidate) {
        ryeos_state::ReplayRecordVerification::Verified => {}
        ryeos_state::ReplayRecordVerification::Unavailable { reason } => {
            anyhow::bail!("dispatch-effect candidate verification unavailable: {reason}");
        }
        ryeos_state::ReplayRecordVerification::IntegrityFailure { reason } => {
            anyhow::bail!("dispatch-effect candidate integrity failure: {reason}");
        }
    }
    let incumbent = match state.state_store.lookup_replay_record(
        &replay_namespace,
        &candidate.cache_key,
        |indexed| verify_dispatch_effect_record(state, &authority, indexed),
    )? {
        ryeos_state::ReplayLookupOutcome::Absent => None,
        ryeos_state::ReplayLookupOutcome::Present(indexed) => Some(indexed),
        ryeos_state::ReplayLookupOutcome::Unavailable { reason } => {
            anyhow::bail!("dispatch-effect incumbent verification unavailable: {reason}");
        }
        ryeos_state::ReplayLookupOutcome::IntegrityFailure { reason } => {
            anyhow::bail!("dispatch-effect incumbent integrity failure: {reason}");
        }
    };
    let outcome = state.state_store.with_state_db(|db| {
        db.publish_replay_record(&replay_namespace, &candidate, |indexed| {
            if indexed == &candidate || incumbent.as_ref() == Some(indexed) {
                ryeos_state::ReplayRecordVerification::Verified
            } else {
                ryeos_state::ReplayRecordVerification::Unavailable {
                    reason: "dispatch-effect incumbent changed after immutable verification; retry exact publication".into(),
                }
            }
        })
    })?;
    match outcome {
        ryeos_state::ReplayPublishOutcome::Inserted { record_hash } => {
            Ok(DispatchEffectPublication {
                record_hash,
                publication: ryeos_runtime::callback_contract::RuntimeDispatchPublication::Inserted,
                answer,
            })
        }
        ryeos_state::ReplayPublishOutcome::Folded { record_hash } => {
            Ok(DispatchEffectPublication {
                record_hash,
                publication: ryeos_runtime::callback_contract::RuntimeDispatchPublication::Folded,
                answer,
            })
        }
        ryeos_state::ReplayPublishOutcome::Unavailable { reason } => {
            anyhow::bail!("dispatch-effect index is unavailable: {reason}")
        }
        ryeos_state::ReplayPublishOutcome::IntegrityConflict {
            existing_record_hash,
            candidate_record_hash,
        } => anyhow::bail!(
            "dispatch-effect answer conflict: existing={existing_record_hash}, candidate={candidate_record_hash}"
        ),
        ryeos_state::ReplayPublishOutcome::IntegrityFailure { reason } => {
            anyhow::bail!("dispatch-effect index integrity failure: {reason}")
        }
    }
}

/// Result of a detached execution launch.
pub struct DetachedResult {
    pub running_thread: ThreadDetail,
}

struct ProtocolLaunchEnv {
    bindings: Vec<EnvBinding>,
    callback_token: Option<String>,
    thread_auth_token: Option<String>,
    isolation_daemon_socket_path: Option<PathBuf>,
}

/// Resolve the signed subprocess protocol declared by the item's actual kind.
/// Ordinary runner execution owns terminal and callback-free framed output.
/// Managed callback envelopes remain on the runtime path.
fn resolved_terminator_protocol<'a>(
    engine: &'a ryeos_engine::engine::Engine,
    resolved: &ResolvedExecutionRequest,
) -> Result<&'a ryeos_engine::protocols::VerifiedProtocol> {
    let kind = &resolved.resolved_item.kind;
    let protocol =
        ryeos_app::thread_lifecycle::resolve_direct_terminator_protocol(engine, resolved)?;
    crate::dispatch::validate_ordinary_protocol_contract(protocol, kind)
        .map_err(|error| anyhow::anyhow!(error))?;
    if protocol.descriptor.stdout.shape
        == ryeos_engine::protocol_vocabulary::StdoutShape::StreamingChunks
    {
        crate::dispatch::validate_direct_result_retention(
            protocol,
            resolved
                .root_admission
                .as_ref()
                .context("direct output lacks root admission")?
                .resolved_result_policy()
                .retention,
            kind,
        )?;
    }
    Ok(protocol)
}

/// Build only the environment declared by the verified protocol. Callback and
/// thread-auth authority are minted lazily from typed descriptor requirements;
/// callback-free tools therefore receive neither credentials nor daemon-socket
/// isolation access.
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
    handler_context: Option<&ryeos_app::handler_context::HandlerContext>,
    current_site_id: &str,
    origin_site_id: &str,
    provenance: ExecutionProvenance,
    item_ref: &str,
    root_raw_content_digest: String,
    // Bundle identity for the token, derived once from the resolved canonical
    // ref by the caller via `effective_bundle_id_for_request` so it matches the
    // identity the runtime-cap minter used. `item_ref` stays the requested ref
    // for provenance/display.
    effective_bundle_id: Option<String>,
    launch_owner: &str,
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
    let project_state_scope = provenance.project_authority().project_state_scope_id()?;

    // Run-scoped credentials cover the run's full duration plus finalization.
    let ttl = launch_token_ttl(duration_seconds);
    let callback_token = callback_token_requested
        .then(|| {
            state
                .callback_tokens
                .generate_with_context(
                    thread_id,
                    callback_project_path.to_path_buf(),
                    ttl,
                    effective_caps.clone(),
                    provenance,
                    effective_bundle_id,
                    Some(item_ref.to_string()),
                    root_raw_content_digest.clone(),
                    None,
                    serde_json::Value::Null,
                    0,
                )
                .token
        })
        .map(|token| {
            if !state
                .callback_tokens
                .set_launch_owner(&token, launch_owner.to_string())
            {
                anyhow::bail!("fresh callback capability disappeared before owner binding");
            }
            Ok(token)
        })
        .transpose()?;
    let callback_handler_context = handler_context
        .map(|context| {
            context.narrowed_for_execution(effective_caps.clone(), current_site_id, origin_site_id)
        })
        .transpose()?;
    let thread_auth_token = thread_auth_requested
        .then(|| {
            state.thread_auth.mint(
                thread_id,
                acting_principal.to_string(),
                effective_caps,
                callback_handler_context,
                current_site_id,
                origin_site_id,
                ttl,
            )
        })
        .transpose()?
        .map(|authority| authority.token);

    let isolation_daemon_socket_path =
        callback_ipc_requested.then(|| state.config.uds_path.clone());
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
        timeout: lillux::time::Duration::from_secs(0),
        item_ref,
        thread_id: thread_id.to_string(),
        project_path: project_path.to_path_buf(),
        acting_principal: acting_principal.to_string(),
        cas_root,
        callback_token: callback_token.clone(),
        callback_socket_path,
        project_state_scope,
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
        isolation_daemon_socket_path,
    })
}

fn verify_fresh_root_admission(params: &ExecutionParams) -> Result<()> {
    let Some(admission) = params.resolved.root_admission.as_ref() else {
        // Existing rows and continuation successors inherit the authoritative
        // root policy; they never reinterpret mutable source content here.
        return Ok(());
    };
    admission.ensure_matches_provenance(&params.provenance)?;
    let engine = admission.request_engine();
    admission.ensure_matches_request(&params.resolved)?;
    admission.ensure_matches_subject(engine, admission.verified_subject(), &params.resolved.kind)
}

#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct ExecutionNotRestartEligible {
    pub item_ref: String,
    pub reason: String,
    pub remediation: String,
}

fn ensure_restart_eligible_artifact(
    lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    item_ref: &str,
    direct_executable_identity: Option<&ryeos_state::objects::DirectExecutableIdentity>,
) -> Result<()> {
    if lifecycle_authority.recovery
        == ryeos_state::objects::ExecutionRecoveryAuthority::RestartRecoverable
        && matches!(
            direct_executable_identity,
            Some(ryeos_state::objects::DirectExecutableIdentity::NodePolicy)
        )
    {
        return Err(anyhow::Error::new(ExecutionNotRestartEligible {
            item_ref: item_ref.to_string(),
            reason: "the direct executable is authorized only by mutable node policy, not verified content identity".to_string(),
            remediation: "install or resolve a content-verified executable, or use an explicitly request-scoped offline execution surface".to_string(),
        }));
    }
    Ok(())
}

/// Fresh direct realization state carried from finalization to spawn: the
/// staged-root publication to finish once the row is durable, and the bound
/// mounts plus generation leases to hold for the runtime's lifetime.
struct DirectExternalRealizations {
    publication: Option<super::PendingCasPublication>,
    retained_resolution: ryeos_engine::resolution::ResolutionOutput,
}

/// Mechanical input delivery derived from the already-finalized retained
/// resolution. This never participates in program, capsule, or effect
/// identity; it exists only for the lifetime of one process launch.
pub(crate) struct PreparedProcessInputs {
    pub(crate) path: PathBuf,
    pub(crate) lifeline: Option<Arc<TempDirGuard>>,
    pub(crate) isolation_project_authority: ryeos_engine::isolation::IsolationProjectAuthority,
    pub(crate) isolation_immutable_project: Option<ryeos_state::PinnedProjectMaterialization>,
    pub(crate) isolation_live_access_authority:
        Option<ryeos_engine::isolation::IsolationLiveAccessAuthority>,
    pub(crate) external: Option<super::external_content::BoundExternalRealizations>,
    pub(crate) source: Option<super::source_closure::BoundSourceClosure>,
}

fn retained_resolution_has_filesystem_bindings(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> Result<bool> {
    let has_source = resolution
        .composed
        .derived
        .contains_key(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY);
    // Runtime-root realizations are not project fold-back exclusions, but
    // still require exact filesystem binding before this process can launch.
    let has_external = resolution
        .composed
        .derived
        .get(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY)
        .map(ryeos_state::objects::ExternalContentRealizationSet::from_value)
        .transpose()?
        .is_some_and(|realizations| !realizations.is_empty());
    Ok(has_source || has_external)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessProjectClass {
    Live,
    Projectless,
    PinnedReadOnly,
    PinnedCow,
}

fn process_project_class(provenance: &ExecutionProvenance) -> ProcessProjectClass {
    match provenance.project_authority() {
        ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. } => {
            ProcessProjectClass::Live
        }
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => {
            ProcessProjectClass::Projectless
        }
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            ..
        } => ProcessProjectClass::PinnedReadOnly,
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
            ..
        } => ProcessProjectClass::PinnedCow,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessInputRootSelection {
    Existing,
    SparsePrivate,
    PinnedReadOnlyPrivate,
    CandidateIntegrationPrivate,
}

/// Physical scratch is launch authority, not a logical project context. Keep
/// projectless plans projectless while handing their already-owned input root
/// to isolation. Other launches retain their existing project-root routing.
fn projectless_isolation_workspace(
    project: ProcessProjectClass,
    process_path: &Path,
) -> Option<PathBuf> {
    match project {
        ProcessProjectClass::Projectless => Some(process_path.to_path_buf()),
        ProcessProjectClass::Live
        | ProcessProjectClass::PinnedReadOnly
        | ProcessProjectClass::PinnedCow => None,
    }
}

fn select_process_input_root(
    project: ProcessProjectClass,
    has_bindings: bool,
    isolation_enforced: bool,
    candidate_integration: bool,
    restore_outputs: bool,
) -> ProcessInputRootSelection {
    if candidate_integration {
        return ProcessInputRootSelection::CandidateIntegrationPrivate;
    }
    if restore_outputs {
        return ProcessInputRootSelection::PinnedReadOnlyPrivate;
    }
    match (project, has_bindings, isolation_enforced) {
        (ProcessProjectClass::Live, true, _) => ProcessInputRootSelection::SparsePrivate,
        (ProcessProjectClass::PinnedReadOnly, _, false) => {
            ProcessInputRootSelection::PinnedReadOnlyPrivate
        }
        _ => ProcessInputRootSelection::Existing,
    }
}

fn bindings_require_private_copy(
    project: ProcessProjectClass,
    root: ProcessInputRootSelection,
    has_bindings: bool,
    isolation_enforced: bool,
) -> bool {
    has_bindings
        && !isolation_enforced
        && (root != ProcessInputRootSelection::Existing
            || matches!(
                project,
                ProcessProjectClass::Projectless | ProcessProjectClass::PinnedCow
            ))
}

/// Mount attachment may not create entries beneath a bound source directory.
/// The executor therefore prepares exact empty targets in its freshly owned
/// sparse input root, before handing that root to isolation. No admitted bytes
/// are copied and no live project or materialization cache is modified.
fn prepare_sparse_input_mount_target(
    root: &lillux::PinnedDirectory,
    relative: &str,
    kind: ryeos_engine::external_content::ExternalContentKind,
) -> Result<()> {
    ryeos_state::objects::validate_canonical_project_relative_path(relative)?;
    let (parent, name) = super::pinned_output_parent(root, relative)?;
    match kind {
        ryeos_engine::external_content::ExternalContentKind::Tree => {
            // The owned root already contains its control-directory skeleton.
            // Reuse only a real directory; this no-follow primitive still
            // refuses a regular file or symlink at the exact target.
            parent.open_or_create_child(&name, 0o700)?;
        }
        ryeos_engine::external_content::ExternalContentKind::File => {
            parent.open_regular_create(&name, true, true, 0o600)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod process_input_selection_tests {
    use super::*;

    #[test]
    fn sparse_input_targets_are_empty_and_leave_admitted_sources_untouched() {
        use ryeos_engine::external_content::ExternalContentKind::{File, Tree};
        let private = tempfile::tempdir().unwrap();
        let live = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(live.path().join(".ai/tools/test")).unwrap();
        std::fs::write(live.path().join(".ai/tools/test/main.py"), b"original").unwrap();
        let root = lillux::PinnedDirectory::open(private.path())
            .unwrap()
            .unwrap();
        for (relative, kind) in [
            (".ai/tools/test", Tree),
            ("vendor/simulator", Tree),
            ("contracts/input.json", File),
        ] {
            prepare_sparse_input_mount_target(&root, relative, kind).unwrap();
        }
        assert!(private.path().join(".ai/tools/test").is_dir());
        assert!(private.path().join("vendor/simulator").is_dir());
        assert_eq!(
            std::fs::read(private.path().join("contracts/input.json")).unwrap(),
            b""
        );
        assert!(!private.path().join(".ai/tools/test/main.py").exists());
        assert_eq!(
            std::fs::read(live.path().join(".ai/tools/test/main.py")).unwrap(),
            b"original"
        );
        assert!(!live.path().join("vendor").exists());
        assert!(!live.path().join("contracts").exists());
        assert!(prepare_sparse_input_mount_target(&root, "contracts/input.json", File).is_err());
    }

    #[test]
    fn sparse_input_targets_refuse_noncanonical_paths_and_symlink_parents() {
        use ryeos_engine::external_content::ExternalContentKind::Tree;
        let private = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(private.path())
            .unwrap()
            .unwrap();
        for relative in [
            "",
            "/escape",
            "../escape",
            "one/../escape",
            "one//two",
            "one/",
        ] {
            assert!(
                prepare_sparse_input_mount_target(&root, relative, Tree).is_err(),
                "{relative}"
            );
        }
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), private.path().join("link")).unwrap();
            assert!(prepare_sparse_input_mount_target(&root, "link/escape", Tree).is_err());
            assert!(!outside.path().join("escape").exists());
        }
    }

    #[test]
    fn sparse_input_targets_reuse_owned_directories_but_refuse_other_leaf_types() {
        use ryeos_engine::external_content::ExternalContentKind::Tree;
        let private = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(private.path())
            .unwrap()
            .unwrap();
        let existing = root
            .create_child(std::ffi::OsStr::new(".ai"), 0o700)
            .unwrap();
        let identity = existing.device_inode().unwrap();
        prepare_sparse_input_mount_target(&root, ".ai", Tree).unwrap();
        assert_eq!(
            root.open_child_directory(std::ffi::OsStr::new(".ai"))
                .unwrap()
                .unwrap()
                .device_inode()
                .unwrap(),
            identity
        );
        std::fs::write(private.path().join("file"), b"unchanged").unwrap();
        assert!(prepare_sparse_input_mount_target(&root, "file", Tree).is_err());
        assert_eq!(
            std::fs::read(private.path().join("file")).unwrap(),
            b"unchanged"
        );
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), private.path().join("link")).unwrap();
            assert!(prepare_sparse_input_mount_target(&root, "link", Tree).is_err());
            assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn sparse_input_targets_follow_owned_descriptor_not_replacement_path() {
        use ryeos_engine::external_content::ExternalContentKind::Tree;
        let base = tempfile::tempdir().unwrap();
        let original = base.path().join("private");
        let retained = base.path().join("retained");
        std::fs::create_dir(&original).unwrap();
        let root = lillux::PinnedDirectory::open(&original).unwrap().unwrap();
        std::fs::rename(&original, &retained).unwrap();
        std::fs::create_dir(&original).unwrap();
        prepare_sparse_input_mount_target(&root, ".ai/tools/test", Tree).unwrap();
        assert!(retained.join(".ai/tools/test").is_dir());
        assert!(!original.join(".ai").exists());
    }

    #[test]
    fn projectless_spawn_uses_exact_owned_scratch_without_a_logical_project() {
        let scratch = Path::new("/exact-owned-execution/scratch");
        assert_eq!(
            projectless_isolation_workspace(ProcessProjectClass::Projectless, scratch),
            Some(scratch.to_path_buf())
        );
        for project in [
            ProcessProjectClass::Live,
            ProcessProjectClass::PinnedReadOnly,
            ProcessProjectClass::PinnedCow,
        ] {
            assert_eq!(projectless_isolation_workspace(project, scratch), None);
        }
    }

    #[test]
    fn exact_input_root_selection_matrix_is_closed() {
        assert_eq!(
            select_process_input_root(ProcessProjectClass::Live, false, false, false, false),
            ProcessInputRootSelection::Existing
        );
        assert_eq!(
            select_process_input_root(ProcessProjectClass::Live, true, false, false, false),
            ProcessInputRootSelection::SparsePrivate
        );
        assert_eq!(
            select_process_input_root(ProcessProjectClass::Live, true, true, false, false),
            ProcessInputRootSelection::SparsePrivate
        );
        assert_eq!(
            select_process_input_root(
                ProcessProjectClass::PinnedReadOnly,
                false,
                false,
                false,
                false,
            ),
            ProcessInputRootSelection::PinnedReadOnlyPrivate
        );
        assert_eq!(
            select_process_input_root(
                ProcessProjectClass::PinnedReadOnly,
                true,
                true,
                false,
                false,
            ),
            ProcessInputRootSelection::Existing
        );
        assert_eq!(
            select_process_input_root(ProcessProjectClass::PinnedCow, true, false, false, false),
            ProcessInputRootSelection::Existing
        );
        assert_eq!(
            select_process_input_root(ProcessProjectClass::PinnedCow, true, true, true, false),
            ProcessInputRootSelection::CandidateIntegrationPrivate
        );
        assert_eq!(
            select_process_input_root(
                ProcessProjectClass::PinnedReadOnly,
                false,
                false,
                false,
                true,
            ),
            ProcessInputRootSelection::PinnedReadOnlyPrivate
        );
    }

    #[test]
    fn disabled_binding_delivery_copies_only_into_private_or_owned_roots() {
        assert!(!bindings_require_private_copy(
            ProcessProjectClass::Live,
            ProcessInputRootSelection::SparsePrivate,
            true,
            true,
        ));
        assert!(bindings_require_private_copy(
            ProcessProjectClass::Live,
            ProcessInputRootSelection::SparsePrivate,
            true,
            false,
        ));
        assert!(bindings_require_private_copy(
            ProcessProjectClass::Projectless,
            ProcessInputRootSelection::Existing,
            true,
            false,
        ));
        assert!(bindings_require_private_copy(
            ProcessProjectClass::PinnedCow,
            ProcessInputRootSelection::Existing,
            true,
            false,
        ));
        assert!(!bindings_require_private_copy(
            ProcessProjectClass::Live,
            ProcessInputRootSelection::Existing,
            false,
            false,
        ));
    }
}

fn validate_process_inputs_outside_workspace_outputs(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    partition: &ryeos_state::objects::WorkspaceOutputPartition,
) -> Result<()> {
    partition.validate()?;
    let mut mounts = super::external_content::admitted_realization_mounts(resolution)?;
    if let Some(source) = super::source_closure::admitted_source_mount(state, resolution)? {
        mounts.push(source);
    }
    for mount in mounts {
        let mount = Path::new(&mount);
        for output in &partition.roots {
            let output_path = Path::new(&output.path);
            if mount.starts_with(output_path) || output_path.starts_with(mount) {
                anyhow::bail!(
                    "workspace output `{}` overlaps process input mount `{}`",
                    output.name,
                    mount.display()
                );
            }
        }
    }
    Ok(())
}

fn restore_process_workspace_output_generation(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    source: &ryeos_state::PinnedProjectMaterialization,
    partition: &ryeos_state::objects::WorkspaceOutputPartition,
    capture_hash: &str,
    budget: &super::external_content::PrivateMaterializationBudget,
) -> Result<()> {
    super::workspace_outputs::admission::validate_current_bounds(state, partition)?;
    let cas = authority.cas_store()?;
    let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        capture_hash,
        ryeos_state::objects::MAX_WORKSPACE_OUTPUT_CAPTURE_BYTES as u64,
    )?;
    let capture = ryeos_state::objects::WorkspaceOutputCapture::from_value(&value)?;
    if capture.partition != *partition
        || capture.result_project_snapshot_hash != source.snapshot_hash()
    {
        anyhow::bail!("workspace output capture contradicts the selected process generation");
    }
    let result = ryeos_state::project_materialization::load_project_snapshot_bounded(
        &cas,
        &capture.result_project_snapshot_hash,
    )?
    .ok_or_else(|| anyhow::anyhow!("workspace output result snapshot is unavailable"))?;
    let policy = ryeos_state::project_materialization::load_project_policy_bounded(
        &cas,
        &partition.project_snapshot_policy_hash,
    )?
    .ok_or_else(|| anyhow::anyhow!("workspace output capture policy is unavailable"))?;
    partition.validate_result_source_policy(&result, &policy)?;
    super::workspace_outputs::restore_workspace_outputs_before_view(
        authority, guard, source, &capture, &policy, budget,
    )
}

pub(crate) fn prepare_process_inputs(
    state: &AppState,
    provenance: &ExecutionProvenance,
    thread_id: &str,
    retained_resolution: &ryeos_engine::resolution::ResolutionOutput,
    base_path: &Path,
) -> Result<PreparedProcessInputs> {
    // `retained_resolution` is the execution being spawned. For a borrowed
    // child this already materializes/mounts the child's finalized source and
    // external realizations into the borrowed workspace while preserving the
    // parent's workspace lifeline. Do not route a managed parent's prepared
    // launch dependencies around this path as a second child environment.
    super::source_closure::validate_external_mount_separation(
        state,
        retained_resolution,
        super::source_closure::SourceMountPlacement::Project,
    )?;
    let has_bindings = retained_resolution_has_filesystem_bindings(retained_resolution)?;
    let project_class = process_project_class(provenance);
    let workspace_outputs = provenance.project_authority().workspace_outputs();
    if let Some(outputs) = workspace_outputs {
        validate_process_inputs_outside_workspace_outputs(
            state,
            retained_resolution,
            &outputs.partition,
        )?;
    }
    let immutable_input_generation = provenance.immutable_workspace_input_generation();
    if let Some(generation) = &immutable_input_generation {
        generation.validate()?;
        if generation.output_capture_hash.is_some() && workspace_outputs.is_none() {
            anyhow::bail!(
                "immutable workspace input source/output pair contradicts subject authority"
            );
        }
    }
    let pinned_read_only_generation = match project_class {
        ProcessProjectClass::PinnedReadOnly => Some(match immutable_input_generation.clone() {
            Some(generation) => generation,
            None => ryeos_state::objects::WorkspaceGenerationPair {
                snapshot_hash: provenance
                    .project_authority()
                    .operational_snapshot_projection()
                    .ok_or_else(|| anyhow::anyhow!("pinned process lost its source generation"))?
                    .to_owned(),
                output_capture_hash: workspace_outputs
                    .and_then(|outputs| outputs.capture_hash.clone()),
            },
        }),
        ProcessProjectClass::Live
        | ProcessProjectClass::Projectless
        | ProcessProjectClass::PinnedCow => None,
    };
    if let Some(generation) = &pinned_read_only_generation {
        generation.validate()?;
    }
    let restore_outputs = pinned_read_only_generation
        .as_ref()
        .is_some_and(|generation| generation.output_capture_hash.is_some());
    let candidate_integration = provenance
        .candidate_evaluation_scope()
        .is_some_and(|scope| {
            matches!(
                &scope.authority().purpose,
                ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. }
            )
        });
    let root_selection = select_process_input_root(
        project_class,
        has_bindings,
        state.isolation.is_enforced(),
        candidate_integration,
        restore_outputs,
    );
    let live_private_root = root_selection == ProcessInputRootSelection::SparsePrivate;
    let pinned_read_only_private_root =
        root_selection == ProcessInputRootSelection::PinnedReadOnlyPrivate;
    let candidate_integration_private_root =
        root_selection == ProcessInputRootSelection::CandidateIntegrationPrivate;

    let (path, lifeline, isolation_project_authority, isolation_live_access_authority) =
        if live_private_root || pinned_read_only_private_root || candidate_integration_private_root
        {
            let (path, lifeline) = ryeos_app::temp_dir_guard::create_admitted_input_workspace(
                &state.config.runtime_root().cache(),
                thread_id,
            )?;
            (
                path,
                Some(lifeline),
                ryeos_engine::isolation::IsolationProjectAuthority::EphemeralScratch,
                None,
            )
        } else {
            (
                base_path.to_path_buf(),
                None,
                provenance.isolation_project_authority(),
                provenance.isolation_live_access_authority()?,
            )
        };

    let private_copy = bindings_require_private_copy(
        project_class,
        root_selection,
        has_bindings,
        state.isolation.is_enforced(),
    );
    let budget =
        (private_copy || pinned_read_only_private_root || candidate_integration_private_root)
            .then(super::external_content::private_materialization_budget)
            .transpose()?;
    if pinned_read_only_private_root || candidate_integration_private_root {
        let snapshot_hash = pinned_read_only_generation
            .as_ref()
            .map(|generation| generation.snapshot_hash.as_str())
            .or_else(|| {
                provenance
                    .project_authority()
                    .operational_snapshot_projection()
            })
            .ok_or_else(|| anyhow::anyhow!("private pinned root lost its source generation"))?;
        let authority = super::pinned_state_authority(state)?;
        let guard = authority.acquire_shared_guard()?;
        let cache = super::cache::MaterializationCache::new(
            state.config.runtime_root().cache().join("snapshots"),
        );
        let (materialized_path, generation_lease, source_materialization) =
            super::checkout_project_snapshot(
                &authority,
                &guard,
                snapshot_hash,
                super::ProjectMaterialization::PrivateWritableWorkspace {
                    target_dir: &path,
                    budget: budget.as_ref(),
                },
                &cache,
            )?;
        if materialized_path != path {
            anyhow::bail!("private pinned project materialization changed its selected root");
        }
        lifeline
            .as_ref()
            .expect("private pinned project root always has a lifeline")
            .retain_lease(generation_lease);
        if let Some(capture_hash) = pinned_read_only_generation
            .as_ref()
            .and_then(|generation| generation.output_capture_hash.as_deref())
        {
            let partition = workspace_outputs
                .map(|outputs| &outputs.partition)
                .ok_or_else(|| anyhow::anyhow!("workspace output capture lost its partition"))?;
            restore_process_workspace_output_generation(
                state,
                &authority,
                &guard,
                &source_materialization,
                partition,
                capture_hash,
                budget
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("workspace output restore has no budget"))?,
            )?;
        }
    }

    let (external, source) = if private_copy {
        let budget = match budget.as_ref() {
            Some(budget) => budget,
            None => {
                anyhow::bail!("private input delivery has no armed materialization budget")
            }
        };
        (
            super::external_content::bind_external_realizations_in_private_workspace_with_budget(
                state,
                retained_resolution,
                &path,
                budget,
            )?,
            super::source_closure::bind_source_in_private_workspace_with_budget(
                state,
                retained_resolution,
                &path,
                budget,
            )?,
        )
    } else {
        (
            super::external_content::bind_external_realizations(state, retained_resolution, &path)?,
            super::source_closure::bind_source(
                state,
                retained_resolution,
                &path,
                super::source_closure::SourceMountPlacement::Project,
            )?,
        )
    };
    if live_private_root && !private_copy {
        let root = lifeline
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sparse input root lost its owner"))?
            .owned_scratch_root()?;
        if let Some(external) = external.as_ref() {
            for (relative, kind) in external.project_mount_targets() {
                prepare_sparse_input_mount_target(root, relative, kind)?;
            }
        }
        if let Some(relative) =
            super::source_closure::admitted_source_mount(state, retained_resolution)?
        {
            prepare_sparse_input_mount_target(
                root,
                &relative,
                ryeos_engine::external_content::ExternalContentKind::Tree,
            )?;
        }
    }
    if let Some(budget) = budget.as_ref() {
        budget.emit_metrics(thread_id)?;
    }
    // Private copied inputs have their own scratch authority. Only an exact
    // unchanged immutable input may carry the retained materialization proof.
    let isolation_immutable_project = if isolation_project_authority
        == ryeos_engine::isolation::IsolationProjectAuthority::ReadOnly
    {
        let proof = provenance.execution_input_materialization();
        if let Some(proof) = proof {
            anyhow::ensure!(
                proof.owns_path(&path)?,
                "immutable process input does not match retained materialization"
            );
        }
        proof.cloned()
    } else {
        None
    };
    Ok(PreparedProcessInputs {
        path,
        lifeline,
        isolation_project_authority,
        isolation_immutable_project,
        isolation_live_access_authority,
        external,
        source,
    })
}

/// Fresh direct-program authority captured before executor compilation. A
/// failed later compile drops the staged publication; it never turns source
/// bytes discovered after compilation into admitted behavior.
pub(crate) struct FinalizedDirectAdmission {
    program: ryeos_engine::effective_program::FinalizedEffectiveProgram,
    publication: Option<super::PendingCasPublication>,
    source_policy: Option<ryeos_engine::launch::plan_builder::ExecutorSourcePolicyProjection>,
}

fn admitted_root_launch_metadata(
    state: &AppState,
    params: &ExecutionParams,
    project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    finalized: FinalizedDirectAdmission,
    prepared_plan: &mut thread_lifecycle::PreparedItemPlan,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
) -> Result<(
    ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    DirectExternalRealizations,
)> {
    let runtime_ref = match &params.runtime_ref {
        Some(runtime_ref) => runtime_ref.clone(),
        None => prepared_plan.runtime_ref()?.to_owned(),
    };
    let FinalizedDirectAdmission {
        program: finalized_program,
        publication: mut external_publication,
        source_policy,
    } = finalized;
    if let Some(parent_thread_id) = params.parent_thread_id.as_deref() {
        let (filesystem, network) =
            super::execution_realization::admitted_parent_isolation_ceilings(
                state,
                parent_thread_id,
            )?;
        prepared_plan.restrict_isolation_authority(filesystem, network);
    }
    if let Some(source_policy) = source_policy.as_ref() {
        source_policy.assert_matches_plan(prepared_plan.execution_plan())?;
    }
    let sealed = SealedRootExecutionRequest::capture_finalized(
        &params.resolved,
        runtime_ref.clone(),
        &finalized_program,
        params.handler_context.as_ref(),
    )?;
    let stable_project_identity = match &project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => None,
        _ => Some(
            ryeos_app::launch_metadata::StableProjectIdentity::from_path(
                params.provenance.original_project_path(),
                &params.resolved.origin_site_id,
            )?,
        ),
    };
    let local_overlay_root = matches!(
        project_authority.environment(),
        ryeos_state::objects::EnvironmentAuthority::ProjectOverlay { .. }
    )
    .then(|| params.provenance.original_project_path().to_path_buf());
    let resume = ResumeContext {
        kind: params.resolved.kind.clone(),
        item_ref: params.resolved.item_ref.clone(),
        ref_bindings: params.resolved.ref_bindings.clone(),
        product_selections: params.resolved.product_selections.clone(),
        launch_mode: params.resolved.launch_mode.clone(),
        parameters: params.parameters.clone(),
        project_context: params.resolved.plan_context.project_context.clone(),
        project_authority,
        lifecycle_authority: params.lifecycle_authority,
        stable_project_identity,
        local_overlay_root,
        original_snapshot_hash: params.provenance.pinned_snapshot_hash().map(str::to_owned),
        original_pushed_head_ref:
            ryeos_app::launch_metadata::OriginalPushedHeadRef::from_provenance(&params.provenance),
        state_root: params
            .provenance
            .state_root_override()
            .map(std::path::Path::to_path_buf),
        current_site_id: params.resolved.current_site_id.clone(),
        origin_site_id: params.resolved.origin_site_id.clone(),
        requested_by: params.resolved.plan_context.requested_by.clone(),
        execution_hints: params.resolved.plan_context.execution_hints.clone(),
        scheduled_fire: params.resolved.plan_context.scheduled_fire.clone(),
        effective_caps: params.effective_caps.clone(),
        parent_delegation_caps: None,
        executor_ref: Some(params.resolved.executor_ref.clone()),
        runtime_ref: Some(runtime_ref),
    };
    let concrete_project_root = match &params.resolved.plan_context.project_context {
        ProjectContext::LocalPath { path } => Some(path.as_path()),
        ProjectContext::None
        | ProjectContext::SnapshotHash { .. }
        | ProjectContext::ProjectRef { .. } => None,
    };
    let admitted_project_root = prepared_plan.bind_logical_project_root(concrete_project_root)?;
    let admitted_artifact_identity =
        prepared_plan.admitted_artifact_identity(&params.resolved, protocol)?;
    let (realization_contract_ref, realization_contract_digest) = match &admitted_artifact_identity
    {
        ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            runtime_identity,
            ..
        } => (
            runtime_identity.runtime_ref.clone(),
            runtime_identity.runtime_content_hash.clone(),
        ),
        ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime { .. } => {
            anyhow::bail!("direct launch produced a managed runtime artifact identity")
        }
    };
    let direct_executable_identity = match &admitted_artifact_identity {
        ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executable_identity,
            ..
        } => Some(executable_identity),
        ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime { .. } => None,
    };
    ensure_restart_eligible_artifact(
        params.lifecycle_authority,
        &params.resolved.item_ref,
        direct_executable_identity,
    )?;
    let cas = state.state_store.pinned_state_authority()?.cas_store()?;
    let execution_closure = prepared_plan.admit_execution_closure(
        &cas,
        state.isolation.as_ref(),
        protocol,
        &params.provenance.request_engine().node_trust_store,
        admitted_project_root.as_deref(),
    )?;
    let mut metadata = ryeos_app::launch_metadata::RuntimeLaunchMetadata::default()
        .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::DirectItemExecutor)
        .with_admitted_artifact_identity(admitted_artifact_identity)
        .with_admitted_execution_closure(execution_closure)
        .with_resume_context(resume)
        .with_sealed_root_request(sealed);
    let realization_admission = super::execution_realization::admit_or_verify(
        state,
        &metadata,
        finalized_program.resolution(),
        finalized_program.effective_definition_digest().as_str(),
        &realization_contract_ref,
        &realization_contract_digest,
        external_publication.as_mut(),
    )?;
    if external_publication.is_none() {
        external_publication = realization_admission.publication;
    }
    metadata = metadata.with_execution_realization_hash(realization_admission.hash);
    metadata.validate()?;
    super::source_closure::validate_external_mount_separation(
        state,
        finalized_program.resolution(),
        super::source_closure::SourceMountPlacement::Project,
    )?;
    let retained_resolution = finalized_program.resolution().clone();
    Ok((
        metadata,
        DirectExternalRealizations {
            publication: external_publication,
            retained_resolution,
        },
    ))
}

/// Finalize the exact resolution used by non-managed/direct execution before
/// it becomes restart authority. Hook-capable kinds are deliberately excluded:
/// their configured policy must be captured by the managed-launch finalizer,
/// never bypassed by this direct protocol path.
pub(crate) fn finalize_direct_effective_program(
    state: &AppState,
    resolved: &ResolvedExecutionRequest,
    provenance: &ExecutionProvenance,
    parent_thread_id: Option<&str>,
    handler_context: Option<&ryeos_app::handler_context::HandlerContext>,
) -> Result<FinalizedDirectAdmission> {
    let admission = resolved.root_admission.as_ref().ok_or_else(|| {
        anyhow::anyhow!("cannot finalize a direct root execution without root admission")
    })?;
    let engine = admission.request_engine();
    // `ResolvedExecutionRequest.kind` is the lifecycle row kind (`tool_run`,
    // `service_run`, ...). Kind-schema composition and execution contracts are
    // owned by the verified item's canonical kind. Conflating the two makes a
    // direct tool ask the registry for a synthetic thread kind and can move
    // source/external policy outside the authority that declared it.
    let item_kind = resolved.resolved_item.kind.as_str();
    if engine
        .kinds
        .get(item_kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.hooks.as_ref())
        .is_some()
    {
        anyhow::bail!(
            "hook-capable kind `{}` must use managed effective-program finalization",
            item_kind
        );
    }

    let mut resolution = admission.resolution_output().clone();
    let roots = engine.resolution_roots(
        admission
            .resolution_workspace()
            .map(std::path::Path::to_path_buf),
    );
    // A declaring kind captures here exactly as the managed path does. A
    // direct launch dispatched as a child (`parent_thread_id`) may reuse the
    // parent's outer sealed realization set under the existing inheritance
    // rule, resolved from the durable parent capsule and fail-closed on broken
    // lineage. This is dependency-byte inheritance, not permission to treat a
    // managed parent's prepared launch as the child's environment: a child
    // declaration and command remain owned by the child's effective program.
    let inherited_external = parent_thread_id
        .map(|parent_thread_id| {
            state
                .state_store
                .admitted_launch_capsule(parent_thread_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "dispatching parent {parent_thread_id} has no authoritative admitted launch capsule"
                    )
                })?
                .external_realization_set()
        })
        .transpose()?
        .flatten();
    let materialization = admission.resolution_materialization_binding()?;
    let project_content = materialization.authoritative_project_content()?;
    let project = project_content.as_ref().map(|(root, content)| {
        (
            *root,
            *content as &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
        )
    });
    let source_contract = engine
        .kinds
        .get(item_kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.source_closure.as_ref());
    let source_policy = if source_contract.is_some() {
        let executor_id = resolved
            .resolved_item
            .metadata
            .executor_id
            .as_deref()
            .ok_or_else(|| ryeos_engine::error::EngineError::InvalidRuntimeConfig {
                path: resolved.item_ref.clone(),
                reason: "source-owning direct item has no executor chain".to_owned(),
            })?;
        ryeos_engine::launch::plan_builder::resolve_executor_source_policy(
            executor_id,
            &resolution.root.source_path,
            item_kind,
            &engine.kinds,
            &engine.parser_dispatcher,
            &roots,
            &engine.trust_store,
            &engine.node_trust_store,
            project,
        )?
    } else {
        None
    };
    let project_identity = provenance.pinned_snapshot_hash().map(str::to_owned);
    let mut publication = None;
    let captured_source = ryeos_app::source_closure_admission::admit_source_closure_in_publication(
        state,
        engine,
        item_kind,
        &mut resolution,
        &roots,
        project
            .map(|(root, content)| {
                Ok::<_, anyhow::Error>((
                    root,
                    content,
                    project_identity.clone().ok_or_else(|| {
                        anyhow::anyhow!("pinned project content has no snapshot identity")
                    })?,
                ))
            })
            .transpose()?,
        source_policy.as_ref(),
        &mut publication,
        None,
    )?;
    ryeos_app::effective_program_preparation::prepare_hookless_preselection_effective_program(
        engine,
        item_kind,
        &mut resolution,
    )?;
    ryeos_app::operator_external_content::product_composition::admit_root_product_selections(
        state,
        &resolved.current_site_id,
        engine,
        &roots,
        materialization.subject_authority(),
        &mut resolution,
        resolved.requested_by.as_deref(),
        handler_context,
        &resolved.product_selections,
        false,
    )?;
    let captured_external =
        ryeos_app::external_content_admission::admit_external_realizations_in_publication(
            state,
            engine,
            item_kind,
            &mut resolution,
            &roots,
            materialization.subject_authority(),
            inherited_external.as_ref(),
            &mut publication,
        )?;
    let semantic_projection =
        ryeos_engine::effective_program::take_recovered_effective_program_derived(&mut resolution);
    let validation = engine
        .effective_validators
        .validate(item_kind, &resolution)?;
    let candidate = ryeos_engine::effective_program::relock_recovered_effective_program(
        resolution,
        validation,
        semantic_projection,
    )?;
    let proof = ryeos_engine::effective_program::prove_finalization_authority(
        &candidate,
        engine
            .kinds
            .get(item_kind)
            .and_then(|schema| schema.external_content_contract()),
        &[],
        &roots,
        project,
        captured_external
            .as_ref()
            .map(|captured| captured.finalization_evidence()),
        captured_source
            .as_ref()
            .map(|captured| captured.finalization_evidence()),
    )?;
    let finalized = ryeos_engine::effective_program::finalize_effective_program(candidate, proof)?;
    Ok(FinalizedDirectAdmission {
        program: finalized,
        publication,
        source_policy,
    })
}

fn recovered_direct_protocol(
    engine: &ryeos_engine::engine::Engine,
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    item_kind: &str,
) -> Result<ryeos_engine::protocols::VerifiedProtocol> {
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        root_subject_source_identity,
        protocol_ref,
        protocol_content_hash,
        protocol_signer_fingerprint,
        executable_identity,
        runtime_identity,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("direct recovery found a non-direct admitted artifact identity");
    };
    let ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
        protocol_descriptor_document,
        ..
    } = &capsule.execution_closure
    else {
        bail!("direct recovery found a non-direct admitted execution closure");
    };
    if !engine
        .node_trust_store
        .is_trusted(protocol_signer_fingerprint)
    {
        bail!(
            "admitted direct protocol signer is no longer trusted: {protocol_signer_fingerprint}"
        );
    }
    if let Some(bundle_signer) = runtime_identity
        .runtime_bundle_signer_fingerprint
        .as_deref()
        && !engine.node_trust_store.is_trusted(bundle_signer)
    {
        bail!("admitted direct runtime bundle signer is no longer trusted: {bundle_signer}");
    }
    if let ryeos_state::objects::DirectRootSourceIdentity::Bundle {
        manifest_signer_fingerprint,
        ..
    } = root_subject_source_identity
        && !engine
            .node_trust_store
            .is_trusted(manifest_signer_fingerprint)
    {
        bail!(
            "admitted direct root manifest signer is no longer trusted: {manifest_signer_fingerprint}"
        );
    }
    if let ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
        executor_manifest_signer_fingerprint,
        ..
    } = executable_identity
        && !engine
            .node_trust_store
            .is_trusted(executor_manifest_signer_fingerprint)
    {
        bail!(
            "admitted direct executable bundle signer is no longer trusted: {executor_manifest_signer_fingerprint}"
        );
    }
    let header = lillux::signature::parse_signature_line(
        protocol_descriptor_document.lines().next().unwrap_or(""),
        "#",
        None,
    )
    .ok_or_else(|| anyhow::anyhow!("admitted direct protocol has no signature header"))?;
    let protocol_body = lillux::signature::strip_signature_lines(protocol_descriptor_document);
    let observed_hash = lillux::signature::content_hash(&protocol_body);
    if observed_hash != *protocol_content_hash
        || header.content_hash != *protocol_content_hash
        || header.signer_fingerprint != *protocol_signer_fingerprint
    {
        bail!("admitted direct protocol document contradicts its sealed identity");
    }
    let protocol_signer = engine
        .node_trust_store
        .get(protocol_signer_fingerprint)
        .ok_or_else(|| anyhow::anyhow!("admitted direct protocol signer was revoked"))?;
    if !lillux::signature::verify_signature(
        protocol_content_hash,
        &header.signature_b64,
        &protocol_signer.verifying_key,
    ) {
        bail!("admitted direct protocol signature no longer verifies");
    }
    let descriptor: ryeos_engine::protocols::ProtocolDescriptor =
        serde_yaml::from_str(&protocol_body)
            .context("decode admitted direct protocol descriptor")?;
    ryeos_engine::protocols::validate_admitted_protocol_descriptor(protocol_ref, &descriptor)
        .context("validate admitted direct protocol descriptor")?;
    let protocol = ryeos_engine::protocols::VerifiedProtocol {
        canonical_ref: protocol_ref.clone(),
        raw_content_digest: protocol_content_hash.clone(),
        signer_fingerprint: protocol_signer_fingerprint.clone(),
        descriptor,
        trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
        bundle_root: PathBuf::new(),
        descriptor_path: PathBuf::new(),
    };
    crate::dispatch::validate_admitted_direct_protocol(&protocol, item_kind)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(protocol)
}

fn validate_recovered_direct_request_authority(
    state: &AppState,
    thread_id: &str,
    params: &ExecutionParams,
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
) -> Result<()> {
    if capsule.launch_driver != ryeos_state::objects::ExecutionLaunchDriver::DirectItemExecutor
        || params.lifecycle_authority != capsule.lifecycle_authority
        || params.runtime_ref.as_deref() != Some(capsule.runtime_ref.as_str())
    {
        anyhow::bail!(
            "direct recovery operational launch authority contradicts its admitted capsule"
        );
    }
    let mut operational_caps = params.effective_caps.clone();
    operational_caps.sort();
    operational_caps.dedup();
    if operational_caps != params.effective_caps || operational_caps != capsule.effective_caps {
        anyhow::bail!(
            "direct recovery operational capability ceiling contradicts its admitted capsule"
        );
    }
    let sealed =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            capsule,
        )?;
    sealed.validate_current_operator_authority(state)?;
    let authoritative =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::restore_from_admitted_capsule(
            capsule,
            params.provenance.request_engine(),
            &ryeos_app::launch_metadata::daemon_thread_state_dir(&state.config.app_root, thread_id)
                .join("launch-capsule"),
            &params.provenance,
        )
        .context("restore authoritative direct request from CAS capsule")?;
    let actual = &params.resolved;
    if params.parameters != authoritative.parameters
        || params.acting_principal != authoritative.requested_by.as_deref().unwrap_or_default()
        || params.pre_minted_thread_id.is_some()
        || params.parent_thread_id.is_some()
        || actual.kind != authoritative.kind
        || actual.item_ref != authoritative.item_ref
        || actual.executor_ref != authoritative.executor_ref
        || actual.launch_mode != authoritative.launch_mode
        || actual.current_site_id != authoritative.current_site_id
        || actual.origin_site_id != authoritative.origin_site_id
        || actual.target_site_id != authoritative.target_site_id
        || actual.requested_by != authoritative.requested_by
        || actual.usage_subject != authoritative.usage_subject
        || actual.usage_subject_asserted_by != authoritative.usage_subject_asserted_by
        || actual.parameters != authoritative.parameters
        || actual.ref_bindings != authoritative.ref_bindings
        || actual.root_raw_content_digest != authoritative.root_raw_content_digest
        || actual.resolved_item.canonical_ref != authoritative.resolved_item.canonical_ref
        || actual.resolved_item.kind != authoritative.resolved_item.kind
        || actual.resolved_item.source_space != authoritative.resolved_item.source_space
        || actual.resolved_item.raw_content_digest != authoritative.resolved_item.raw_content_digest
        || actual.resolved_item.content_hash != authoritative.resolved_item.content_hash
        || actual.resolved_item.signature_header != authoritative.resolved_item.signature_header
        || serde_json::to_value(&actual.resolved_item.metadata)?
            != serde_json::to_value(&authoritative.resolved_item.metadata)?
    {
        anyhow::bail!(
            "direct recovery request differs from the authoritative CAS capsule invocation"
        );
    }
    let actual_admission = actual
        .root_admission
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("direct recovery request has no exact root admission"))?;
    let authoritative_admission = authoritative
        .root_admission
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("authoritative direct request lost its root admission"))?;
    authoritative_admission.ensure_matches_request(actual)?;
    actual_admission.ensure_matches_request(&authoritative)?;
    if serde_json::to_value(actual_admission.resolution_output())?
        != serde_json::to_value(authoritative_admission.resolution_output())?
    {
        anyhow::bail!(
            "direct recovery resolution differs from the authoritative CAS capsule program"
        );
    }
    Ok(())
}

/// Admit an execution and wait for its current thread to settle.
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
pub async fn run_and_wait(
    state: AppState,
    mut params: ExecutionParams,
    launch_handoff: Option<&super::launch::LaunchHandoff>,
) -> Result<WaitOutcome> {
    let mut guard = ExecutionGuard::new(state.clone());

    // Pre-mint and reserve the launch ID before its row is published. An SSE
    // caller's supplied ID remains exact; ordinary roots receive the same
    // persistence-first ownership boundary.
    let thread_id = params
        .pre_minted_thread_id
        .clone()
        .unwrap_or_else(ryeos_app::thread_lifecycle::new_thread_id);
    let launch_claim = ThreadLaunchClaim::acquire_fresh(&state, &thread_id)?;
    let wait_launch_owner = launch_claim.canonical_owner()?;
    guard.track_launch_owner(wait_launch_owner.clone());

    // Prepare only the project facts selected at admission. Live authority
    // remains direct; pinned authority binds its admitted CAS generation.
    let PreparedCasContext {
        mut effective_path,
        pre_tree_hash,
        pre_policy_hash,
        resume_snapshot_hash,
        mut tree_publication,
    } = prepare_cas_context(&state, &params.provenance, &thread_id, &mut guard)?;
    verify_fresh_root_admission(&params).context("revalidate exact admitted waiting root")?;
    let wait_project_authority = params.provenance.project_authority().clone();
    let admission_isolation_live_access_authority =
        params.provenance.isolation_live_access_authority()?;
    let engine = params.provenance.request_engine().clone();
    let finalized_direct = params.finalized_direct.take().ok_or_else(|| {
        anyhow::anyhow!("fresh direct launch has no pre-authorized effective program")
    })?;
    // Block-scoped: the sealed-bytes source holds non-Send descriptor state
    // and plan build is fully synchronous, so it must be statically dead
    // before this future's next await point.
    let mut prepared_plan = {
        let sealed_dependency_bytes =
            super::external_content::sealed_dependency_bytes_for_child_dispatch(
                &state,
                &params,
                finalized_direct.program.resolution(),
            )
            .context("derive sealed dependency bytes for child dispatch")?;
        thread_lifecycle::prepare_item_plan(
            &engine,
            &params.resolved,
            state.isolation.as_ref(),
            params.lifecycle_authority,
            admission_isolation_live_access_authority.as_ref(),
            sealed_dependency_bytes
                .as_ref()
                .map(|sealed| sealed as &dyn ryeos_engine::project_content::SealedDependencyBytes),
            match params.parent_thread_id.as_deref() {
                Some(parent) => {
                    super::execution_realization::admitted_parent_isolation_ceilings(
                        &state, parent,
                    )?
                    .0
                }
                None => ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
            },
        )?
    };
    prepared_plan.bind_realization_command(
        &state,
        &engine,
        finalized_direct.program.resolution(),
        state.isolation.as_ref(),
    )?;
    if params.provenance.project_source()
        == ryeos_app::execution_provenance::ProjectSourceKind::LiveFs
        && retained_resolution_has_filesystem_bindings(finalized_direct.program.resolution())?
    {
        prepared_plan.ensure_no_project_local_interpreter(params.provenance.effective_path())?;
    }
    let protocol = resolved_terminator_protocol(&engine, &params.resolved)?;
    let stdout_shape = protocol.descriptor.stdout.shape;
    let wait_cas_guard = state
        .state_store
        .pinned_state_authority()?
        .acquire_shared_guard()
        .context("acquire direct admission CAS mutation authority")?;
    let (wait_launch_metadata, mut wait_external) = admitted_root_launch_metadata(
        &state,
        &params,
        wait_project_authority.clone(),
        finalized_direct,
        &mut prepared_plan,
        protocol,
    )?;
    let dispatch_effect_identity = if let Some(prepared) = params.effect_authority.as_ref() {
        if !params
            .resolved
            .resolved_item
            .metadata
            .required_secrets
            .is_empty()
            || !params.vault_bindings.is_empty()
        {
            anyhow::bail!(
                "durable execution refused: the callee uses late-bound secret values with no sealed generation authority"
            );
        }
        if params.provenance.project_source()
            == ryeos_app::execution_provenance::ProjectSourceKind::LiveFs
            && !retained_resolution_has_filesystem_bindings(&wait_external.retained_resolution)?
        {
            anyhow::bail!(
                "recorded execution refused: a live-project subprocess with no admitted filesystem bindings can observe undeclared project bytes"
            );
        }
        prepared.validate()?;
        let capsule = wait_launch_metadata
            .admitted_launch_capsule()?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "durable dispatch subject `{}` produced no admission capsule",
                    params.resolved.item_ref
                )
            })?;
        let subject = admitted_subject_from_capsule(
            &params.resolved,
            &capsule,
            prepared.subject_effect_class_ceiling,
        )?;
        let identity = ryeos_effect_contract::DispatchEffectIdentity {
            authorization: prepared.authorization.clone(),
            action_digest: prepared.action_digest.clone(),
            subject,
        };
        identity.validate()?;
        let cache_key = identity.cache_key()?;
        let authority = super::pinned_state_authority(&state)?;
        let replay_namespace =
            ryeos_state::ReplayIndexNamespace::new(ryeos_effect_contract::EFFECT_REPLAY_NAMESPACE)?;
        let lookup =
            state
                .state_store
                .lookup_replay_record(&replay_namespace, &cache_key, |indexed| {
                    verify_dispatch_effect_record(&state, &authority, indexed)
                })?;
        match lookup {
            ryeos_state::ReplayLookupOutcome::Absent => Some(identity),
            ryeos_state::ReplayLookupOutcome::Present(indexed) => {
                let record = load_verified_dispatch_effect_record(
                    &state,
                    &authority,
                    &indexed,
                    Some(&capsule),
                )?;
                if record.identity != identity {
                    anyhow::bail!(
                        "verified dispatch-effect record {} contradicts the current admitted identity",
                        indexed.record_hash
                    );
                }
                let result = record.answer.replay_leaf_envelope(&indexed.record_hash)?;
                release_tree_publication(
                    tree_publication.take(),
                    "dispatch-effect replay without child launch",
                );
                drop(wait_cas_guard);
                guard.cleanup();
                let effect_identity = identity.cache_key()?;
                return Ok(WaitOutcome::Replayed {
                    result,
                    dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
                        source: ryeos_runtime::callback_contract::RuntimeDispatchSource::EffectRecord,
                        effect_class: runtime_effect_class(identity.authorization.class),
                        action_digest: identity.action_digest.clone(),
                        effect_identity: Some(effect_identity),
                        publication: ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable,
                        record_hash: Some(indexed.record_hash.clone()),
                        replayed_from: Some(indexed.record_hash),
                        result_projection: record.answer.result_projection(),
                    },
                });
            }
            ryeos_state::ReplayLookupOutcome::Unavailable { reason } => {
                anyhow::bail!("dispatch-effect lookup is unavailable: {reason}")
            }
            ryeos_state::ReplayLookupOutcome::IntegrityFailure { reason } => {
                anyhow::bail!("dispatch-effect replay integrity failure: {reason}")
            }
        }
    } else {
        None
    };
    let created = state
        .threads
        .create_root_thread_with_events_and_launch_metadata(
            &thread_id,
            &params.resolved,
            wait_project_authority.clone(),
            Vec::new(),
            Some(&wait_launch_metadata),
        )
        .map_err(|error| {
            anyhow::anyhow!("persist admitted waiting root before runtime preparation: {error:#}")
        })
        .inspect_err(|_e| {
            guard.cleanup();
        })?;
    guard.track_thread(&created.thread_id);
    drop(wait_cas_guard);
    // The row and its capsule are durable, so realization roots are
    // capsule-reachable; retire the staging lease. Failure before this point
    // drops the publication and conservatively abandons the staged roots.
    if let Some(publication) = wait_external.publication.take() {
        publication.publish().map_err(|error| {
            anyhow::anyhow!("publish direct external realization roots: {error:#}")
        })?;
    }
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
                            "record waiting child lineage for {parent_thread_id} failed: {error}; conditional cleanup also failed: {cleanup_error:#}"
                        )
                    })?;
                if !cleanup.is_settled() {
                    tracing::warn!(
                        thread_id = %created.thread_id,
                        parent_thread_id,
                        "waiting child-link cleanup refused because another launch owner advanced the row"
                    );
                }
                guard.cleanup();
                return Err(anyhow::anyhow!(
                    "record waiting child lineage for {parent_thread_id}: {error}"
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
    bind_owned_workspace_after_thread_birth(
        &state,
        &params.provenance,
        &created.thread_id,
        &wait_launch_owner,
    )?;
    let PreparedProcessInputs {
        path: process_path,
        lifeline: process_input_lifeline,
        isolation_project_authority: wait_isolation_project_authority,
        isolation_immutable_project: wait_isolation_immutable_project,
        isolation_live_access_authority: wait_isolation_live_access_authority,
        external: wait_bound_external,
        source: wait_bound_source,
    } = prepare_process_inputs(
        &state,
        &params.provenance,
        &created.thread_id,
        &wait_external.retained_resolution,
        &effective_path,
    )
    .map_err(|error| guard.fail_before_spawn(error))?;
    effective_path = process_path;
    if let Some(lifeline) = process_input_lifeline {
        guard.track_process_input_dir(lifeline);
    }
    tracing::Span::current().record("thread_id", created.thread_id.as_str());

    // The capsule retains a stable logical project root. This in-memory spawn
    // copy executes against the concrete live or pinned workspace selected for
    // this launch. Rebind only typed/validated project paths after birth has
    // rooted the logical closure.
    if matches!(
        &params.resolved.plan_context.project_context,
        ProjectContext::LocalPath { .. }
    ) {
        let admitted_project_root =
            Path::new(ryeos_app::thread_lifecycle::ADMITTED_DIRECT_PROJECT_ROOT);
        prepared_plan
            .relocate_project_for_spawn(Some(admitted_project_root), Some(&effective_path))
            .map_err(|error| guard.fail_before_spawn(error.into()))?;
        params.resolved.plan_context.project_context =
            ryeos_engine::contracts::ProjectContext::LocalPath {
                path: effective_path.clone(),
            };
    }
    super::external_content::bind_prepared_realization_command(
        &mut prepared_plan,
        wait_bound_external.as_ref(),
        state.isolation.as_ref(),
    )
    .map_err(|error| guard.fail_before_spawn(error))?;

    // Spawn — use the per-request engine (pushed_head overlay or
    // daemon startup engine), NOT state.engine directly.
    let launch_timeout_secs = prepared_plan.timeout_secs;
    let tid = created.thread_id.clone();
    let crid = created.chain_root_id.clone();
    let resolved = params.resolved.clone();
    let vault = params.vault_bindings.clone();

    // Resolve the terminator's signed protocol and materialize exactly its env
    // contract. Callback authority exists only when that descriptor asks for
    // it; the guard owns any credentials that were actually minted.
    // This token authenticates callbacks made by the terminal root process
    // itself. Keep its exact root provenance (including an admitted candidate
    // integration authoring root). Callback-dispatched descendants derive a
    // borrowed projection in `runtime_dispatch`, so they cannot inherit root
    // publication authority.
    let callback_provenance = params.provenance.clone();
    // Callback-state project path: the deliberate `state_root` override when
    // requested, otherwise the effective project path. Protocol `project_path`
    // injections remain anchored at `effective_path`; only callback authority
    // moves to the isolated state anchor.
    let runtime_state_root = params
        .provenance
        .state_root_override()
        .unwrap_or(params.provenance.effective_path())
        .to_path_buf();
    tracing::info!(
        source_root = %effective_path.display(),
        state_root = %runtime_state_root.display(),
        "execution roots resolved"
    );
    if super::process_attachment::finalize_requested_stop_if_present(&state, &tid)? {
        return Err(guard.fail_before_spawn(anyhow::anyhow!(
            "terminal subprocess {tid} was stopped before credential mint"
        )));
    }
    let ProtocolLaunchEnv {
        bindings: protocol_env_bindings,
        callback_token,
        thread_auth_token,
        isolation_daemon_socket_path,
    } = build_protocol_launch_env(
        &state,
        protocol,
        &tid,
        &effective_path,
        &runtime_state_root,
        Some(launch_timeout_secs),
        params.effective_caps.clone(),
        &params.acting_principal,
        params.handler_context.as_ref(),
        &params.resolved.current_site_id,
        &params.resolved.origin_site_id,
        callback_provenance,
        &params.resolved.item_ref,
        params.resolved.root_raw_content_digest.clone(),
        effective_bundle_id_for_request(&params.resolved),
        &wait_launch_owner,
    )
    .map_err(|error| guard.fail_before_spawn(error))?;
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
    let wait_snapshot = resume_snapshot_hash.clone();
    let wait_owns_workspace = !params.provenance.is_borrowed_child()
        && wait_project_authority.requires_project_foldback();
    let wait_requires_foldback = wait_owns_workspace;
    let wait_records_terminal_generation =
        wait_project_authority.records_terminal_project_generation();
    let wait_state_root = params
        .provenance
        .state_root_override()
        .map(std::path::Path::to_path_buf);
    let wait_roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
        &engine.resolution_roots(Some(effective_path.clone())),
        &state.config.app_root,
    )
    .map_err(|error| guard.fail_before_spawn(error))?;
    let wait_isolation = state.isolation.clone();
    let wait_isolation_daemon_socket_path = isolation_daemon_socket_path;
    if super::process_attachment::finalize_requested_stop_if_present(&state, &tid)? {
        return Err(guard.fail_before_spawn(anyhow::anyhow!(
            "terminal subprocess {tid} was stopped before isolation and process spawn"
        )));
    }
    // Mounts travel into the spawn; `wait_external.bound` (the generation
    // leases) stays in this scope, which outlives the in-fn wait.
    let wait_external_mounts = wait_bound_external
        .as_ref()
        .map(|bound| bound.mounts().to_vec())
        .unwrap_or_default();
    let wait_external_sealed_env = wait_bound_external
        .as_ref()
        .map(|bound| bound.sealed_set_env().to_string());
    let wait_source_sealed_env = wait_bound_source
        .as_ref()
        .map(|bound| bound.sealed_identity_env().to_string());
    let mut wait_source_mounts = wait_bound_source
        .as_ref()
        .map(|bound| bound.mounts().to_vec())
        .unwrap_or_default();
    let mut wait_external_mounts = wait_external_mounts;
    wait_external_mounts.append(&mut wait_source_mounts);
    let spawn_workspace_lifeline = guard.process_workspace_lifeline();
    let wait_workspace_view = if wait_isolation_project_authority
        == ryeos_engine::isolation::IsolationProjectAuthority::RuntimeWorkspace
    {
        borrow_bound_workspace_view(&state, guard.temp_dir.as_ref(), &created.thread_id)
            .map_err(|error| guard.fail_before_spawn(error))?
    } else {
        None
    };
    let wait_node_trusted_keys_dir = state.config.runtime_root().trusted_keys_dir();
    let wait_isolation_workspace =
        projectless_isolation_workspace(process_project_class(&params.provenance), &effective_path);
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
            roots: wait_roots,
            isolation: wait_isolation,
            isolation_project_authority: wait_isolation_project_authority,
            isolation_immutable_project: wait_isolation_immutable_project,
            isolation_workspace_view: wait_workspace_view,
            isolation_live_access_authority: wait_isolation_live_access_authority,
            isolation_external_read_only_mounts: wait_external_mounts,
            isolation_node_trusted_keys_dir: wait_node_trusted_keys_dir,
            isolation_workspace: wait_isolation_workspace,
            inherited_fds: Vec::new(),
            external_realizations_env: wait_external_sealed_env,
            admitted_source_env: wait_source_sealed_env,
            isolation_daemon_socket_path: wait_isolation_daemon_socket_path.as_deref(),
            thread_state_dir: Some(thread_state_dir.as_path()),
            is_resume: false,
            original_snapshot_hash: wait_snapshot.as_deref(),
            state_root: wait_state_root.as_deref(),
        })
    });

    // The durable row and complete execution request are now owned by the
    // scheduled spawn task. Accepted launch may expose its pre-minted id at
    // this point; spawn/attach/runtime failures remain inspectable on that row.
    if let Some(handoff) = launch_handoff {
        handoff.publish(created.thread_id.clone());
    }

    let mut spawned = match spawn_handle.await {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            tracing::error!(error = %err, "engine error while spawning waited execution");
            if err.contact_is_settled() {
                let outcome = fail_settled_unattached_thread(
                    &state,
                    &created.thread_id,
                    "engine_error",
                    &wait_launch_owner,
                )?;
                if outcome.disarms_guard() {
                    guard.mark_finalized();
                }
            } else {
                guard.fail_thread("engine_error");
            }
            guard.cleanup();
            return Err(err.into_error());
        }
        Err(join_err) => {
            tracing::error!(error = %join_err, "task panic while spawning waited execution");
            guard.fail_thread("task_panic");
            guard.cleanup();
            return Err(anyhow::anyhow!("spawn task panic: {join_err}"));
        }
    };
    spawned.launch_metadata = match wait_launch_metadata
        .merge_for_process_attach(&spawned.launch_metadata)
    {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::error!(%error, "spawn attempt contradicted admitted waiting launch metadata");
            let failure = abort_and_settle_pending_attach_failure(
                &state,
                &created.thread_id,
                &wait_launch_owner,
                spawned,
                PendingAttachFailure {
                    operation: "merge admitted launch metadata",
                    outcome_code: "launch_metadata_conflict".to_string(),
                    process_attached: false,
                    error,
                },
            );
            if failure.cleanup_disarms_guard() {
                guard.mark_finalized();
            }
            guard.cleanup();
            return Err(anyhow::Error::new(failure));
        }
    };

    // Validate restart authority before attach. This boundary never changes a
    // live execution into a pinned execution.
    let snapshot_publication = if !params.provenance.is_borrowed_child() {
        match validate_resume_project_authority(
            &state,
            &mut spawned.launch_metadata,
            &pre_tree_hash,
            &pre_policy_hash,
            &resume_snapshot_hash,
            &mut tree_publication,
        ) {
            Ok(Some(publication)) => Some(publication),
            Ok(None) => None,
            Err(err) => {
                tracing::error!(error = %err, "native-resume project authority validation failed");
                let failure = abort_and_settle_pending_attach_failure(
                    &state,
                    &created.thread_id,
                    &wait_launch_owner,
                    spawned,
                    PendingAttachFailure {
                        operation: "validate native-resume project authority",
                        outcome_code: "resume_project_authority_invalid".to_string(),
                        process_attached: false,
                        error: err,
                    },
                );
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

    // Attach the held process. Failure is settled only after checked abort/reap.
    if let Err(failure) = attach_pending_process(PendingAttachParams {
        state: &state,
        thread_id: &created.thread_id,
        spawned_pid: spawned.pid,
        spawned_pgid: spawned.pgid,
        process_identity: &spawned.process_identity,
        launch_metadata: &spawned.launch_metadata,
        failed_outcome_code: "attach_failed",
        launch_owner: &wait_launch_owner,
        workspace_lifeline: guard.temp_dir.as_ref(),
        owns_workspace: wait_owns_workspace,
    }) {
        let failure = abort_and_settle_pending_attach_failure(
            &state,
            &created.thread_id,
            &wait_launch_owner,
            spawned,
            failure,
        );
        if failure.cleanup_disarms_guard() {
            guard.mark_finalized();
        }
        guard.cleanup();
        return Err(anyhow::Error::new(failure));
    }
    let running = match state.threads.mark_running(&created.thread_id) {
        Ok(running) => running,
        Err(error) => {
            let failure = abort_and_settle_pending_attach_failure(
                &state,
                &created.thread_id,
                &wait_launch_owner,
                spawned,
                PendingAttachFailure {
                    operation: "mark attached execution running",
                    outcome_code: "launch_mark_running_failed".to_string(),
                    process_attached: true,
                    error,
                },
            );
            if failure.cleanup_disarms_guard() {
                guard.mark_finalized();
            }
            guard.cleanup();
            return Err(anyhow::Error::new(failure));
        }
    };
    if let Err(error) = state.threads.authorize_process_release_owned(
        &created.thread_id,
        &spawned.process_identity,
        &wait_launch_owner,
    ) {
        let failure = abort_and_settle_pending_attach_failure(
            &state,
            &created.thread_id,
            &wait_launch_owner,
            spawned,
            PendingAttachFailure {
                operation: "authorize waiting execution release",
                outcome_code: "launch_release_not_authorized".to_string(),
                process_attached: true,
                error,
            },
        );
        if failure.cleanup_disarms_guard() {
            guard.mark_finalized();
        }
        guard.cleanup();
        return Err(anyhow::Error::new(failure));
    }
    let release_identity = spawned.process_identity.clone();
    let spawned = match spawned.release_after_attachment() {
        Ok(running) => running,
        Err(error) => {
            if release_cleanup_is_settled(&error) {
                let _ = clear_finished_process(
                &state,
                &created.thread_id,
                &release_identity,
                &wait_launch_owner,
            ).inspect_err(|error| tracing::error!(%error, "failed launch retains unresolved process authority"));
            }
            let cleanup = fail_thread_static_owned(
                &state,
                &created.thread_id,
                "launch_release_after_attachment_failed",
                &wait_launch_owner,
            );
            if let Err(cleanup_error) = cleanup {
                tracing::error!(
                    thread_id = %created.thread_id,
                    error = %cleanup_error,
                    "waiting execution release failure cleanup did not settle"
                );
            }
            guard.mark_finalized();
            guard.cleanup();
            return Err(error.context("release waiting execution after durable attachment"));
        }
    };
    release_snapshot_publication(snapshot_publication, "waiting launch metadata attachment");
    release_tree_publication(
        tree_publication.take(),
        "waiting authoritative birth and launch metadata attachment",
    );

    // Wait
    let wait_workspace_lifeline = guard.process_workspace_lifeline();
    let waited_identity = spawned.process_identity.clone();
    let mut completion = match super::direct_output::wait(
        spawned,
        stdout_shape,
        state.clone(),
        running.chain_root_id.clone(),
        running.thread_id.clone(),
        wait_launch_owner.clone(),
        wait_workspace_lifeline,
    )
    .await
    {
        Ok(c) => {
            clear_finished_process(
                &state,
                &running.thread_id,
                &waited_identity,
                &wait_launch_owner,
            )?;
            c
        }
        Err(join_err) => {
            let _ = clear_finished_process(
                &state,
                &running.thread_id,
                &waited_identity,
                &wait_launch_owner,
            ).inspect_err(|error| tracing::error!(%error, "failed launch retains unresolved process authority"));
            tracing::error!(error = %join_err, "failed to observe waiting execution");
            guard.fail_thread("process_observation_failed");
            guard.cleanup();
            return Err(join_err);
        }
    };

    // Lift the `--debug-raw` block out of the completion metadata before
    // finalization consumes the completion. `None` on the normal path.
    let debug_block = completion
        .metadata
        .as_ref()
        .and_then(|m| m.get("debug"))
        .cloned();

    let candidate_integration_completion = record_candidate_integration_process_completion(
        &state,
        &running.thread_id,
        params
            .provenance
            .candidate_evaluation_scope()
            .map(|scope| scope.authority()),
        &completion,
    )?;

    if !state.state_store.process_attachment_admission_is_open() {
        let _ = state.state_store.reset_resume_attempts(&running.thread_id);
        release_tree_publication(tree_publication, "waiting shutdown without CAS publication");
        guard.cleanup();
        anyhow::bail!("execution interrupted by daemon shutdown; row preserved for recovery");
    }

    let callback_sealed_result = state
        .state_store
        .authoritative_result_generation(&running.thread_id)?;
    if candidate_integration_completion.is_some() && callback_sealed_result.is_some() {
        guard.fail_thread("candidate_integration_result_authority_conflict");
        guard.cleanup();
        anyhow::bail!(
            "candidate integration cannot bypass its retained workspace freeze with a callback result"
        );
    }
    let mut pending_project_result = None;
    let result_generation = if let Some(snapshot) = callback_sealed_result.as_ref() {
        if !wait_requires_foldback {
            guard.fail_thread("readonly_project_result_rejected");
            guard.cleanup();
            anyhow::bail!("read-only project authority cannot publish a project result generation");
        }
        wait_records_terminal_generation.then(|| snapshot.clone())
    } else if !wait_requires_foldback || !wait_records_terminal_generation {
        None
    } else {
        let contact_fence = ryeos_app::hosted_operation::begin_hosted_root_terminalization_async(
            &state.state_store,
            &running.thread_id,
        )
        .await?;
        transition_owned_workspace(
            &state,
            guard.temp_dir.as_ref(),
            &running.thread_id,
            &[WorkspaceState::Active],
            WorkspaceState::Freezing,
            None,
        )
        .inspect_err(|_| {
            guard.fail_thread("workspace_freeze_failed");
        })?;
        match (
            pre_tree_hash.as_deref(),
            pre_policy_hash.as_deref(),
            resume_snapshot_hash.as_deref(),
            guard.temp_dir.as_ref().and_then(|guard| guard.path()),
        ) {
            (
                Some(pre_tree_hash),
                Some(pre_policy_hash),
                Some(base_snapshot_hash),
                Some(workspace),
            ) => {
                let pending = post_execution_foldback(PostExecutionFoldbackParams {
                    state: &state,
                    contact_fence: &contact_fence,
                    thread_id: &running.thread_id,
                    acting_principal: &params.acting_principal,
                    pre_tree_hash,
                    pre_policy_hash,
                    base_snapshot_hash,
                    terminal_publication: wait_project_authority
                        .terminal_publication()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "fold-back execution is missing sealed terminal publication authority"
                            )
                        })?,
                    project_path: params.provenance.original_project_path(),
                    execution_dir: Some(&workspace),
                    completion: &completion,
                })
                .inspect_err(|_| {
                    guard.fail_thread("foldback_failed");
                })?;
                let generation = pending.generation().clone();
                pending_project_result = Some(pending);
                wait_records_terminal_generation.then_some(generation)
            }
            (None, None, None, _) => None,
            _ => {
                guard.fail_thread("foldback_lineage_missing");
                guard.cleanup();
                anyhow::bail!(
                    "execution {} lost its authoritative workspace generation",
                    running.thread_id
                );
            }
        }
    };
    let result_project_snapshot_hash = result_generation
        .as_ref()
        .map(|generation| generation.snapshot_hash.clone());
    if let Some(fact) = candidate_integration_completion.as_ref() {
        let result_snapshot_hash = result_project_snapshot_hash.as_deref().ok_or_else(|| {
            anyhow::anyhow!("successful candidate integration produced no frozen result generation")
        })?;
        completion = fact.canonical_completion(result_snapshot_hash)?;
    }
    // A non-resumable live-tree execution has no durable snapshot root. Keep
    // its staged project tree protected through execution, then release it here.
    // Native-resume pinning transferred the same permit into
    // `snapshot_publication` and released it after launch-metadata attachment.
    release_tree_publication(tree_publication, "waited execution completion");

    // A session-bound worker finishing its process is not the terminal RyeOS
    // execution boundary. Freeze and durably bind the retained candidate while
    // the ordinary root thread/event chain is still running, then wait for the
    // owner-authorized publish/discard disposition. This keeps validation and
    // publication evidence on the one authoritative root chain and avoids the
    // old terminal -> frozen state reversal.
    let mut dedicated_workspace_closed = false;
    if let (Some(snapshot_hash), Some(session)) = (
        result_project_snapshot_hash.as_deref(),
        state.state_store.dedicated_session(&running.thread_id)?,
    ) && session.state == "freezing"
    {
        if wait_requires_foldback {
            close_owned_workspace(&state, guard.temp_dir.as_ref(), &running.thread_id)
                .context("close session-bound workspace before candidate disposition")?;
            dedicated_workspace_closed = true;
        }
        if !state
            .state_store
            .bind_dedicated_session_candidate(&running.thread_id, snapshot_hash)
            .context("bind retained session-bound candidate")?
        {
            anyhow::bail!("session-bound candidate lost its freezing identity/state CAS");
        }
        // Closing the view releases the workspace journal's GC root. Keep
        // the staged source generation owned until its candidate row has
        // durably taken over; a process crash leaves the stage for the
        // existing closed-workspace candidate-binding reconciliation.
        if let Some(pending) = pending_project_result.take() {
            pending
                .publish()
                .context("publish retained candidate recovery root")?;
        }
        loop {
            let session = state
                .state_store
                .dedicated_session(&running.thread_id)?
                .ok_or_else(|| anyhow::anyhow!("session-bound projection disappeared"))?;
            if session.state == "terminal" {
                break;
            }
            if !matches!(
                session.state.as_str(),
                "frozen"
                    | "verifying"
                    | "qualifying"
                    | "publish_ready"
                    | "publishing"
                    | "discarding"
            ) {
                anyhow::bail!(
                    "session-bound candidate entered invalid disposition state {}",
                    session.state
                );
            }
            tokio::select! {
                result = ryeos_app::dedicated_session_service::wait_for_projection_change(
                    &state,
                    &running.thread_id,
                    session.updated_at_ms,
                    lillux::time::Duration::from_secs(24 * 60 * 60),
                ) => {
                    result?;
                }
                _ = state.state_store.wait_for_process_attachment_admission_close() => {
                    let _ = state.state_store.reset_resume_attempts(&running.thread_id);
                    anyhow::bail!(
                        "session-bound candidate disposition interrupted by daemon shutdown"
                    );
                }
            }
        }
    }

    // Finalize only after any session-bound retained candidate has a terminal
    // owner disposition.
    let finalize_result = if callback_sealed_result.is_some() {
        state
            .threads
            .get_thread(&running.thread_id)?
            .ok_or_else(|| anyhow::anyhow!("callback-sealed thread disappeared"))
            .map_err(|error| ExecutionCleanupFailure {
                operation: "read callback-sealed completion",
                operation_error: error,
                cleanup: Ok(ExecutionCleanupOutcome::AlreadyTerminal),
            })
    } else {
        finalize_completion(
            &state,
            &running.thread_id,
            completion,
            result_generation.as_ref(),
            &wait_launch_owner,
        )
    };
    let finalized = match finalize_result {
        Ok(t) => {
            let publication = if wait_records_terminal_generation {
                pending_project_result
                    .take()
                    .map(crate::execution::PendingProjectResult::publish)
                    .transpose()
                    .map(|_| ())
            } else {
                drop(pending_project_result.take());
                Ok(())
            };
            let close = if wait_requires_foldback && !dedicated_workspace_closed {
                close_terminal_workspace(
                    &state,
                    guard.temp_dir.as_ref(),
                    &running.thread_id,
                    wait_project_authority
                        .terminal_publication()
                        .ok_or_else(|| {
                            anyhow::anyhow!("owned workspace has no terminal publication authority")
                        })?,
                    result_project_snapshot_hash.as_deref(),
                )
            } else {
                Ok(())
            };
            if let Err(error) = close {
                if let Some(workspace) = guard.temp_dir.as_ref() {
                    workspace.disarm();
                }
                guard.mark_finalized();
                // The immutable thread already records the tool outcome.
                // Preserve its exact coordinate and the cleanup cause in the
                // public refusal instead of masking both with an outer label.
                return Err(anyhow::anyhow!(
                    "close execution workspace journal for {}: {error:#}",
                    running.thread_id
                ));
            }
            guard.mark_finalized();
            publication.context("release owner-bound fold-back publication")?;
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
    let result_value = serde_json::to_value(&result).unwrap_or(json!(null));
    let dispatch_effect = if let Some(identity) = dispatch_effect_identity {
        let action_digest = identity.action_digest.clone();
        let effect_class = runtime_effect_class(identity.authorization.class);
        let effect_identity = identity.cache_key()?;
        let response = json!({
            "thread": &finalized,
            "result": &result_value,
        });
        let publication =
            publish_dispatch_effect_record(&state, identity, &response, &finalized.thread_id)?;
        Some(ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
            source: ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed,
            effect_class,
            action_digest,
            effect_identity: Some(effect_identity),
            publication: publication.publication,
            record_hash: Some(publication.record_hash),
            replayed_from: None,
            result_projection: publication.answer.result_projection(),
        })
    } else {
        None
    };
    guard.cleanup();

    Ok(WaitOutcome::Executed(WaitResult {
        finalized_thread: finalized,
        result: result_value,
        result_project_snapshot_hash,
        debug: debug_block,
        dispatch_effect,
    }))
}

/// Launch a detached execution (returns immediately, runs in background).
///
/// Handles the full lifecycle in a background tokio task: CAS
/// context, snapshot, spawn, fold-back, finalize, cleanup. The
/// pre-spawn synchronous setup uses the same cancellation-safe
/// `ExecutionGuard` discipline as `run_and_wait`; once ownership is
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

    // See `run_and_wait` for the pre-publish launch reservation contract.
    let thread_id = params
        .pre_minted_thread_id
        .clone()
        .unwrap_or_else(ryeos_app::thread_lifecycle::new_thread_id);
    let launch_claim = ThreadLaunchClaim::acquire_fresh(&state, &thread_id)?;
    let detached_launch_owner = launch_claim.canonical_owner()?;
    guard.track_launch_owner(detached_launch_owner.clone());

    let PreparedCasContext {
        mut effective_path,
        pre_tree_hash,
        pre_policy_hash,
        resume_snapshot_hash,
        tree_publication,
    } = prepare_cas_context(&state, &params.provenance, &thread_id, &mut guard)?;
    verify_fresh_root_admission(&params).context("revalidate exact admitted detached root")?;
    let bg_project_authority = params.provenance.project_authority().clone();
    let bg_candidate_operation_authority = params
        .provenance
        .candidate_evaluation_scope()
        .map(|scope| scope.authority().clone());
    let admission_isolation_live_access_authority =
        params.provenance.isolation_live_access_authority()?;
    let engine = params.provenance.request_engine().clone();
    let finalized_direct = params.finalized_direct.take().ok_or_else(|| {
        anyhow::anyhow!("fresh direct launch has no pre-authorized effective program")
    })?;
    // Block-scoped: the sealed-bytes source holds non-Send descriptor state
    // and plan build is fully synchronous, so it must be statically dead
    // before this future's next await point.
    let mut prepared_plan = {
        let sealed_dependency_bytes =
            super::external_content::sealed_dependency_bytes_for_child_dispatch(
                &state,
                &params,
                finalized_direct.program.resolution(),
            )
            .context("derive sealed dependency bytes for child dispatch")?;
        thread_lifecycle::prepare_item_plan(
            &engine,
            &params.resolved,
            state.isolation.as_ref(),
            params.lifecycle_authority,
            admission_isolation_live_access_authority.as_ref(),
            sealed_dependency_bytes
                .as_ref()
                .map(|sealed| sealed as &dyn ryeos_engine::project_content::SealedDependencyBytes),
            match params.parent_thread_id.as_deref() {
                Some(parent) => {
                    super::execution_realization::admitted_parent_isolation_ceilings(
                        &state, parent,
                    )?
                    .0
                }
                None => ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
            },
        )?
    };
    prepared_plan.bind_realization_command(
        &state,
        &engine,
        finalized_direct.program.resolution(),
        state.isolation.as_ref(),
    )?;
    if params.provenance.project_source()
        == ryeos_app::execution_provenance::ProjectSourceKind::LiveFs
        && retained_resolution_has_filesystem_bindings(finalized_direct.program.resolution())?
    {
        prepared_plan.ensure_no_project_local_interpreter(params.provenance.effective_path())?;
    }
    let protocol = resolved_terminator_protocol(&engine, &params.resolved)?;
    let bg_cas_guard = state
        .state_store
        .pinned_state_authority()?
        .acquire_shared_guard()
        .context("acquire detached direct admission CAS mutation authority")?;
    let (bg_launch_metadata, mut bg_fresh_external) = admitted_root_launch_metadata(
        &state,
        &params,
        bg_project_authority.clone(),
        finalized_direct,
        &mut prepared_plan,
        protocol,
    )?;
    let created = state
        .threads
        .create_root_thread_with_events_and_launch_metadata(
            &thread_id,
            &params.resolved,
            bg_project_authority.clone(),
            Vec::new(),
            Some(&bg_launch_metadata),
        )
        .map_err(|error| {
            anyhow::anyhow!("persist admitted detached root before runtime preparation: {error:#}")
        })
        .inspect_err(|_e| {
            guard.cleanup();
        })?;
    guard.track_thread(&created.thread_id);
    drop(bg_cas_guard);
    // Row and capsule are durable: realization roots are capsule-reachable,
    // so retire the staging lease before scheduling the detached task.
    if let Some(publication) = bg_fresh_external.publication.take() {
        publication.publish().map_err(|error| {
            anyhow::anyhow!("publish direct external realization roots: {error:#}")
        })?;
    }
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
    bind_owned_workspace_after_thread_birth(
        &state,
        &params.provenance,
        &created.thread_id,
        &detached_launch_owner,
    )?;
    let PreparedProcessInputs {
        path: process_path,
        lifeline: process_input_lifeline,
        isolation_project_authority: bg_isolation_project_authority,
        isolation_immutable_project: bg_isolation_immutable_project,
        isolation_live_access_authority: bg_isolation_live_access_authority,
        external: bg_bound_external,
        source: bg_bound_source,
    } = prepare_process_inputs(
        &state,
        &params.provenance,
        &created.thread_id,
        &bg_fresh_external.retained_resolution,
        &effective_path,
    )
    .map_err(|error| guard.fail_before_spawn(error))?;
    effective_path = process_path;
    if let Some(lifeline) = process_input_lifeline {
        guard.track_process_input_dir(lifeline);
    }
    tracing::Span::current().record("thread_id", created.thread_id.as_str());

    // Keep the serialized logical admission plan sealed while rebinding this
    // spawn copy to the selected live or pinned workspace; see `run_and_wait`.
    if matches!(
        &params.resolved.plan_context.project_context,
        ProjectContext::LocalPath { .. }
    ) {
        let admitted_project_root =
            Path::new(ryeos_app::thread_lifecycle::ADMITTED_DIRECT_PROJECT_ROOT);
        prepared_plan
            .relocate_project_for_spawn(Some(admitted_project_root), Some(&effective_path))
            .map_err(|error| guard.fail_before_spawn(error.into()))?;
        params.resolved.plan_context.project_context =
            ryeos_engine::contracts::ProjectContext::LocalPath {
                path: effective_path.clone(),
            };
    }
    super::external_content::bind_prepared_realization_command(
        &mut prepared_plan,
        bg_bound_external.as_ref(),
        state.isolation.as_ref(),
    )
    .map_err(|error| guard.fail_before_spawn(error))?;

    // Capture thread details before moving guard
    let admitted_thread_id = created.thread_id.clone();

    // Build the exact signed protocol env. Any minted credentials transfer to
    // the background task's revocation guards; callback-free protocols mint
    // none and do not receive isolation access to the daemon socket.
    // The detached terminal process is still this execution root. Descendant
    // callback launches are narrowed at their dispatch boundary.
    let callback_provenance = params.provenance.clone();
    // Same runtime-state root selection as `run_and_wait` (see comment there).
    let runtime_state_root = params
        .provenance
        .state_root_override()
        .unwrap_or(params.provenance.effective_path())
        .to_path_buf();
    tracing::info!(
        source_root = %effective_path.display(),
        state_root = %runtime_state_root.display(),
        "execution roots resolved"
    );
    if super::process_attachment::finalize_requested_stop_if_present(&state, &created.thread_id)? {
        return Err(guard.fail_before_spawn(anyhow::anyhow!(
            "detached terminal subprocess {} was stopped before credential mint",
            created.thread_id
        )));
    }
    let launch_timeout_secs = prepared_plan.timeout_secs;
    let ProtocolLaunchEnv {
        bindings: protocol_env_bindings,
        callback_token,
        thread_auth_token,
        isolation_daemon_socket_path,
    } = build_protocol_launch_env(
        &state,
        protocol,
        &created.thread_id,
        &effective_path,
        &runtime_state_root,
        Some(launch_timeout_secs),
        params.effective_caps.clone(),
        &params.acting_principal,
        params.handler_context.as_ref(),
        &params.resolved.current_site_id,
        &params.resolved.origin_site_id,
        callback_provenance,
        &params.resolved.item_ref,
        params.resolved.root_raw_content_digest.clone(),
        effective_bundle_id_for_request(&params.resolved),
        &detached_launch_owner,
    )
    .map_err(|error| guard.fail_before_spawn(error))?;
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
    let bg_process_input_dir = parts.process_input_dir;
    let bg_cb_token = parts.callback_token;
    let bg_tat_token = parts.thread_auth_token;
    let bg_thread_id = created.thread_id.clone();
    let bg_chain_root_id = created.chain_root_id.clone();
    let bg_resolved = params.resolved.clone();
    let bg_prepared_plan = prepared_plan;
    let bg_stdout_shape = protocol.descriptor.stdout.shape;
    // Per-request engine (pushed_head overlay or daemon startup engine).
    let bg_engine = engine;
    let bg_vault = params.vault_bindings.clone();
    let bg_protocol_env_bindings = protocol_env_bindings;
    let bg_acting_principal = params.acting_principal.clone();
    let bg_pre_tree_hash = pre_tree_hash;
    let bg_pre_policy_hash = pre_policy_hash;
    let bg_resume_snapshot_hash = resume_snapshot_hash;
    let bg_tree_publication = tree_publication;
    let bg_project_path = Some(params.provenance.original_project_path().to_path_buf());
    let bg_skip_resume_snapshot_pin = params.provenance.is_borrowed_child();
    let bg_terminal_publication = params
        .provenance
        .project_authority()
        .terminal_publication()
        .cloned();
    let bg_state_root = params
        .provenance
        .state_root_override()
        .map(std::path::Path::to_path_buf);
    let bg_runtime_state_dir = state.config.app_root.clone();

    let bg_isolation_workspace =
        projectless_isolation_workspace(process_project_class(&params.provenance), &effective_path);

    tokio::spawn(dispatch_detached_bg_task(
        bg_state,
        bg_thread_id,
        bg_chain_root_id,
        bg_resolved,
        bg_prepared_plan,
        bg_engine,
        bg_vault,
        bg_protocol_env_bindings,
        bg_stdout_shape,
        bg_acting_principal,
        bg_pre_tree_hash,
        bg_pre_policy_hash,
        bg_resume_snapshot_hash,
        bg_tree_publication,
        bg_project_path,
        bg_project_authority,
        bg_candidate_operation_authority,
        bg_state_root,
        bg_isolation_workspace,
        bg_isolation_project_authority,
        bg_isolation_immutable_project,
        bg_isolation_live_access_authority,
        isolation_daemon_socket_path,
        bg_temp_dir,
        bg_process_input_dir,
        bg_skip_resume_snapshot_pin,
        bg_terminal_publication,
        bg_bound_external,
        bg_bound_source,
        bg_runtime_state_dir,
        DetachedDispatchKind::Detached,
        Some(
            ryeos_state::objects::ThreadStatus::Created
                .as_str()
                .to_string(),
        ),
        bg_cb_token,
        bg_tat_token,
        Some(launch_claim),
    ));

    // Every execution input and cleanup guard is now owned by the scheduled
    // detached task. This is the terminal-subprocess acknowledgement boundary.
    if let Some(handoff) = launch_handoff {
        handoff.publish(admitted_thread_id.clone());
    }

    // Re-fetch the thread detail (the original was consumed by the background task setup)
    let admitted_detail = state
        .threads
        .get_thread(&admitted_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("thread {admitted_thread_id} not found after spawn"))?;

    Ok(DetachedResult {
        running_thread: admitted_detail,
    })
}

/// Shared background-task body for detached spawns.
///
/// Used by fresh detached execution, never-started admitted-root recovery, and
/// checkpoint resume. Centralizes the
/// spawn → pin → attach → wait → fold-back → finalize → cleanup flow
/// so both paths stay in lock-step on the fail-closed contract:
/// any failure between spawn and attach kills the live PG and
/// finalizes the thread; finalize errors are logged loudly.
///
/// `prior_status_for_mark_running` is `Some("created")` only on a recovery
/// path when the persisted thread row has not yet been
/// transitioned out of `created`. The dispatcher calls
/// `mark_running` after attach so `drain_running_threads` (which
/// only queries `["running"]`) can reach the live child on shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetachedDispatchKind {
    /// A newly admitted ordinary detached execution.
    Detached,
    /// Recovery of an admitted root that never reached its first process attach.
    RecoveredAdmittedRoot,
    /// Recovery of a process that previously ran and emitted a checkpoint.
    NativeResume,
}

impl DetachedDispatchKind {
    fn is_checkpoint_resume(self) -> bool {
        matches!(self, Self::NativeResume)
    }

    fn waits_for_recovery_gate(self) -> bool {
        !matches!(self, Self::Detached)
    }

    fn log_phase(self) -> &'static str {
        match self {
            Self::Detached => "detached",
            Self::RecoveredAdmittedRoot => "admitted_root_recovery",
            Self::NativeResume => "resume",
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "thread:dispatch",
    skip(
        bg_state, bg_chain_root_id, bg_resolved, bg_prepared_plan, bg_engine, bg_vault,
        bg_protocol_env_bindings, bg_acting_principal, bg_pre_tree_hash,
        bg_pre_policy_hash,
        bg_resume_snapshot_hash, bg_tree_publication,
        bg_project_path, bg_state_root, bg_isolation_workspace,
        bg_project_authority,
        bg_candidate_operation_authority,
        bg_isolation_project_authority, bg_isolation_immutable_project, bg_isolation_daemon_socket_path, bg_temp_dir,
        bg_process_input_dir,
        bg_skip_resume_snapshot_pin, bg_terminal_publication, bg_external_realizations,
        bg_source_closure,
        bg_runtime_state_dir,
        prior_status_for_mark_running,
        bg_cb_token, bg_tat_token, launch_claim
    ),
    fields(
        thread_id = %bg_thread_id,
        item_ref = %bg_resolved.item_ref,
        dispatch_kind = ?dispatch_kind,
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
    bg_stdout_shape: ryeos_engine::protocol_vocabulary::StdoutShape,
    bg_acting_principal: String,
    bg_pre_tree_hash: Option<String>,
    bg_pre_policy_hash: Option<String>,
    bg_resume_snapshot_hash: Option<String>,
    mut bg_tree_publication: Option<super::PendingCasPublication>,
    bg_project_path: Option<PathBuf>,
    bg_project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    bg_candidate_operation_authority: Option<
        ryeos_app::thread_lifecycle::CandidateEvaluationAuthority,
    >,
    bg_state_root: Option<PathBuf>,
    bg_isolation_workspace: Option<PathBuf>,
    bg_isolation_project_authority: ryeos_engine::isolation::IsolationProjectAuthority,
    bg_isolation_immutable_project: Option<ryeos_state::PinnedProjectMaterialization>,
    bg_isolation_live_access_authority: Option<
        ryeos_engine::isolation::IsolationLiveAccessAuthority,
    >,
    bg_isolation_daemon_socket_path: Option<PathBuf>,
    mut bg_temp_dir: Option<Arc<TempDirGuard>>,
    bg_process_input_dir: Option<Arc<TempDirGuard>>,
    bg_skip_resume_snapshot_pin: bool,
    bg_terminal_publication: Option<ryeos_state::objects::PinnedTerminalPublication>,
    bg_external_realizations: Option<super::external_content::BoundExternalRealizations>,
    bg_source_closure: Option<super::source_closure::BoundSourceClosure>,
    bg_runtime_state_dir: PathBuf,
    dispatch_kind: DetachedDispatchKind,
    prior_status_for_mark_running: Option<String>,
    bg_cb_token: Option<String>,
    bg_tat_token: Option<String>,
    launch_claim: Option<ThreadLaunchClaim>,
) {
    // Keep recovery's durable spawn authorization alive through spawn, attach,
    // running, wait, and failure/finalization. Every early return drops it; a
    // completed task releases it at the function boundary.
    let launch_claim_guard = launch_claim;
    // Mount authorities travel into the spawn; the bound value itself — the
    // materialization generation leases — lives to the end of this task,
    // which spans spawn, attach, running, and wait.
    let bg_external_mounts = bg_external_realizations
        .as_ref()
        .map(|bound| bound.mounts().to_vec())
        .unwrap_or_default();
    let bg_external_sealed_env = bg_external_realizations
        .as_ref()
        .map(|bound| bound.sealed_set_env().to_string());
    let bg_source_sealed_env = bg_source_closure
        .as_ref()
        .map(|bound| bound.sealed_identity_env().to_string());
    let mut bg_source_mounts = bg_source_closure
        .as_ref()
        .map(|bound| bound.mounts().to_vec())
        .unwrap_or_default();
    let mut bg_external_mounts = bg_external_mounts;
    bg_external_mounts.append(&mut bg_source_mounts);
    let launch_owner = match launch_claim_guard
        .as_ref()
        .map(ThreadLaunchClaim::canonical_owner)
        .transpose()
    {
        Ok(Some(owner)) => owner,
        Ok(None) => {
            tracing::error!(thread_id = %bg_thread_id, "detached launch lost its durable owner");
            return;
        }
        Err(error) => {
            tracing::error!(thread_id = %bg_thread_id, %error, "serialize detached launch owner");
            return;
        }
    };
    // Revoke every protocol-requested credential on every exit path. A
    // callback-free protocol passes `None` and installs inert guards.
    let _cb_guard = defer_cb_token_revocation(&bg_state, &bg_thread_id, &bg_cb_token);
    let _tat_guard = defer_tat_token_revocation(&bg_state, &bg_thread_id, &bg_tat_token);
    if dispatch_kind.waits_for_recovery_gate()
        && !ryeos_app::recovery_execution_gate::wait_if_armed().await
    {
        // Reconcile reserved this native-resume attempt before enqueueing the
        // launch. If startup fails before the execution gate opens, no worker
        // was contacted and the reservation must not consume the bounded
        // retry budget. The owned launch claim drops immediately after this
        // reset, preserving the same rearm-before-release ordering as stale
        // dead-generation claim recovery.
        if dispatch_kind.is_checkpoint_resume()
            && let Err(error) = bg_state.state_store.reset_resume_attempts(&bg_thread_id)
        {
            tracing::error!(
                thread_id = %bg_thread_id,
                error = %error,
                "failed to rearm native-resume attempt interrupted before recovery-gate release"
            );
        }
        return;
    }

    if let Some(ref s) = prior_status_for_mark_running {
        tracing::Span::current().record("prior_status", s.as_str());
    }
    let is_resume = dispatch_kind.is_checkpoint_resume();
    let log_phase = dispatch_kind.log_phase();
    let attach_outcome_code = if is_resume {
        "resume_attach_failed"
    } else if matches!(dispatch_kind, DetachedDispatchKind::RecoveredAdmittedRoot) {
        "admitted_root_attach_failed"
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
    let bg_requires_foldback =
        !bg_skip_resume_snapshot_pin && bg_project_authority.requires_project_foldback();
    let bg_records_terminal_generation = bg_project_authority.records_terminal_project_generation();
    let state_root_for_spawn = bg_state_root;
    let isolation_for_spawn = bg_state.isolation.clone();
    let isolation_daemon_socket_path_for_spawn = bg_isolation_daemon_socket_path;
    let spawn_workspace_lifeline = bg_process_input_dir.clone().or_else(|| bg_temp_dir.clone());

    match super::process_attachment::finalize_requested_stop_if_present(&bg_state, &bg_thread_id) {
        Ok(true) => {
            if let Err(cleanup) = fail_settled_unattached_thread(
                &bg_state,
                &bg_thread_id,
                "stopped_before_spawn",
                &launch_owner,
            ) {
                tracing::error!(thread_id = %bg_thread_id, %cleanup, "settle stopped uncontacted launch");
            }
            drop(bg_temp_dir.take());
            return;
        }
        Ok(false) => {}
        Err(error) => {
            tracing::error!(
                phase = log_phase,
                thread_id = %bg_thread_id,
                %error,
                "durable stop fence failed before detached isolation and process spawn"
            );
            if let Err(cleanup_error) = fail_settled_unattached_thread(
                &bg_state,
                &bg_thread_id,
                "stop_fence_failed",
                &launch_owner,
            ) {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %cleanup_error,
                    "detached stop-fence failure and terminal cleanup both failed"
                );
            }
            drop(bg_temp_dir.take());
            return;
        }
    }

    let bg_node_trusted_keys_dir = bg_state.config.runtime_root().trusted_keys_dir();
    let bg_workspace_view = if bg_isolation_project_authority
        == ryeos_engine::isolation::IsolationProjectAuthority::RuntimeWorkspace
    {
        match borrow_bound_workspace_view(&bg_state, bg_temp_dir.as_ref(), &bg_thread_id) {
            Ok(view) => view,
            Err(error) => {
                tracing::error!(thread_id = %bg_thread_id, %error, "workspace borrow refused before detached spawn");
                let _ = fail_settled_unattached_thread(
                    &bg_state,
                    &bg_thread_id,
                    "workspace_borrow_refused",
                    &launch_owner,
                );
                return;
            }
        }
    } else {
        None
    };
    // Resolve before scheduling: this error is demonstrably before process
    // contact, unlike an arbitrary error returned by the engine spawn owner.
    let project_root = match &res_for_spawn.plan_context.project_context {
        ryeos_engine::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
        _ => None,
    };
    let roots = match ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
        &eng_for_spawn.resolution_roots(project_root),
        &bg_runtime_state_dir,
    ) {
        Ok(roots) => roots,
        Err(error) => {
            tracing::error!(thread_id = %bg_thread_id, %error, "resolve detached launch roots");
            if let Err(cleanup) = fail_settled_unattached_thread(
                &bg_state,
                &bg_thread_id,
                "launch_roots_invalid",
                &launch_owner,
            ) {
                tracing::error!(thread_id = %bg_thread_id, %cleanup, "settle uncontacted detached launch");
            }
            return;
        }
    };
    let spawn_result = task::spawn_blocking(move || {
        let _spawn_workspace_lifeline = spawn_workspace_lifeline;
        thread_lifecycle::spawn_item(thread_lifecycle::SpawnItemParams {
            engine: &eng_for_spawn,
            resolved: &res_for_spawn,
            prepared_plan: bg_prepared_plan,
            thread_id: &tid_for_spawn,
            chain_root_id: &crid_for_spawn,
            vault_bindings: vault_for_spawn,
            protocol_env_bindings: protocol_env_for_spawn,
            roots,
            isolation: isolation_for_spawn,
            isolation_project_authority: bg_isolation_project_authority,
            isolation_immutable_project: bg_isolation_immutable_project,
            isolation_workspace_view: bg_workspace_view,
            isolation_live_access_authority: bg_isolation_live_access_authority,
            isolation_external_read_only_mounts: bg_external_mounts,
            isolation_node_trusted_keys_dir: bg_node_trusted_keys_dir,
            isolation_workspace: bg_isolation_workspace,
            inherited_fds: Vec::new(),
            external_realizations_env: bg_external_sealed_env,
            admitted_source_env: bg_source_sealed_env,
            isolation_daemon_socket_path: isolation_daemon_socket_path_for_spawn.as_deref(),
            thread_state_dir: Some(thread_state_dir.as_path()),
            is_resume,
            original_snapshot_hash: snap_for_spawn.as_deref(),
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
            let cleanup = if err.contact_is_settled() {
                fail_settled_unattached_thread(
                    &bg_state,
                    &bg_thread_id,
                    "engine_error",
                    &launch_owner,
                )
            } else {
                fail_thread_static_owned(&bg_state, &bg_thread_id, "engine_error", &launch_owner)
            };
            if let Err(cleanup_error) = cleanup {
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
            if let Err(cleanup_error) =
                fail_thread_static_owned(&bg_state, &bg_thread_id, "task_panic", &launch_owner)
            {
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
    let admitted_launch_metadata = match bg_state
        .state_store
        .get_launch_metadata(&bg_thread_id)
        .and_then(|metadata| {
            metadata.ok_or_else(|| {
                anyhow::anyhow!(
                    "thread {bg_thread_id} has no admitted launch metadata before process attach"
                )
            })
        }) {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::error!(
                phase = log_phase,
                thread_id = %bg_thread_id,
                %error,
                "load admitted launch metadata before process attach"
            );
            let failure = abort_and_settle_pending_attach_failure(
                &bg_state,
                &bg_thread_id,
                &launch_owner,
                spawned,
                PendingAttachFailure {
                    operation: "load admitted launch metadata",
                    outcome_code: "launch_metadata_missing".to_string(),
                    process_attached: false,
                    error,
                },
            );
            tracing::error!(thread_id = %bg_thread_id, %failure, "pending launch refused");
            drop(bg_temp_dir.take());
            return;
        }
    };
    spawned.launch_metadata =
        match admitted_launch_metadata.merge_for_process_attach(&spawned.launch_metadata) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    %error,
                    "spawn attempt contradicted admitted launch metadata"
                );
                let failure = abort_and_settle_pending_attach_failure(
                    &bg_state,
                    &bg_thread_id,
                    &launch_owner,
                    spawned,
                    PendingAttachFailure {
                        operation: "merge admitted launch metadata",
                        outcome_code: "launch_metadata_conflict".to_string(),
                        process_attached: false,
                        error,
                    },
                );
                tracing::error!(thread_id = %bg_thread_id, %failure, "pending launch refused");
                drop(bg_temp_dir.take());
                return;
            }
        };

    // Validate restart authority before attach without changing its mode.
    let snapshot_publication = if !bg_skip_resume_snapshot_pin {
        match validate_resume_project_authority(
            &bg_state,
            &mut spawned.launch_metadata,
            &bg_pre_tree_hash,
            &bg_pre_policy_hash,
            &bg_resume_snapshot_hash,
            &mut bg_tree_publication,
        ) {
            Ok(Some(publication)) => Some(publication),
            Ok(None) => None,
            Err(err) => {
                tracing::error!(
                    phase = log_phase,
                    error = %err,
                    "native-resume project authority validation failed"
                );
                let failure = abort_and_settle_pending_attach_failure(
                    &bg_state,
                    &bg_thread_id,
                    &launch_owner,
                    spawned,
                    PendingAttachFailure {
                        operation: "validate native-resume project authority",
                        outcome_code: "resume_project_authority_invalid".to_string(),
                        process_attached: false,
                        error: err,
                    },
                );
                tracing::error!(thread_id = %bg_thread_id, %failure, "pending launch refused");
                drop(bg_temp_dir.take());
                return;
            }
        }
    } else {
        None
    };

    // Attach the held process. Failure is settled only after checked abort/reap.
    if let Err(failure) = attach_pending_process(PendingAttachParams {
        state: &bg_state,
        thread_id: &bg_thread_id,
        spawned_pid: spawned.pid,
        spawned_pgid: spawned.pgid,
        process_identity: &spawned.process_identity,
        launch_metadata: &spawned.launch_metadata,
        failed_outcome_code: attach_outcome_code,
        launch_owner: &launch_owner,
        workspace_lifeline: bg_temp_dir.as_ref(),
        owns_workspace: bg_requires_foldback,
    }) {
        let failure = abort_and_settle_pending_attach_failure(
            &bg_state,
            &bg_thread_id,
            &launch_owner,
            spawned,
            failure,
        );
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
    release_tree_publication(
        bg_tree_publication.take(),
        "detached authoritative birth and launch metadata attachment",
    );

    // Recovery of a `created` row: transition to `running` so
    // `drain_running_threads` sees it on shutdown.
    if matches!(prior_status_for_mark_running.as_deref(), Some("created"))
        && let Err(err) = bg_state.threads.mark_running(&bg_thread_id)
    {
        tracing::error!(
            phase = log_phase,
            thread_id = %bg_thread_id,
            error = %err,
            "failed to transition recovered created thread to running; terminating its process"
        );
        let failure = abort_and_settle_pending_attach_failure(
            &bg_state,
            &bg_thread_id,
            &launch_owner,
            spawned,
            PendingAttachFailure {
                operation: "mark recovered execution running",
                outcome_code: "recovery_mark_running_failed".to_string(),
                process_attached: true,
                error: err,
            },
        );
        tracing::error!(thread_id = %bg_thread_id, %failure, "pending launch refused");
        drop(bg_temp_dir.take());
        return;
    }

    if let Err(error) = bg_state.threads.authorize_process_release_owned(
        &bg_thread_id,
        &spawned.process_identity,
        &launch_owner,
    ) {
        tracing::error!(
            phase = log_phase,
            thread_id = %bg_thread_id,
            %error,
            "detached execution release was not authorized after attachment"
        );
        let failure = abort_and_settle_pending_attach_failure(
            &bg_state,
            &bg_thread_id,
            &launch_owner,
            spawned,
            PendingAttachFailure {
                operation: "authorize detached execution release",
                outcome_code: "launch_release_not_authorized".to_string(),
                process_attached: true,
                error,
            },
        );
        tracing::error!(thread_id = %bg_thread_id, %failure, "pending launch refused");
        drop(bg_temp_dir.take());
        return;
    }

    let release_identity = spawned.process_identity.clone();
    let spawned = match spawned.release_after_attachment() {
        Ok(running) => running,
        Err(error) => {
            tracing::error!(
                phase = log_phase,
                thread_id = %bg_thread_id,
                %error,
                "failed to release detached execution after durable attachment"
            );
            if release_cleanup_is_settled(&error) {
                let _ = clear_finished_process(&bg_state, &bg_thread_id, &release_identity, &launch_owner).inspect_err(|error| tracing::error!(%error, "failed launch retains unresolved process authority"));
            }
            if let Err(cleanup_error) = fail_thread_static_owned(
                &bg_state,
                &bg_thread_id,
                "launch_release_after_attachment_failed",
                &launch_owner,
            ) {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %cleanup_error,
                    "detached release failure cleanup did not settle"
                );
            }
            drop(bg_temp_dir.take());
            return;
        }
    };

    let wait_workspace_lifeline = bg_process_input_dir.clone().or_else(|| bg_temp_dir.clone());
    let waited_identity = spawned.process_identity.clone();
    let wait_result = super::direct_output::wait(
        spawned,
        bg_stdout_shape,
        bg_state.clone(),
        bg_chain_root_id.clone(),
        bg_thread_id.clone(),
        launch_owner.clone(),
        wait_workspace_lifeline,
    )
    .await;
    if let Err(error) =
        clear_finished_process(&bg_state, &bg_thread_id, &waited_identity, &launch_owner)
    {
        tracing::error!(thread_id = %bg_thread_id, %error, "detached process settlement refused; no capture or publication");
        let _ = fail_thread_static_owned(
            &bg_state,
            &bg_thread_id,
            "process_settlement_unproved",
            &launch_owner,
        );
        return;
    }
    // Extract the execution dir path while the Arc is still alive.
    let bg_exec_dir_path = bg_temp_dir.as_ref().and_then(|g| g.path());
    match wait_result {
        Ok(mut completion) => {
            let candidate_integration_completion =
                match record_candidate_integration_process_completion(
                    &bg_state,
                    &bg_thread_id,
                    bg_candidate_operation_authority.as_ref(),
                    &completion,
                ) {
                    Ok(fact) => fact,
                    Err(error) => {
                        tracing::error!(
                            phase = log_phase,
                            thread_id = %bg_thread_id,
                            %error,
                            "candidate integration completion could not be journaled"
                        );
                        let _ = fail_thread_static_owned(
                            &bg_state,
                            &bg_thread_id,
                            "candidate_integration_completion_invalid",
                            &launch_owner,
                        );
                        drop(bg_temp_dir.take());
                        return;
                    }
                };
            if !bg_state.state_store.process_attachment_admission_is_open() {
                let _ = bg_state.state_store.reset_resume_attempts(&bg_thread_id);
                release_tree_publication(
                    bg_tree_publication,
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
            let callback_sealed_result = match bg_state
                .state_store
                .authoritative_result_generation(&bg_thread_id)
            {
                Ok(value) => value,
                Err(error) => {
                    tracing::error!(thread_id = %bg_thread_id, %error, "read callback-sealed generation failed");
                    return;
                }
            };
            if candidate_integration_completion.is_some() && callback_sealed_result.is_some() {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    "candidate integration attempted to bypass retained-workspace freeze"
                );
                let _ = fail_thread_static_owned(
                    &bg_state,
                    &bg_thread_id,
                    "candidate_integration_result_authority_conflict",
                    &launch_owner,
                );
                drop(bg_temp_dir.take());
                return;
            }
            // Acquire before any synchronous capture/CAS permit, so draining
            // operations cannot be starved of the resources they need to exit.
            let contact_fence = if callback_sealed_result.is_none()
                && bg_requires_foldback
                && bg_records_terminal_generation
            {
                match ryeos_app::hosted_operation::begin_hosted_root_terminalization_async(
                    &bg_state.state_store,
                    &bg_thread_id,
                )
                .await
                {
                    Ok(fence) => Some(fence),
                    Err(error) => {
                        tracing::error!(thread_id = %bg_thread_id, %error, "terminal capture contact fence failed");
                        let _ = fail_thread_static_owned(
                            &bg_state,
                            &bg_thread_id,
                            "workspace_capture_fence_failed",
                            &launch_owner,
                        );
                        return;
                    }
                }
            } else {
                None
            };
            if callback_sealed_result.is_none()
                && bg_requires_foldback
                && let Err(error) = transition_owned_workspace(
                    &bg_state,
                    bg_temp_dir.as_ref(),
                    &bg_thread_id,
                    &[WorkspaceState::Active],
                    WorkspaceState::Freezing,
                    None,
                )
            {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %error,
                    "workspace freeze transition failed"
                );
                let _ = fail_thread_static_owned(
                    &bg_state,
                    &bg_thread_id,
                    "workspace_freeze_failed",
                    &launch_owner,
                );
                return;
            }
            let mut pending_project_result = None;
            let result_generation = if let Some(snapshot) = callback_sealed_result.as_ref() {
                if !bg_requires_foldback {
                    tracing::error!(
                        phase = log_phase,
                        thread_id = %bg_thread_id,
                        "read-only project authority attempted to publish a result generation"
                    );
                    let _ = fail_thread_static_owned(
                        &bg_state,
                        &bg_thread_id,
                        "readonly_project_result_rejected",
                        &launch_owner,
                    );
                    drop(bg_temp_dir.take());
                    return;
                }
                bg_records_terminal_generation.then(|| snapshot.clone())
            } else if !bg_requires_foldback || !bg_records_terminal_generation {
                None
            } else {
                match (
                    bg_pre_tree_hash.as_deref(),
                    bg_pre_policy_hash.as_deref(),
                    bg_resume_snapshot_hash.as_deref(),
                    bg_project_path.as_deref(),
                    bg_exec_dir_path.as_deref(),
                ) {
                    (
                        Some(pre_tree_hash),
                        Some(pre_policy_hash),
                        Some(base_snapshot_hash),
                        Some(project_path),
                        Some(workspace),
                    ) => match post_execution_foldback(PostExecutionFoldbackParams {
                        state: &bg_state,
                        contact_fence: contact_fence
                            .as_ref()
                            .expect("terminal foldback acquired its contact fence"),
                        thread_id: &bg_thread_id,
                        acting_principal: &bg_acting_principal,
                        pre_tree_hash,
                        pre_policy_hash,
                        base_snapshot_hash,
                        terminal_publication: match bg_terminal_publication.as_ref() {
                            Some(publication) => publication,
                            None => {
                                tracing::error!(
                                    phase = log_phase,
                                    thread_id = %bg_thread_id,
                                    "fold-back execution is missing sealed terminal publication authority"
                                );
                                let _ = fail_thread_static_owned(
                                    &bg_state,
                                    &bg_thread_id,
                                    "foldback_authority_missing",
                                    &launch_owner,
                                );
                                drop(bg_temp_dir.take());
                                return;
                            }
                        },
                        project_path,
                        execution_dir: Some(workspace),
                        completion: &completion,
                    }) {
                        Ok(pending) => {
                            let generation = pending.generation().clone();
                            pending_project_result = Some(pending);
                            bg_records_terminal_generation.then_some(generation)
                        }
                        Err(error) => {
                            tracing::error!(
                                phase = log_phase,
                                thread_id = %bg_thread_id,
                                error = %error,
                                "authoritative fold-back failed; refusing successful settlement"
                            );
                            if let Err(cleanup_error) = fail_thread_static_owned(
                                &bg_state,
                                &bg_thread_id,
                                "foldback_failed",
                                &launch_owner,
                            ) {
                                tracing::error!(
                                    phase = log_phase,
                                    thread_id = %bg_thread_id,
                                    error = %cleanup_error,
                                    "fold-back failure cleanup did not settle"
                                );
                            }
                            drop(bg_temp_dir.take());
                            return;
                        }
                    },
                    (None, None, None, _, _) => None,
                    _ => {
                        tracing::error!(
                            thread_id = %bg_thread_id,
                            "execution lost its authoritative workspace generation"
                        );
                        let _ = fail_thread_static_owned(
                            &bg_state,
                            &bg_thread_id,
                            "foldback_lineage_missing",
                            &launch_owner,
                        );
                        drop(bg_temp_dir.take());
                        return;
                    }
                }
            };
            // Freezing now durably rejects new workspace contact. Candidate
            // disposition has its own operations; do not retain this temporary
            // drain gate through review/publication or reacquire it on failure.
            drop(contact_fence);
            let result_project_snapshot_hash = result_generation
                .as_ref()
                .map(|generation| generation.snapshot_hash.clone());
            if let Some(fact) = candidate_integration_completion.as_ref() {
                let Some(result_snapshot_hash) = result_project_snapshot_hash.as_deref() else {
                    tracing::error!(
                        phase = log_phase,
                        thread_id = %bg_thread_id,
                        "successful candidate integration produced no frozen result generation"
                    );
                    let _ = fail_thread_static_owned(
                        &bg_state,
                        &bg_thread_id,
                        "candidate_integration_result_missing",
                        &launch_owner,
                    );
                    drop(bg_temp_dir.take());
                    return;
                };
                completion = match fact.canonical_completion(result_snapshot_hash) {
                    Ok(completion) => completion,
                    Err(error) => {
                        tracing::error!(
                            phase = log_phase,
                            thread_id = %bg_thread_id,
                            %error,
                            "candidate integration terminal result canonicalization failed"
                        );
                        let _ = fail_thread_static_owned(
                            &bg_state,
                            &bg_thread_id,
                            "candidate_integration_result_invalid",
                            &launch_owner,
                        );
                        drop(bg_temp_dir.take());
                        return;
                    }
                };
            }
            let mut dedicated_disposition = false;
            if let (Some(snapshot_hash), Ok(Some(session))) = (
                result_project_snapshot_hash.as_deref(),
                bg_state.state_store.dedicated_session(&bg_thread_id),
            ) && session.state == "freezing"
            {
                let disposition = async {
                    if bg_requires_foldback {
                        close_owned_workspace(&bg_state, bg_temp_dir.as_ref(), &bg_thread_id)
                            .context("close detached session-bound workspace")?;
                    }
                    if !bg_state
                        .state_store
                        .bind_dedicated_session_candidate(&bg_thread_id, snapshot_hash)?
                    {
                        anyhow::bail!(
                            "detached session-bound candidate lost its freezing identity/state CAS"
                        );
                    }
                    if let Some(pending) = pending_project_result.take() {
                        pending
                            .publish()
                            .context("publish detached retained-candidate recovery root")?;
                    }
                    loop {
                        let session = bg_state
                            .state_store
                            .dedicated_session(&bg_thread_id)?
                            .ok_or_else(|| anyhow::anyhow!("session-bound projection disappeared"))?;
                        if session.state == "terminal" {
                            break;
                        }
                        if !matches!(
                            session.state.as_str(),
                            "frozen"
                                | "verifying"
                                | "qualifying"
                                | "publish_ready"
                                | "publishing"
                                | "discarding"
                        ) {
                            anyhow::bail!(
                                "detached session-bound candidate entered invalid disposition state {}",
                                session.state
                            );
                        }
                        tokio::select! {
                            result = ryeos_app::dedicated_session_service::wait_for_projection_change(
                                &bg_state,
                                &bg_thread_id,
                                session.updated_at_ms,
                                lillux::time::Duration::from_secs(24 * 60 * 60),
                            ) => {
                                result?;
                            }
                            _ = bg_state.state_store.wait_for_process_attachment_admission_close() => {
                                let _ = bg_state.state_store.reset_resume_attempts(&bg_thread_id);
                                anyhow::bail!(
                                    "detached session-bound candidate disposition interrupted by daemon shutdown"
                                );
                            }
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(error) = disposition {
                    tracing::error!(
                        phase = log_phase,
                        thread_id = %bg_thread_id,
                        %error,
                        "detached session-bound candidate disposition failed"
                    );
                    let _ = fail_thread_static_owned(
                        &bg_state,
                        &bg_thread_id,
                        "candidate_disposition_failed",
                        &launch_owner,
                    );
                    drop(bg_temp_dir.take());
                    return;
                }
                dedicated_disposition = true;
            }
            let settlement = if callback_sealed_result.is_some() {
                bg_state
                    .threads
                    .get_thread(&bg_thread_id)
                    .and_then(|thread| {
                        thread.ok_or_else(|| anyhow::anyhow!("callback-sealed thread disappeared"))
                    })
                    .map(|_| ())
            } else {
                finalize_completion(
                    &bg_state,
                    &bg_thread_id,
                    completion,
                    result_generation.as_ref(),
                    &launch_owner,
                )
                .map(|_| ())
                .map_err(anyhow::Error::new)
            };
            if let Err(err) = settlement {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %err,
                    "completion finalization failed; terminal cleanup outcome is included"
                );
            } else {
                if bg_records_terminal_generation && !dedicated_disposition {
                    if let Some(pending) = pending_project_result.take()
                        && let Err(error) = pending.publish()
                    {
                        tracing::error!(
                            phase = log_phase,
                            thread_id = %bg_thread_id,
                            %error,
                            "failed to release owner-bound fold-back publication"
                        );
                    }
                } else {
                    drop(pending_project_result.take());
                }
                let close = if bg_requires_foldback && !dedicated_disposition {
                    match bg_project_authority.terminal_publication() {
                        Some(publication) => close_terminal_workspace(
                            &bg_state,
                            bg_temp_dir.as_ref(),
                            &bg_thread_id,
                            publication,
                            result_project_snapshot_hash.as_deref(),
                        ),
                        None => Err(anyhow::anyhow!(
                            "owned workspace has no terminal publication authority"
                        )),
                    }
                } else {
                    Ok(())
                };
                if let Err(error) = close {
                    if let Some(workspace) = bg_temp_dir.as_ref() {
                        workspace.disarm();
                    }
                    tracing::error!(
                        phase = log_phase,
                        thread_id = %bg_thread_id,
                        error = %error,
                        "workspace close transition failed"
                    );
                }
            }
        }
        Err(join_err) => {
            tracing::error!(
                phase = log_phase,
                error = %join_err,
                "task panic during wait"
            );
            if let Err(cleanup_error) =
                fail_thread_static_owned(&bg_state, &bg_thread_id, "task_panic", &launch_owner)
            {
                tracing::error!(
                    phase = log_phase,
                    thread_id = %bg_thread_id,
                    error = %cleanup_error,
                    "wait-task panic and terminal cleanup both failed"
                );
            }
        }
    }
    // If no resumable snapshot was published, retain the staged project tree
    // through execution and release its publication permit now.
    release_tree_publication(bg_tree_publication, "detached execution completion");

    // Drop the Arc<TempDirGuard>. If this is the last holder, the
    // directory is removed by the TempDirGuard Drop impl.
    drop(bg_temp_dir);
}

fn fail_thread_static_owned(
    state: &AppState,
    thread_id: &str,
    outcome_code: &str,
    launch_owner: &str,
) -> Result<ExecutionCleanupOutcome> {
    match super::process_attachment::finalize_requested_stop_if_present(state, thread_id) {
        Ok(true) => return Ok(ExecutionCleanupOutcome::DurableStopSettled),
        Ok(false) => {}
        Err(error) => {
            return Err(error.context("settle durable stop before owned failure finalization"));
        }
    }
    if !state.state_store.process_attachment_admission_is_open() {
        state.state_store.reset_resume_attempts(thread_id)?;
        return Ok(ExecutionCleanupOutcome::PreservedForShutdown);
    }
    let params = ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: "failed".to_string(),
        outcome_code: Some(outcome_code.to_string()),
        result: None,
        error: Some(json!({ "code": outcome_code })),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    };
    match state.threads.finalize_thread_owned(&params, launch_owner) {
        Ok(_) => Ok(ExecutionCleanupOutcome::Finalized),
        Err(error) => {
            let terminal = state
                .threads
                .get_thread(thread_id)?
                .is_some_and(|thread| is_terminal_status(&thread.status));
            if terminal {
                Ok(ExecutionCleanupOutcome::AlreadyTerminal)
            } else {
                Err(error.context("persist owner-fenced terminal cleanup"))
            }
        }
    }
}

/// Revoke a callback token. Called on every exit path of the background task.
fn revoke_token(state: &AppState, thread_id: &str, token: &Option<String>) {
    if let Some(t) = token {
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
    if let Some(t) = token {
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
    /// Mutable local project authority is reopened directly. The admitted item
    /// itself remains sealed separately; this does not re-resolve a
    /// continuation against changed source bytes.
    LiveProject(&'a std::path::Path),
    /// Projectless work receives a fresh daemon-owned scratch cwd while
    /// retaining `ProjectContext::None` authority.
    Projectless,
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
    /// The record's project_context carries no working tree and no
    /// pushed-head identity was captured: the overlay engine cannot be
    /// rebuilt. Refuse — never silently fall back to the live tree.
    MissingPushedHeadRef(&'a ProjectContext),
}

fn decide_resume_provenance(resume: &ResumeContext) -> ResumeProvenanceDecision<'_> {
    match &resume.project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => {
            ResumeProvenanceDecision::Projectless
        }
        ryeos_state::objects::ExecutionProjectAuthority::LiveProject { canonical_root, .. } => {
            ResumeProvenanceDecision::LiveProject(canonical_root)
        }
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash,
            display_path,
            ..
        } => match &resume.original_pushed_head_ref {
            Some(pinned) => ResumeProvenanceDecision::PinnedPushedHead(pinned),
            None => match display_path.as_deref() {
                Some(original_path) => ResumeProvenanceDecision::PinnedLocalSnapshot {
                    snapshot_hash,
                    original_path,
                },
                None => ResumeProvenanceDecision::MissingPushedHeadRef(&resume.project_context),
            },
        },
    }
}

fn execution_provenance_from_resume_context(
    state: &AppState,
    resume: &ResumeContext,
) -> Result<(ExecutionProvenance, ProjectContext)> {
    match decide_resume_provenance(resume) {
        ResumeProvenanceDecision::LiveProject(project_root) => {
            let canonical = std::fs::canonicalize(project_root).map_err(|error| {
                anyhow::anyhow!(
                    "resume: live project authority {} is unavailable for {}: {error}",
                    project_root.display(),
                    resume.item_ref
                )
            })?;
            if canonical != project_root || !canonical.is_dir() {
                anyhow::bail!(
                    "resume: live project authority changed identity: expected {}, resolved {}",
                    project_root.display(),
                    canonical.display()
                );
            }
            let provenance = ExecutionProvenance::root_live_fs(
                canonical.clone(),
                Arc::clone(&state.engine),
                resume.project_authority.clone(),
            )?
            .with_state_root(resume.state_root.clone());
            Ok((provenance, ProjectContext::LocalPath { path: canonical }))
        }
        ResumeProvenanceDecision::Projectless => {
            let workspace_name = format!(
                "resume-projectless-{}-{:08x}",
                lillux::time::timestamp_millis(),
                rand::random::<u32>()
            );
            let (scratch, lifeline) = ryeos_app::temp_dir_guard::create_projectless_workspace(
                &state.config.runtime_root().cache(),
                &workspace_name,
            )
            .context("create protected projectless resume workspace")?;
            let provenance = ExecutionProvenance::root_projectless(
                scratch,
                Arc::clone(&state.engine),
                lifeline,
                resume.project_authority.clone(),
            )?;
            Ok((provenance, ProjectContext::None))
        }
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
                super::project_source::pinned_context_realization(&resume.project_authority)?,
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
                pinned.original_project_path.clone(),
                ctx.request_engine,
                lifeline,
                ctx.pinned_materialization.ok_or_else(|| {
                    anyhow::anyhow!(
                        "resume: pinned context has no verified materialization authority"
                    )
                })?,
                resume.project_authority.clone(),
            )?;
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
                super::project_source::pinned_context_realization(&resume.project_authority)?,
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
            let provenance = ExecutionProvenance::root_pushed_head(
                original_path.to_path_buf(),
                ctx.request_engine,
                lifeline,
                ctx.pinned_materialization.ok_or_else(|| {
                    anyhow::anyhow!(
                        "resume: pinned context has no verified materialization authority"
                    )
                })?,
                resume.project_authority.clone(),
            )?;
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
        ResumeProvenanceDecision::MissingPushedHeadRef(other) => {
            anyhow::bail!(
                "resume: pinned record for {} has project_context {other:?} but no \
                 stable display identity for reconstructing its exact engine; \
                 refusing to substitute a live project or another path",
                resume.item_ref,
            );
        }
    }
}

/// Transfer the original live view to a same-daemon retry claim after all
/// predecessor contacts settle. This is not cold recreation: the original
/// descriptor and its creation identity remain unchanged.
pub(crate) fn handoff_live_workspace_for_retry(
    state: &AppState,
    provenance: &ExecutionProvenance,
    thread_id: &str,
    recovery_launch_owner: &str,
) -> Result<()> {
    let Some(root_lifeline) = provenance.workspace_lifeline() else {
        return Ok(());
    };
    let Some(lifeline) = root_lifeline.owned_workspace_lifeline()? else {
        return Ok(());
    };
    let (workspace_id, view_identity) = lifeline
        .workspace_view_identity()?
        .ok_or_else(|| anyhow::anyhow!("live retry lost its original created workspace view"))?;
    let workspace = state
        .state_store
        .execution_workspace(&workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("live retry workspace journal disappeared"))?;
    if workspace.thread_id.as_deref() != Some(thread_id)
        || workspace.mount_identity.as_deref() != Some(view_identity.as_str())
    {
        anyhow::bail!("live retry cannot substitute another workspace or view");
    }
    let admitted_outputs = provenance.project_authority().workspace_outputs();
    if provenance
        .project_authority()
        .operational_snapshot_projection()
        != Some(workspace.base_snapshot.as_str())
        || workspace.workspace_output_partition_identity.as_deref()
            != admitted_outputs.map(|outputs| outputs.partition.partition_identity.as_str())
        || workspace.base_output_capture_hash.as_deref()
            != admitted_outputs.and_then(|outputs| outputs.capture_hash.as_deref())
    {
        anyhow::bail!("live retry workspace source/output generation changed");
    }
    let frozen_generation = workspace.frozen_generation()?;
    if workspace.state == WorkspaceState::Freezing && frozen_generation.is_none() {
        anyhow::bail!("live retry frozen workspace has no paired result generation");
    }
    if let Some(identity) =
        ryeos_app::dedicated_session_service::workspace_worker_capture_identity(state, &workspace)?
    {
        ryeos_app::process::assert_reaped_process_group_absent(&identity)?;
    }
    if let Some(identity) = workspace.process_identity.as_deref() {
        ryeos_app::process::assert_reaped_process_group_absent(&serde_json::from_str(identity)?)?;
    }
    let previous_owner = workspace
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("live retry workspace has no previous owner"))?;
    state
        .state_store
        .handoff_execution_workspace_for_live_retry(
            &workspace_id,
            thread_id,
            previous_owner,
            recovery_launch_owner,
            workspace.state,
            workspace.process_identity.as_deref(),
            &view_identity,
        )?;
    // Transfer only the original view owner. A recovered frozen disposition
    // launches no process and must not acquire an unattachable borrower row.
    // Any actual resumed launch binds at its ordinary before-contact boundary.
    Ok(())
}

/// Cold recovery creates a new operational view over the exact retained
/// backing state only after predecessor-daemon/process settlement. The
/// caller's existing lifecycle owner must cover construction and every
/// fallible reconstruction step before the resumed launcher takes over.
pub(crate) fn retained_workspace_provenance_for_native_resume(
    state: &AppState,
    thread_id: &str,
    recovery_launch_owner: &str,
    resume: &ResumeContext,
    preparation_owner: &mut super::process_attachment::LifecycleOwnerGuard,
) -> Result<Option<ExecutionProvenance>> {
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        snapshot_hash,
        realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
        workspace_outputs,
        ..
    } = &resume.project_authority
    else {
        return Ok(None);
    };
    let original_project_path = match decide_resume_provenance(resume) {
        ResumeProvenanceDecision::PinnedPushedHead(pinned) => {
            if pinned.snapshot_hash != *snapshot_hash {
                anyhow::bail!("retained pushed-head identity changed before native resume");
            }
            pinned.original_project_path.clone()
        }
        ResumeProvenanceDecision::PinnedLocalSnapshot {
            snapshot_hash: retained_snapshot,
            original_path,
        } => {
            if retained_snapshot != snapshot_hash {
                anyhow::bail!("retained local snapshot identity changed before native resume");
            }
            original_path.to_path_buf()
        }
        _ => return Ok(None),
    };
    let Some(workspace) = state
        .state_store
        .execution_workspace_for_thread(thread_id)?
    else {
        return Ok(None);
    };
    if workspace.thread_id.as_deref() != Some(thread_id)
        || workspace.base_snapshot != *snapshot_hash
        || workspace.workspace_output_partition_identity.as_deref()
            != workspace_outputs
                .as_ref()
                .map(|outputs| outputs.partition.partition_identity.as_str())
        || workspace.base_output_capture_hash.as_deref()
            != workspace_outputs
                .as_ref()
                .and_then(|outputs| outputs.capture_hash.as_deref())
        || !matches!(
            workspace.state,
            WorkspaceState::Ready | WorkspaceState::Active | WorkspaceState::Freezing
        )
        || workspace.backend_id.is_none()
        || workspace.backend_version.is_none()
        || workspace.pinned_root_identities.is_none()
        || workspace.mount_identity.is_none()
    {
        anyhow::bail!("retained execution workspace journal is incomplete or contradictory");
    }
    // Cold recreation must also settle the existing worker owner. A failed
    // start may have no root member or workspace PID while retaining unknown
    // process contact in its dedicated-session/credential authority.
    if let Some(identity) =
        ryeos_app::dedicated_session_service::workspace_worker_capture_identity(state, &workspace)?
    {
        ryeos_app::process::assert_reaped_process_group_absent(&identity)?;
    }
    let previous_launch_owner = workspace
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("retained execution workspace has no launch owner"))?;
    let recorded_process_identity = workspace
        .process_identity
        .as_deref()
        .map(serde_json::from_str::<ryeos_app::process::ExecutionProcessIdentity>)
        .transpose()
        .context("decode retained execution-workspace process identity")?;
    if workspace.state == WorkspaceState::Ready && recorded_process_identity.is_some() {
        anyhow::bail!("ready retained execution workspace still has a process attachment");
    }
    let frozen_generation = workspace.frozen_generation()?;
    if workspace.state == WorkspaceState::Freezing && frozen_generation.is_none() {
        anyhow::bail!("frozen retained execution workspace has no candidate generation");
    }
    if recorded_process_identity.as_ref().is_some_and(|identity| {
        ryeos_app::process::execution_group_liveness(identity)
            != ryeos_app::process::IdentityLiveness::DeadOrStale
    }) {
        anyhow::bail!("retained execution workspace process owner is not proved dead");
    }
    let root = PathBuf::from(&workspace.root_path);
    if root.file_name().and_then(|name| name.to_str()) != Some(workspace.workspace_id.as_str()) {
        anyhow::bail!("retained execution workspace root does not encode its journal identity");
    }
    let layout = super::workspace::WorkspaceLayout::from_root(root.clone());
    let pinned_roots: BTreeMap<String, String> = serde_json::from_str(
        workspace
            .pinned_root_identities
            .as_deref()
            .expect("complete retained workspace has pinned roots"),
    )
    .context("decode retained workspace root identities")?;
    let expected_project_identity = pinned_roots
        .get("project")
        .ok_or_else(|| anyhow::anyhow!("retained workspace journal has no project identity"))?;

    // Build the immutable project engine from a shared read-only realization;
    // mutable workspace bytes are never re-admitted as engine configuration.
    tracing::debug!(
        thread_id,
        recovery_stage = "project-engine-reconstruction",
        "retained workspace recovery stage"
    );
    let resolved = super::project_source::resolve_pinned_snapshot_context(
        state,
        snapshot_hash,
        original_project_path.clone(),
        "retained-native-resume-resolution",
        super::project_source::PinnedContextRealization::ReadOnly,
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    tracing::debug!(
        thread_id,
        recovery_stage = "project-engine-reconstructed",
        "retained workspace recovery stage"
    );
    let materialization = {
        let authority = super::pinned_state_authority(state)?;
        let cas_guard = authority.acquire_shared_guard()?;
        let closure = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
            &authority.cas_store()?,
            snapshot_hash,
        )?;
        ryeos_state::PinnedProjectMaterialization::recover_retained_workspace_from_closure(
            &authority,
            &cas_guard,
            &closure,
            &layout.project,
            expected_project_identity,
        )?
    };
    tracing::debug!(
        thread_id,
        recovery_stage = "retained-backing-recovered",
        "retained workspace recovery stage"
    );
    // The materialization now owns its exact pinned root, CAS and loaded tree;
    // it does not borrow the mutation guard. End that guard's fork-sensitive
    // descriptor lease before rebind/Create can spawn the workspace creator.
    // Never carry CAS locking through process preparation or weaken Lillux's
    // fork admission to make cold reconstruction succeed.
    state.state_store.rebind_execution_workspace_for_recovery(
        &workspace.workspace_id,
        thread_id,
        previous_launch_owner,
        recovery_launch_owner,
        workspace.state,
        workspace.process_identity.as_deref(),
    )?;
    let lifeline = Arc::new(TempDirGuard::new_workspace(root, layout.project)?);
    preparation_owner.track_owned_workspace_lifeline(lifeline.clone())?;
    let provenance = ExecutionProvenance::root_pushed_head(
        original_project_path,
        resolved.request_engine,
        lifeline,
        materialization,
        resume.project_authority.clone(),
    )?;
    // Cold rebind owns a NEW construction incarnation. Old root identities
    // are retained only to verify the exact backing state, never as evidence
    // that a vanished daemon descriptor survived restart. Construction is not
    // borrower admission: recovered disposition may only finish a previously
    // frozen result and never launch a target. The resumed launch binds its
    // member separately at the ordinary before-contact boundary.
    tracing::debug!(
        thread_id,
        recovery_stage = "workspace-rebind",
        "retained workspace recovery stage"
    );
    prepare_owned_workspace_after_thread_birth(
        state,
        &provenance,
        thread_id,
        recovery_launch_owner,
        WorkspaceConstructionInput::RetainedBacking(&workspace),
    )?;
    tracing::debug!(
        thread_id,
        recovery_stage = "workspace-rebound",
        "retained workspace recovery stage"
    );
    if workspace.state == WorkspaceState::Freezing {
        let frozen = frozen_generation.as_ref().ok_or_else(|| {
            anyhow::anyhow!("retained frozen disposition has no committed result generation")
        })?;
        let reconstructed = state
            .state_store
            .execution_workspace(&workspace.workspace_id)?
            .ok_or_else(|| anyhow::anyhow!("reconstructed frozen workspace disappeared"))?;
        if reconstructed.frozen_generation()?.as_ref() != Some(frozen) {
            anyhow::bail!("workspace reconstruction changed its committed frozen generation");
        }
        // Create/Ready proves the NEW view exists; restoring the exact prior
        // frozen phase does not admit a writer or infer a freeze from a stale
        // hash on an otherwise Active/Ready workspace.
        state.state_store.transition_execution_workspace_owned(
            &workspace.workspace_id,
            thread_id,
            recovery_launch_owner,
            &[WorkspaceState::Ready],
            WorkspaceState::Freezing,
            None,
        )?;
    }
    tracing::info!(
        thread_id,
        workspace_id = %workspace.workspace_id,
        snapshot_hash,
        "native resume retained the exact unpublished execution workspace"
    );
    Ok(Some(provenance))
}

/// Reconstruct a created root from its exact, already-admitted authority.
///
/// Unlike ordinary crash resume, this path must not resolve an item ref or
/// re-run admission: the slot and launch metadata were durably committed only
/// after the complete verified request was sealed. The resume context remains
/// the independently persisted launch envelope identity and parent authority;
/// every overlapping field must agree before the request can be used.
pub fn execution_params_from_sealed_root_request(
    state: &AppState,
    thread_id: &str,
    resume: &ResumeContext,
    sealed: &SealedRootExecutionRequest,
    provenance_override: Option<ExecutionProvenance>,
) -> Result<ExecutionParams> {
    sealed.validate_current_operator_authority(state)?;
    let provenance = match sealed.candidate_evaluation_authority() {
        Some(authority) => {
            candidate_evaluation_provenance_from_resume_context(
                state,
                resume,
                authority,
                provenance_override,
            )?
            .0
        }
        None => match provenance_override {
            Some(provenance) => provenance,
            None => execution_provenance_from_resume_context(state, resume)?.0,
        },
    };
    if sealed.project_authority() != &resume.project_authority
        || sealed.project_authority() != provenance.project_authority()
        || sealed.project_context() != &resume.project_context
    {
        anyhow::bail!(
            "created-root sealed, resume, and reconstructed provenance identities disagree for {}",
            resume.item_ref
        );
    }
    let resolved = sealed.restore_for_reconstructed_provenance(
        provenance.request_engine(),
        &ryeos_app::launch_metadata::daemon_thread_state_dir(&state.config.app_root, thread_id)
            .join("launch-capsule"),
        &provenance,
    )?;
    resolved
        .root_admission
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("restored sealed root has no admission"))?
        .ensure_matches_provenance(&provenance)?;
    let mut operational_resume = resume.clone();
    operational_resume.project_context = resolved.plan_context.project_context.clone();
    let acting_principal = operational_resume.principal_identifier().to_string();

    if resolved.kind != operational_resume.kind
        || resolved.item_ref != operational_resume.item_ref
        || resolved.launch_mode != operational_resume.launch_mode
        || resolved.parameters != operational_resume.parameters
        || resolved.ref_bindings != operational_resume.ref_bindings
        || resolved.current_site_id != operational_resume.current_site_id
        || resolved.origin_site_id != operational_resume.origin_site_id
        || resolved.requested_by.as_deref() != Some(acting_principal.as_str())
        || resolved.plan_context.requested_by != operational_resume.requested_by
        || resolved.plan_context.project_context != operational_resume.project_context
        || resolved.plan_context.execution_hints != operational_resume.execution_hints
        || resolved.plan_context.scheduled_fire != operational_resume.scheduled_fire
        || operational_resume.executor_ref.as_deref() != Some(sealed.executor_ref())
        || operational_resume.runtime_ref.as_deref() != Some(sealed.runtime_ref())
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
        handler_context: sealed.handler_context().cloned(),
        vault_bindings: HashMap::new(),
        pre_minted_thread_id: None,
        effective_caps: operational_resume.effective_caps.clone(),
        provenance,
        lifecycle_authority: operational_resume.lifecycle_authority,
        runtime_ref: Some(sealed.runtime_ref().to_string()),
        // The created row already carries any operational parent link. This
        // reconstruction must not try to attach it a second time at launch.
        parent_thread_id: None,
        effect_authority: None,
        finalized_direct: None,
    })
}

/// Reconstruct both immutable legs of a candidate operation after restart.
/// The executable closure is restored from `base_snapshot_hash`; the runtime
/// sees only the separately materialized candidate generation. Neither leg is
/// rebuilt from the live project or current HEAD.
pub(crate) fn candidate_evaluation_provenance_from_resume_context(
    state: &AppState,
    resume: &ResumeContext,
    authority: &ryeos_app::thread_lifecycle::CandidateEvaluationAuthority,
    retained_candidate_provenance: Option<ExecutionProvenance>,
) -> Result<(ExecutionProvenance, ProjectContext)> {
    use ryeos_state::objects::{
        ChildProjectAuthorityPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        PinnedProjectRealization, PinnedTerminalPublication,
    };

    authority.validate()?;
    let (
        stable_project_identity,
        original_project_path,
        candidate_base,
        candidate_snapshot,
        candidate_realization,
        candidate_environment,
        candidate_capabilities,
    ) = match &resume.project_authority {
        ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity,
            display_path: Some(display_path),
            base_snapshot_hash,
            snapshot_hash,
            realization,
            environment,
            capability_ceiling,
            ..
        } => (
            stable_project_identity.clone(),
            display_path.clone(),
            base_snapshot_hash,
            snapshot_hash,
            realization,
            environment,
            capability_ceiling.clone(),
        ),
        _ => anyhow::bail!(
            "candidate operation recovery requires a pinned candidate with stable display identity"
        ),
    };
    let owner = resume.principal_identifier();
    let candidate_mode_ok = match &authority.purpose {
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate => {
            candidate_base == &authority.candidate_snapshot_hash
                && matches!(
                    candidate_realization,
                    PinnedProjectRealization::ReadOnly
                        | PinnedProjectRealization::Cow {
                            terminal_publication: PinnedTerminalPublication::Discard,
                        }
                )
                && !candidate_capabilities.iter().any(|capability| {
                    capability == ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY
                })
        }
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. } => {
            candidate_base == &authority.base_snapshot_hash
                && matches!(
                    candidate_realization,
                    PinnedProjectRealization::Cow {
                        terminal_publication:
                            PinnedTerminalPublication::RetainCurrentHead {
                                expected_hash,
                                ..
                            },
                    } if expected_hash == &authority.base_snapshot_hash
                )
                && !candidate_capabilities.iter().any(|capability| {
                    capability == ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY
                })
        }
    };
    if owner != authority.owner_principal
        || candidate_snapshot != &authority.candidate_snapshot_hash
        || !candidate_mode_ok
        || candidate_environment != &EnvironmentAuthority::None
    {
        anyhow::bail!("candidate operation resume authority contradicts its sealed coordinate");
    }

    let base_checkout_id = format!(
        "candidate-evaluator-base-{}-{:08x}",
        lillux::time::timestamp_millis(),
        rand::random::<u32>()
    );
    let base = super::project_source::resolve_pinned_snapshot_context(
        state,
        &authority.base_snapshot_hash,
        original_project_path.clone(),
        &base_checkout_id,
        super::project_source::PinnedContextRealization::ReadOnly,
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let base_lifeline = base.temp_dir.clone().ok_or_else(|| {
        anyhow::anyhow!("candidate operation base materialization has no workspace lifeline")
    })?;
    let base_materialization = base.pinned_materialization.clone().ok_or_else(|| {
        anyhow::anyhow!("candidate operation base materialization has no CAS proof")
    })?;
    let base_authority = ExecutionProjectAuthority::pinned(
        stable_project_identity,
        Some(original_project_path.clone()),
        authority.base_snapshot_hash.clone(),
        PinnedProjectRealization::ReadOnly,
        EnvironmentAuthority::None,
        candidate_capabilities,
    )?
    .with_child_policy(ChildProjectAuthorityPolicy::Inherit)?;
    let base_provenance = ExecutionProvenance::root_pushed_head(
        original_project_path.clone(),
        base.request_engine.clone(),
        base_lifeline,
        base_materialization,
        base_authority,
    )?;
    let base_plan_context = ryeos_engine::contracts::PlanContext {
        requested_by: resume.requested_by.clone(),
        project_context: ProjectContext::LocalPath {
            path: base.effective_path.clone(),
        },
        subject_resolution_authority: base_provenance.subject_resolution_authority(),
        current_site_id: resume.current_site_id.clone(),
        origin_site_id: resume.origin_site_id.clone(),
        execution_hints: resume.execution_hints.clone(),
        scheduled_fire: resume.scheduled_fire.clone(),
        validate_only: false,
    };
    let base_binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
        &base.request_engine,
        &base_plan_context,
        &base_provenance,
    )?;
    let scope = Arc::new(
        ryeos_app::thread_lifecycle::CandidateEvaluationExecutionScope::admit(
            authority.clone(),
            base_plan_context,
            base_binding,
        )?,
    );

    if let Some(retained) = retained_candidate_provenance {
        // Native COW recovery has already re-opened and journal-validated the
        // exact retained candidate workspace. Preserve that path,
        // materialization and authority, but replace its temporary C-derived
        // resolution engine with the independently verified immutable-B
        // engine before attaching the dual-authority scope. Re-materializing C
        // here would overwrite integration output already present in the
        // retained workspace.
        if retained.is_borrowed_child()
            || retained.project_authority() != &resume.project_authority
            || retained.original_project_path() != original_project_path.as_path()
        {
            anyhow::bail!("retained candidate workspace contradicts its sealed resume authority");
        }
        let candidate_lifeline = retained.workspace_lifeline().ok_or_else(|| {
            anyhow::anyhow!("retained candidate workspace has no ownership lifeline")
        })?;
        let candidate_materialization =
            retained.pinned_materialization().cloned().ok_or_else(|| {
                anyhow::anyhow!("retained candidate workspace has no materialization proof")
            })?;
        let effective_path = retained.effective_path().to_path_buf();
        let provenance = ExecutionProvenance::root_pushed_head(
            original_project_path,
            base.request_engine,
            candidate_lifeline,
            candidate_materialization,
            resume.project_authority.clone(),
        )?
        .with_candidate_evaluation_scope(scope)?;
        return Ok((
            provenance,
            ProjectContext::LocalPath {
                path: effective_path,
            },
        ));
    }

    let candidate_checkout_id = format!(
        "candidate-evaluator-workspace-{}-{:08x}",
        lillux::time::timestamp_millis(),
        rand::random::<u32>()
    );
    let candidate = super::project_source::resolve_pinned_snapshot_context(
        state,
        &authority.candidate_snapshot_hash,
        original_project_path.clone(),
        &candidate_checkout_id,
        super::project_source::pinned_context_realization(&resume.project_authority)?,
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let candidate_lifeline = candidate.temp_dir.clone().ok_or_else(|| {
        anyhow::anyhow!("candidate operation materialization has no workspace lifeline")
    })?;
    let candidate_materialization = candidate
        .pinned_materialization
        .ok_or_else(|| anyhow::anyhow!("candidate operation materialization has no CAS proof"))?;
    let effective_path = candidate.effective_path.clone();
    let provenance = ExecutionProvenance::root_pushed_head(
        original_project_path,
        base.request_engine,
        candidate_lifeline,
        candidate_materialization,
        resume.project_authority.clone(),
    )?
    .with_candidate_evaluation_scope(scope)?;
    Ok((
        provenance,
        ProjectContext::LocalPath {
            path: effective_path,
        },
    ))
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
/// semantics. See `.ai/knowledge/ryeos/future/native-resume-snapshot-pinning.md`
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
    params: ExecutionParams,
    prior_status: String,
) -> Result<RecoveryLaunchOutcome, ResumeError> {
    run_existing_recovered_thread(
        state,
        thread_id,
        chain_root_id,
        params,
        prior_status,
        DetachedDispatchKind::NativeResume,
    )
    .await
}

/// Launch a root whose admitted birth committed but whose first process never
/// attached. This uses the same durable ownership and detached supervision as
/// native recovery without injecting checkpoint-resume semantics into a program
/// that has not run yet.
pub async fn run_existing_admitted_root(
    state: AppState,
    thread_id: String,
    chain_root_id: String,
    params: ExecutionParams,
    prior_status: String,
) -> Result<RecoveryLaunchOutcome, ResumeError> {
    run_existing_recovered_thread(
        state,
        thread_id,
        chain_root_id,
        params,
        prior_status,
        DetachedDispatchKind::RecoveredAdmittedRoot,
    )
    .await
}

async fn run_existing_recovered_thread(
    state: AppState,
    thread_id: String,
    chain_root_id: String,
    mut params: ExecutionParams,
    prior_status: String,
    dispatch_kind: DetachedDispatchKind,
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
    if matches!(dispatch_kind, DetachedDispatchKind::RecoveredAdmittedRoot)
        && (thread.status != ryeos_state::objects::ThreadStatus::Created.as_str()
            || thread.upstream_thread_id.is_some())
    {
        return Ok(RecoveryLaunchOutcome::Skipped("not_fresh_created_root"));
    }
    // Process attach precedes the `created -> running` transition, so liveness
    // is checked for every nonterminal status. A duplicate recovery must never
    // spawn beside an already-attached tool subprocess.
    if thread.runtime.process_identity.is_some() {
        // Only reconciliation owns the liveness proof and compare-clear that
        // retires an attached process. A dead group leader is not proof that
        // the complete process group is absent, so a recovery launcher never
        // deletes input state or replaces the process on its own.
        return Ok(RecoveryLaunchOutcome::Skipped(
            "attached_process_not_reconciled",
        ));
    }
    let mut guard = ExecutionGuard::new(state.clone());
    guard.track_thread(&thread_id);
    let resume_launch_owner = resume_claim.canonical_owner()?;
    guard.track_launch_owner(resume_launch_owner.clone());

    // Prepare CAS context.
    let PreparedCasContext {
        mut effective_path,
        pre_tree_hash,
        pre_policy_hash,
        resume_snapshot_hash,
        tree_publication,
    } = match prepare_cas_context(&state, &params.provenance, &thread_id, &mut guard) {
        Ok(ctx) => ctx,
        Err(err) => {
            guard.fail_thread("cas_context_failed");
            guard.cleanup();
            return Err(ResumeError::CasContext(err));
        }
    };
    bind_owned_workspace_after_thread_birth(
        &state,
        &params.provenance,
        &thread_id,
        &resume_launch_owner,
    )
    .map_err(ResumeError::CasContext)?;

    // Successful binding above establishes this fresh launch owner's exact
    // membership. From here until the detached handoff, explicit preparation
    // errors can settle that membership: no spawn task has been scheduled.
    // Earlier recovery refusals do not classify old uncertain contact as safe.
    //
    // Update plan_context to point at the materialized path so the
    // engine resolves item refs from there.
    if matches!(
        params.provenance.project_authority(),
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration { .. }
    ) && effective_path != params.provenance.effective_path()
    {
        params.resolved.plan_context.project_context = ProjectContext::LocalPath {
            path: effective_path.clone(),
        };
    }

    // Rebuild the signed protocol environment for the recovered subprocess.
    // Credentials are fresh when declared and absent for callback-free tools;
    // originals, if any, were revoked with the prior background owner.
    // Recovery remints the same root-scoped callback authority sealed at the
    // original launch. It must not pre-project the root into a borrowed child.
    let callback_provenance = params.provenance.clone();
    let engine = params.provenance.request_engine().clone();
    let admitted_capsule = state
        .state_store
        .admitted_launch_capsule(&thread_id)
        .map_err(|error| {
            guard.fail_before_spawn(error.context("admitted_execution_closure_unavailable"))
        })?
        .ok_or_else(|| {
            guard.fail_before_spawn(anyhow::anyhow!(
                "direct recovery has no admitted launch capsule"
            ))
        })?;
    validate_recovered_direct_request_authority(&state, &thread_id, &params, &admitted_capsule)
        .map_err(|error| {
            guard.fail_before_spawn(error.context("admitted_program_authority_invalid"))
        })?;
    let retained_admission = params.resolved.root_admission.as_ref().ok_or_else(|| {
        guard.fail_before_spawn(anyhow::anyhow!("restored sealed root has no admission"))
    })?;
    let retained_resolution = retained_admission.resolution_output();
    let mut selection_resolution = retained_resolution.clone();
    ryeos_app::operator_external_content::product_composition::admit_root_product_selections(
        &state,
        &params.resolved.current_site_id,
        &engine,
        &engine.resolution_roots(
            retained_admission
                .resolution_workspace()
                .map(std::path::Path::to_path_buf),
        ),
        retained_admission.resolution_subject_authority(),
        &mut selection_resolution,
        params.resolved.requested_by.as_deref(),
        params.handler_context.as_ref(),
        &params.resolved.product_selections,
        true,
    )
    .map_err(|error| guard.fail_before_spawn(error))?;
    ryeos_app::source_closure_admission::recover_source_closure(
        &state,
        &engine,
        retained_resolution,
    )
    .map_err(|error| guard.fail_before_spawn(error))?;
    let PreparedProcessInputs {
        path: process_path,
        lifeline: process_input_lifeline,
        isolation_project_authority: bg_isolation_project_authority,
        isolation_immutable_project: bg_isolation_immutable_project,
        isolation_live_access_authority: bg_isolation_live_access_authority,
        external: bg_external_realizations,
        source: bg_source_closure,
    } = prepare_process_inputs(
        &state,
        &params.provenance,
        &thread_id,
        retained_resolution,
        &effective_path,
    )
    .map_err(|error| {
        guard.fail_before_spawn(error.context("recovery_input_materialization_failed"))
    })?;
    effective_path = process_path;
    if let Some(lifeline) = process_input_lifeline {
        guard.track_process_input_dir(lifeline);
    }
    if matches!(
        &params.resolved.plan_context.project_context,
        ProjectContext::LocalPath { .. }
    ) {
        params.resolved.plan_context.project_context = ProjectContext::LocalPath {
            path: effective_path.clone(),
        };
    }
    // Same runtime-state selection as a fresh direct launch. Recovery creates
    // a fresh private input root and never adopts scratch from the prior
    // process attempt.
    let runtime_state_root = params
        .provenance
        .state_root_override()
        .unwrap_or(params.provenance.effective_path())
        .to_path_buf();
    let cas_root = state
        .state_store
        .cas_root()
        .map_err(|error| guard.fail_before_spawn(error))?;
    let cas_directory = lillux::PinnedDirectory::open(&cas_root)
        .map_err(|error| guard.fail_before_spawn(error))?
        .ok_or_else(|| guard.fail_before_spawn(anyhow::anyhow!("state CAS root is unavailable")))?;
    let cas = lillux::CasStore::from_pinned_root(cas_directory);
    let effective_project_root = match &params.resolved.plan_context.project_context {
        ProjectContext::LocalPath { path } => Some(path.as_path()),
        ProjectContext::None
        | ProjectContext::SnapshotHash { .. }
        | ProjectContext::ProjectRef { .. } => None,
    };
    let mut prepared_plan = thread_lifecycle::PreparedItemPlan::recover_from_execution_closure(
        &admitted_capsule,
        &cas,
        state.isolation.as_ref(),
        effective_project_root,
    )
    .map_err(|error| {
        guard.fail_before_spawn(error.context("admitted_execution_closure_invalid"))
    })?;
    super::external_content::bind_prepared_realization_command(
        &mut prepared_plan,
        bg_external_realizations.as_ref(),
        state.isolation.as_ref(),
    )
    .map_err(|error| {
        guard.fail_before_spawn(error.context("admitted_realization_command_invalid"))
    })?;
    if params.provenance.project_source()
        == ryeos_app::execution_provenance::ProjectSourceKind::LiveFs
        && retained_resolution_has_filesystem_bindings(retained_resolution)
            .map_err(|error| guard.fail_before_spawn(error))?
    {
        prepared_plan
            .ensure_no_project_local_interpreter(Path::new(
                ryeos_app::thread_lifecycle::ADMITTED_DIRECT_PROJECT_ROOT,
            ))
            .map_err(|error| guard.fail_before_spawn(error))?;
    }
    let launch_timeout_secs = prepared_plan.timeout_secs;
    let protocol = recovered_direct_protocol(
        &engine,
        &admitted_capsule,
        &params.resolved.resolved_item.kind,
    )
    .map_err(|error| guard.fail_before_spawn(error.context("admitted_protocol_closure_invalid")))?;
    crate::dispatch::validate_direct_result_retention(
        &protocol,
        params
            .resolved
            .root_admission
            .as_ref()
            .context("recovered output lacks retained root admission")
            .map_err(|error| guard.fail_before_spawn(error))?
            .resolved_result_policy()
            .retention,
        &params.resolved.resolved_item.kind,
    )
    .map_err(anyhow::Error::new)
    .map_err(|error| guard.fail_before_spawn(error.context("admitted_output_retention_invalid")))?;
    let root_admission = params.resolved.root_admission.as_ref().ok_or_else(|| {
        guard.fail_before_spawn(anyhow::anyhow!(
            "direct recovery has no exact admitted root resolution"
        ))
    })?;
    let primary_resolution = root_admission.resolution_output();
    let current_trust_store = root_admission
        .current_policy_trust_store()
        .map_err(|error| guard.fail_before_spawn(error))?;
    super::admitted_trust::validate_direct_current_trust(
        &engine,
        &current_trust_store,
        primary_resolution,
        prepared_plan.execution_plan(),
        &admitted_capsule,
    )
    .map_err(|error| {
        guard.fail_before_spawn(error.context("admitted_program_authority_revoked"))
    })?;

    // Read credentials only after the exact plan, protocol, executor, and
    // executable authority match the CAS-rooted admitted capsule. Secret
    // overlays remain operator input outside immutable project snapshots, but
    // substituted code can never use admission as a credential oracle.
    {
        let secret_requirements = crate::execution::launch::build_secret_requirements(
            &params.resolved.resolved_item.metadata.required_secrets,
        );
        let secret_names: Vec<String> = secret_requirements
            .iter()
            .map(|req| req.name.clone())
            .collect();
        let vault_bindings = ryeos_app::vault::read_required_secrets_with_authority(
            state.vault.as_ref(),
            &params.acting_principal,
            &secret_names,
            params.provenance.project_authority(),
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
                ResumeError::VaultRead(
                    guard.fail_before_spawn(
                        ryeos_app::vault::VaultReadError::MissingSecrets {
                            principal: params.acting_principal.clone(),
                            names,
                        }
                        .into(),
                    ),
                )
            }
            error @ ryeos_app::vault::VaultReadError::AuthorityViolation(_) => {
                guard.fail_thread("vault_read_failed");
                ResumeError::VaultRead(guard.fail_before_spawn(error.into()))
            }
            ryeos_app::vault::VaultReadError::Internal(e) => {
                guard.fail_thread("vault_read_failed");
                ResumeError::VaultRead(
                    guard.fail_before_spawn(ryeos_app::vault::VaultReadError::Internal(e).into()),
                )
            }
        })?;

        params.vault_bindings = vault_bindings;
    }
    if super::process_attachment::finalize_requested_stop_if_present(&state, &thread_id)
        .map_err(|error| guard.fail_before_spawn(error))?
    {
        fail_settled_unattached_thread(
            &state,
            &thread_id,
            "stopped_before_recovery_spawn",
            &resume_launch_owner,
        )?;
        guard.mark_finalized();
        guard.cleanup();
        return Ok(RecoveryLaunchOutcome::Skipped("stop_requested"));
    }
    let ProtocolLaunchEnv {
        bindings: protocol_env_bindings,
        callback_token,
        thread_auth_token,
        isolation_daemon_socket_path,
    } = build_protocol_launch_env(
        &state,
        &protocol,
        &thread_id,
        &effective_path,
        &runtime_state_root,
        Some(launch_timeout_secs),
        params.effective_caps.clone(),
        &params.acting_principal,
        params.handler_context.as_ref(),
        &params.resolved.current_site_id,
        &params.resolved.origin_site_id,
        callback_provenance,
        &params.resolved.item_ref,
        params.resolved.root_raw_content_digest.clone(),
        effective_bundle_id_for_request(&params.resolved),
        &resume_launch_owner,
    )
    .map_err(|error| guard.fail_before_spawn(error.context("protocol_contract_failed")))?;
    if let Some(token) = callback_token {
        guard.track_callback_token(token);
    }
    if let Some(token) = thread_auth_token {
        guard.track_thread_auth_token(token);
    }

    // Carry the already-sealed operational authority unchanged. Effective
    // runtime capabilities remain a separate launch fact and never rewrite
    // project authority after admission.
    let bg_project_authority = params.provenance.project_authority().clone();
    let bg_candidate_operation_authority = params
        .provenance
        .candidate_evaluation_scope()
        .map(|scope| scope.authority().clone());
    let parts = guard.into_detached_parts();
    let bg_state = parts.state;
    let bg_temp_dir = parts.temp_dir;
    let bg_process_input_dir = parts.process_input_dir;
    let bg_cb_token = parts.callback_token;
    let bg_tat_token = parts.thread_auth_token;
    let bg_thread_id = thread_id.clone();
    let bg_chain_root_id = chain_root_id.clone();
    let bg_resolved = params.resolved.clone();
    let bg_prepared_plan = prepared_plan;
    let bg_stdout_shape = protocol.descriptor.stdout.shape;
    // Per-request engine selected from the sealed project authority.
    let bg_engine = engine;
    let bg_vault = params.vault_bindings.clone();
    let bg_protocol_env_bindings = protocol_env_bindings;
    let bg_acting_principal = params.acting_principal.clone();
    let bg_pre_tree_hash = pre_tree_hash;
    let bg_pre_policy_hash = pre_policy_hash;
    let bg_resume_snapshot_hash = resume_snapshot_hash;
    let bg_tree_publication = tree_publication;
    let bg_project_path = Some(params.provenance.original_project_path().to_path_buf());
    let bg_skip_resume_snapshot_pin = params.provenance.is_borrowed_child();
    let bg_terminal_publication = params
        .provenance
        .project_authority()
        .terminal_publication()
        .cloned();
    let bg_state_root = params
        .provenance
        .state_root_override()
        .map(std::path::Path::to_path_buf);
    let bg_runtime_state_dir = state.config.app_root.clone();
    let bg_isolation_workspace =
        projectless_isolation_workspace(process_project_class(&params.provenance), &effective_path);
    tokio::spawn(dispatch_detached_bg_task(
        bg_state,
        bg_thread_id,
        bg_chain_root_id,
        bg_resolved,
        bg_prepared_plan,
        bg_engine,
        bg_vault,
        bg_protocol_env_bindings,
        bg_stdout_shape,
        bg_acting_principal,
        bg_pre_tree_hash,
        bg_pre_policy_hash,
        bg_resume_snapshot_hash,
        bg_tree_publication,
        bg_project_path,
        bg_project_authority,
        bg_candidate_operation_authority,
        bg_state_root,
        bg_isolation_workspace,
        bg_isolation_project_authority,
        bg_isolation_immutable_project,
        bg_isolation_live_access_authority,
        isolation_daemon_socket_path,
        bg_temp_dir,
        bg_process_input_dir,
        bg_skip_resume_snapshot_pin,
        bg_terminal_publication,
        bg_external_realizations,
        bg_source_closure,
        bg_runtime_state_dir,
        dispatch_kind,
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

    #[test]
    fn release_diagnostics_never_substitute_for_kernel_cleanup_testimony() {
        assert!(!release_cleanup_is_settled(&anyhow::anyhow!(
            "cleanup completed synchronously"
        )));
        assert!(!release_cleanup_is_settled(&anyhow::Error::new(
            ryeos_engine::error::EngineError::ExecutionFailed {
                reason: "no process remains".to_owned(),
            },
        )));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn direct_precontact_failure_settles_only_current_child_membership() {
        let (_temp, state) =
            crate::augmentations::compose_context_positions::tests::bound_runtime_children_fixture(
            );
        let store = &state.state_store;
        let child = "T-runtime-child";
        let claim = store.get_launch_claim(child).unwrap().unwrap();
        let parent = store.thread_workspace_binding("T-runtime-parent").unwrap();
        let sibling = store.thread_workspace_binding("T-runtime-sibling").unwrap();
        let mut guard = ExecutionGuard::new(state.clone());
        guard.track_thread(child);
        guard.track_launch_owner(claim.claimed_by.clone());
        let error = guard.fail_before_spawn(anyhow::anyhow!("invalid preparation"));
        assert!(error.to_string().contains("invalid preparation"));
        assert!(guard.thread_finalized);
        assert!(is_terminal_status(
            &store.get_thread(child).unwrap().unwrap().status
        ));
        assert!(store.thread_workspace_binding(child).unwrap().is_none());
        assert!(store.is_launch_owner_active(&claim.claimed_by));
        assert_eq!(
            store.thread_workspace_binding("T-runtime-parent").unwrap(),
            parent
        );
        assert_eq!(
            store.thread_workspace_binding("T-runtime-sibling").unwrap(),
            sibling
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn direct_terminal_failure_without_contact_proof_retains_membership() {
        let (_temp, state) =
            crate::augmentations::compose_context_positions::tests::bound_runtime_children_fixture(
            );
        let store = &state.state_store;
        let child = "T-runtime-child";
        let claim = store.get_launch_claim(child).unwrap().unwrap();
        let binding = store.thread_workspace_binding(child).unwrap();
        let sibling_owner = store
            .get_launch_claim("T-runtime-sibling")
            .unwrap()
            .unwrap();
        assert!(
            fail_settled_unattached_thread(
                &state,
                child,
                "invalid_owner",
                &sibling_owner.claimed_by
            )
            .is_err()
        );
        assert_eq!(store.thread_workspace_binding(child).unwrap(), binding);
        // The ordinary failure path is also used after task panic and unknown
        // contact. Terminalization alone never retires the borrow.
        let mut guard = ExecutionGuard::new(state.clone());
        guard.track_thread(child);
        guard.track_launch_owner(claim.claimed_by.clone());
        guard.fail_thread("task_panic");
        guard.cleanup();
        assert_eq!(store.thread_workspace_binding(child).unwrap(), binding);
        store
            .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
            .unwrap();
        assert!(
            store
                .settle_thread_workspace_owned(child, binding.as_ref().unwrap())
                .is_err()
        );
        // Quarantine belongs to the workspace owner, not an individual
        // borrower. Its abandoned view must retain the unresolved child.
        let parent = store.get_launch_claim("T-runtime-parent").unwrap().unwrap();
        store
            .release_active_thread_launch_claim(
                "T-runtime-parent",
                &parent.claim_id,
                &parent.claimed_by,
            )
            .unwrap();
        assert!(
            store
                .has_retained_workspace_quarantine("T-runtime-parent")
                .unwrap()
        );
    }

    #[test]
    fn product_build_key_commits_original_source_parameters_and_partition() {
        let producer = ryeos_state::external_content::products::ProductProducerAdmission {
            canonical_ref: "graph:test/producer".into(),
            effective_definition_digest: "a".repeat(64),
            exact_program_hash: "b".repeat(64),
            producer_project_snapshot_hash: "c".repeat(64),
            launch_authority_digest: "d".repeat(64),
            admitted_parameters_digest: "e".repeat(64),
        };
        let partition = "f".repeat(64);
        let replay_authority = "0".repeat(64);
        let original =
            product_build_subject_digest(&producer, &partition, &replay_authority).unwrap();
        assert_eq!(
            original,
            product_build_subject_digest(&producer.clone(), &partition, &replay_authority).unwrap()
        );
        let mut moved = producer.clone();
        moved.producer_project_snapshot_hash = "1".repeat(64);
        assert_ne!(
            original,
            product_build_subject_digest(&moved, &partition, &replay_authority).unwrap()
        );
        moved = producer.clone();
        moved.admitted_parameters_digest = "2".repeat(64);
        assert_ne!(
            original,
            product_build_subject_digest(&moved, &partition, &replay_authority).unwrap()
        );
        assert_ne!(
            original,
            product_build_subject_digest(&producer, &"3".repeat(64), &replay_authority).unwrap()
        );
        moved = producer.clone();
        moved.launch_authority_digest = "4".repeat(64);
        assert_eq!(
            original,
            product_build_subject_digest(&moved, &partition, &replay_authority).unwrap()
        );
        assert_ne!(
            original,
            product_build_subject_digest(&producer, &partition, &"5".repeat(64)).unwrap()
        );
        assert!(product_build_subject_digest(&producer, "pending", &replay_authority).is_err());
        assert!(product_build_subject_digest(&producer, &partition, "pending").is_err());
    }

    fn retained_output_workspace(state: WorkspaceState) -> ryeos_app::runtime_db::WorkspaceRecord {
        ryeos_app::runtime_db::WorkspaceRecord {
            workspace_id: "workspace-test".into(),
            thread_id: Some("T-test".into()),
            launch_owner: Some("previous-owner".into()),
            backend_id: Some("native".into()),
            backend_version: Some("1".into()),
            pinned_root_identities: Some("retained-root-proof".into()),
            mount_identity: Some("previous-view".into()),
            base_snapshot: "a".repeat(64),
            workspace_output_partition_identity: Some("b".repeat(64)),
            base_output_capture_hash: Some("c".repeat(64)),
            frozen_snapshot_hash: (state == WorkspaceState::Freezing).then(|| "d".repeat(64)),
            frozen_output_capture_hash: (state == WorkspaceState::Freezing).then(|| "e".repeat(64)),
            root_path: "/private/workspace-test".into(),
            state,
            process_identity: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn cold_workspace_reconstruction_preserves_ready_active_and_frozen_contents() {
        for state in [
            WorkspaceState::Ready,
            WorkspaceState::Active,
            WorkspaceState::Freezing,
        ] {
            let previous = retained_output_workspace(state);
            let mut constructing = previous.clone();
            constructing.state = WorkspaceState::Constructing;
            constructing.launch_owner = Some("recovery-owner".into());
            constructing.mount_identity = None;
            let input = WorkspaceConstructionInput::RetainedBacking(&previous);
            // The exact recovery branch must skip baseline content verification
            // and restoration regardless of whether writes are present. It is
            // selected by the admitted journal, never by disk appearance.
            assert!(
                input
                    .preserves_retained_contents(&constructing, "T-test")
                    .unwrap()
            );
            assert!(
                !WorkspaceConstructionInput::PristineSource
                    .preserves_retained_contents(&constructing, "T-test")
                    .unwrap()
            );
            let mut changed = constructing.clone();
            changed.base_output_capture_hash = Some("f".repeat(64));
            assert!(
                input
                    .preserves_retained_contents(&changed, "T-test")
                    .is_err()
            );
            changed = constructing.clone();
            changed.pinned_root_identities = Some("different-root".into());
            assert!(
                input
                    .preserves_retained_contents(&changed, "T-test")
                    .is_err()
            );
            assert!(
                input
                    .preserves_retained_contents(&constructing, "T-other")
                    .is_err()
            );
            if state == WorkspaceState::Freezing {
                changed = constructing.clone();
                changed.frozen_output_capture_hash = Some("f".repeat(64));
                assert!(
                    input
                        .preserves_retained_contents(&changed, "T-test")
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn retained_reconstruction_requires_a_previous_admitted_workspace_view() {
        let mut previous = retained_output_workspace(WorkspaceState::Constructing);
        let mut constructing = previous.clone();
        assert!(
            WorkspaceConstructionInput::RetainedBacking(&previous)
                .preserves_retained_contents(&constructing, "T-test")
                .is_err()
        );
        previous.state = WorkspaceState::Active;
        constructing.state = WorkspaceState::Constructing;
        previous.backend_id = None;
        assert!(
            WorkspaceConstructionInput::RetainedBacking(&previous)
                .preserves_retained_contents(&constructing, "T-test")
                .is_err()
        );
    }

    #[test]
    fn ready_workspace_requires_exact_output_coordinates_in_both_directions() {
        let partition = "a".repeat(64);
        let capture = "b".repeat(64);

        validate_retained_workspace_output_coordinates(
            WorkspaceState::Constructing,
            None,
            None,
            Some(&partition),
            Some(&capture),
        )
        .unwrap();
        validate_retained_workspace_output_coordinates(
            WorkspaceState::Ready,
            Some(&partition),
            Some(&capture),
            Some(&partition),
            Some(&capture),
        )
        .unwrap();
        assert!(
            validate_retained_workspace_output_coordinates(
                WorkspaceState::Ready,
                None,
                None,
                Some(&partition),
                Some(&capture),
            )
            .is_err()
        );
        assert!(
            validate_retained_workspace_output_coordinates(
                WorkspaceState::Ready,
                Some(&partition),
                Some(&capture),
                None,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_workspace_close_requires_terminal_generation_coherence() {
        use ryeos_state::objects::PinnedTerminalPublication;

        assert_eq!(
            terminal_workspace_close_source(
                &PinnedTerminalPublication::Discard,
                WorkspaceState::Ready,
                None,
                None,
            )
            .unwrap(),
            WorkspaceState::Ready
        );
        assert_eq!(
            terminal_workspace_close_source(
                &PinnedTerminalPublication::Discard,
                WorkspaceState::Active,
                None,
                None,
            )
            .unwrap(),
            WorkspaceState::Active
        );
        assert_eq!(
            terminal_workspace_close_source(
                &PinnedTerminalPublication::Discard,
                WorkspaceState::Freezing,
                Some("ignored"),
                None,
            )
            .unwrap(),
            WorkspaceState::Freezing
        );

        let retained = PinnedTerminalPublication::RetainResult;
        assert!(
            terminal_workspace_close_source(&retained, WorkspaceState::Active, None, None,)
                .unwrap_err()
                .to_string()
                .contains("not frozen")
        );
        assert!(
            terminal_workspace_close_source(
                &retained,
                WorkspaceState::Freezing,
                Some("frozen"),
                Some("other"),
            )
            .unwrap_err()
            .to_string()
            .contains("disagrees")
        );
        assert_eq!(
            terminal_workspace_close_source(
                &retained,
                WorkspaceState::Freezing,
                Some("frozen"),
                Some("frozen"),
            )
            .unwrap(),
            WorkspaceState::Freezing
        );
    }

    #[test]
    fn restartable_node_policy_direct_execution_is_refused_before_metadata_birth() {
        let error = ensure_restart_eligible_artifact(
            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            "tool:test/node-policy",
            Some(&ryeos_state::objects::DirectExecutableIdentity::NodePolicy),
        )
        .unwrap_err();
        let eligibility = error
            .downcast_ref::<ExecutionNotRestartEligible>()
            .expect("typed restart eligibility refusal");
        assert_eq!(eligibility.item_ref, "tool:test/node-policy");
    }

    #[test]
    fn verified_and_request_scoped_direct_execution_remain_eligible() {
        let verified = ryeos_state::objects::DirectExecutableIdentity::CapturedContent {
            content_hash: "a".repeat(64),
        };
        ensure_restart_eligible_artifact(
            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            "tool:test/verified",
            Some(&verified),
        )
        .unwrap();
        ensure_restart_eligible_artifact(
            ryeos_state::objects::ExecutionLifecycleAuthority::REQUEST_SCOPED,
            "tool:test/node-policy",
            Some(&ryeos_state::objects::DirectExecutableIdentity::NodePolicy),
        )
        .unwrap();
    }

    fn resume_record(
        project_context: ProjectContext,
        pushed: Option<OriginalPushedHeadRef>,
    ) -> ResumeContext {
        let stable_project_identity = match &project_context {
            ProjectContext::LocalPath { path } => Some(
                ryeos_app::launch_metadata::StableProjectIdentity::from_path(path, "site:test")
                    .unwrap(),
            ),
            _ => None,
        };
        let project_authority = match (&project_context, pushed.as_ref()) {
            (ProjectContext::None, None) => {
                ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS
            }
            (ProjectContext::LocalPath { path: root }, None) => {
                ryeos_state::objects::ExecutionProjectAuthority::live(
                    root.clone(),
                    format!("local:{}", root.display()),
                    ryeos_state::objects::LiveProjectAccess::ReadWrite,
                    ryeos_state::objects::LiveFilesystemConfinement::standard_fixed_parents(),
                    ryeos_state::objects::EnvironmentAuthority::None,
                    Vec::new(),
                )
                .unwrap()
            }
            (ProjectContext::LocalPath { path }, Some(pushed)) => {
                ryeos_state::objects::ExecutionProjectAuthority::pinned(
                    format!("local:{}", path.display()),
                    Some(path.clone()),
                    pushed.snapshot_hash.clone(),
                    ryeos_state::objects::PinnedProjectRealization::Cow {
                        terminal_publication:
                            ryeos_state::objects::PinnedTerminalPublication::Discard,
                    },
                    ryeos_state::objects::EnvironmentAuthority::None,
                    Vec::new(),
                )
                .unwrap()
            }
            (ProjectContext::SnapshotHash { hash }, None) => {
                ryeos_state::objects::ExecutionProjectAuthority::pinned(
                    format!("snapshot:{hash}"),
                    None,
                    hash.clone(),
                    ryeos_state::objects::PinnedProjectRealization::Cow {
                        terminal_publication:
                            ryeos_state::objects::PinnedTerminalPublication::Discard,
                    },
                    ryeos_state::objects::EnvironmentAuthority::None,
                    Vec::new(),
                )
                .unwrap()
            }
            (context, pushed) => panic!(
                "resume fixture requires an explicit authority for context {context:?} and pushed head {pushed:?}"
            ),
        };
        ResumeContext {
            kind: "graph".into(),
            item_ref: "graph:test/item".into(),
            ref_bindings: std::collections::BTreeMap::new(),
            product_selections: Vec::new(),
            launch_mode: "detached".into(),
            parameters: json!({}),
            project_context,
            project_authority,
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity,
            local_overlay_root: None,
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
            scheduled_fire: None,
            effective_caps: Vec::new(),
            parent_delegation_caps: None,
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
        let stale_checkout = tempfile::tempdir().unwrap();
        let stale_checkout_path = stale_checkout.path().to_path_buf();
        let snapshot_hash = "c".repeat(64);
        let resume = resume_record(
            ProjectContext::LocalPath {
                path: stale_checkout_path,
            },
            Some(OriginalPushedHeadRef {
                snapshot_hash: snapshot_hash.clone(),
                original_project_path: PathBuf::from("/laptop/proj"),
            }),
        );
        drop(stale_checkout);
        match decide_resume_provenance(&resume) {
            ResumeProvenanceDecision::PinnedPushedHead(pinned) => {
                assert_eq!(pinned.snapshot_hash, snapshot_hash);
                assert_eq!(pinned.original_project_path, PathBuf::from("/laptop/proj"));
            }
            other => panic!("expected PinnedPushedHead, got {other:?}"),
        }
    }

    #[test]
    fn live_localpath_record_selects_live_authority_without_snapshot_hints() {
        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().to_path_buf();
        let resume = resume_record(
            ProjectContext::LocalPath {
                path: project_path.clone(),
            },
            None,
        );
        match decide_resume_provenance(&resume) {
            ResumeProvenanceDecision::LiveProject(path) => {
                assert_eq!(path, project_path);
            }
            other => panic!("expected LiveProject, got {other:?}"),
        }
    }

    #[test]
    fn pinned_localpath_record_selects_a_fresh_snapshot_checkout() {
        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().to_path_buf();
        let mut resume = resume_record(
            ProjectContext::LocalPath {
                path: project_path.clone(),
            },
            None,
        );
        resume.project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            format!("local:{}", project_path.display()),
            Some(project_path.clone()),
            "a".repeat(64),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        resume.original_snapshot_hash = Some("non_authoritative_hint_must_not_win".into());

        match decide_resume_provenance(&resume) {
            ResumeProvenanceDecision::PinnedLocalSnapshot {
                snapshot_hash,
                original_path,
            } => {
                assert_eq!(snapshot_hash, "a".repeat(64));
                assert_eq!(original_path, project_path);
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
            let mut resume = resume_record(ProjectContext::None, None);
            resume.project_context = pc.clone();
            resume.project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
                "snapshot:unattributed".to_string(),
                None,
                "b".repeat(64),
                ryeos_state::objects::PinnedProjectRealization::Cow {
                    terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
                },
                ryeos_state::objects::EnvironmentAuthority::None,
                Vec::new(),
            )
            .unwrap();
            match decide_resume_provenance(&resume) {
                ResumeProvenanceDecision::MissingPushedHeadRef(_) => {}
                other => panic!("expected MissingPushedHeadRef for {pc:?}, got {other:?}"),
            }
        }
    }
}
