//! Shared ownership for subprocesses that can call back into the daemon.
//!
//! Attachment-required subprocesses are held before user/runtime code can run.
//! The local owner captures and persists the exact held identity, releases the
//! process, and compare-clears that same identity after its owned wait.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;

use ryeos_app::state::AppState;
use ryeos_app::state_store::{StopIntent, is_terminal_status};
use ryeos_app::temp_dir_guard::TempDirGuard;
use ryeos_app::thread_lifecycle::{ThreadAttachProcessParams, ThreadFinalizeParams};

/// RAII ownership for any waiting lifecycle row whose async request may be
/// cancelled after creation. Explicit terminal/preserve paths disarm it; an
/// unplanned future drop delegates to the same exact-identity tree stop used by
/// the unified runner.
pub(crate) struct LifecycleOwnerGuard {
    state: AppState,
    thread_id: String,
    disarmed: bool,
    settled_owned_wait: bool,
    callback_token: Option<String>,
    thread_auth_token: Option<String>,
    workspace_lifeline: Option<Arc<TempDirGuard>>,
}

impl LifecycleOwnerGuard {
    pub(crate) fn new(state: &AppState, thread_id: &str) -> Self {
        Self {
            state: state.clone(),
            thread_id: thread_id.to_string(),
            disarmed: false,
            settled_owned_wait: false,
            callback_token: None,
            thread_auth_token: None,
            workspace_lifeline: None,
        }
    }

    pub(crate) fn track_callback_token(&mut self, token: String) {
        self.callback_token = Some(token);
    }

    pub(crate) fn track_thread_auth_token(&mut self, token: String) {
        self.thread_auth_token = Some(token);
    }

    /// Retain the original owned workspace before construction/contact. The
    /// caller selects only its owned root, never a borrowed child lifeline.
    pub(crate) fn track_owned_workspace_lifeline(
        &mut self,
        workspace: Arc<TempDirGuard>,
    ) -> Result<()> {
        if let Some(existing) = self.workspace_lifeline.as_ref() {
            if !Arc::ptr_eq(existing, &workspace) {
                anyhow::bail!("lifecycle owner cannot replace its original workspace lifeline");
            }
        } else {
            self.workspace_lifeline = Some(workspace);
        }
        Ok(())
    }

    /// Record a `SpawnedRuntime::wait` result carrying its explicit settled
    /// attached-wait proof. That proof is emitted only after the exact process
    /// group was reaped and the attached workspace membership settled. Fallible
    /// terminal capture still needs this guard's workspace cleanup owner, but
    /// no later error may be reclassified as an owner-drop kill.
    pub(crate) fn record_settled_owned_wait(&mut self) {
        self.revoke_tokens();
        self.settled_owned_wait = true;
    }

    /// A wait error does not prove that process or workspace descendants are
    /// quiescent. Revoke callback authority, but retain the ordinary owner-drop
    /// stop order.
    pub(crate) fn revoke_tokens_after_unsettled_wait(&mut self) {
        self.revoke_tokens();
    }

    pub(crate) fn has_settled_owned_wait(&self) -> bool {
        self.settled_owned_wait
    }

    pub(crate) fn disarm(&mut self) {
        self.revoke_tokens();
        self.disarmed = true;
    }

    fn revoke_tokens(&mut self) {
        if let Some(token) = self.callback_token.take() {
            self.state.callback_tokens.invalidate(&token);
            self.state
                .callback_tokens
                .invalidate_for_thread(&self.thread_id);
        }
        if let Some(token) = self.thread_auth_token.take() {
            self.state.thread_auth.invalidate(&token);
            self.state
                .thread_auth
                .invalidate_for_thread(&self.thread_id);
        }
    }
}

impl Drop for LifecycleOwnerGuard {
    fn drop(&mut self) {
        self.revoke_tokens();
        if self.disarmed {
            return;
        }
        if self
            .state
            .state_store
            .get_thread(&self.thread_id)
            .is_ok_and(|thread| thread.is_some_and(|thread| is_terminal_status(&thread.status)))
        {
            // A committed terminalization gate cannot be acquired again.
            // Finish original-owner closure directly; it independently checks
            // every member and worker's death, so terminal history is not
            // substituted for cleanup proof.
            if let Err(error) = super::runner::close_aborted_owned_workspace(
                &self.state,
                self.workspace_lifeline.as_ref(),
                &self.thread_id,
            ) {
                tracing::error!(thread_id = %self.thread_id, %error,
                    "terminal lifecycle owner retained unresolved original workspace");
            }
            return;
        }
        match super::runner::stop_owner_dropped_execution_tree(&self.state, &self.thread_id) {
            Ok(super::runner::OwnerDropStopOutcome::Settled) => {
                if let Some(workspace) = self.workspace_lifeline.as_ref()
                    && let Err(error) = super::runner::close_aborted_owned_workspace(
                        &self.state,
                        Some(workspace),
                        &self.thread_id,
                    )
                {
                    tracing::error!(
                        thread_id = %self.thread_id,
                        error = %error,
                        "stopped lifecycle owner retained an unresolved workspace journal"
                    );
                }
            }
            Ok(super::runner::OwnerDropStopOutcome::PreservedForShutdown) => tracing::info!(
                thread_id = %self.thread_id,
                "waiting lifecycle owner preserved row for shutdown coordinator"
            ),
            Err(error) => tracing::error!(
                thread_id = %self.thread_id,
                error = %error,
                "waiting lifecycle owner could not fully stop and settle execution tree"
            ),
        }
    }
}

/// Exact attached-process owner shared by callback and managed runtime waits.
/// Drop is deliberately non-settling: it also runs after failed cleanup.
pub(crate) struct AttachedProcessGuard {
    state: AppState,
    thread_id: String,
    launch_owner: String,
    identity: ryeos_app::process::ExecutionProcessIdentity,
    workspace_binding: Option<ryeos_app::runtime_db::RuntimeWorkspaceBinding>,
    settled: bool,
}

impl AttachedProcessGuard {
    pub(crate) fn new(
        state: &AppState,
        thread_id: &str,
        launch_owner: &str,
        identity: ryeos_app::process::ExecutionProcessIdentity,
    ) -> Result<Self> {
        let workspace_binding = state.state_store.thread_workspace_binding(thread_id)?;
        Ok(Self {
            state: state.clone(),
            thread_id: thread_id.to_owned(),
            launch_owner: launch_owner.to_owned(),
            identity,
            workspace_binding,
            settled: false,
        })
    }

    /// Call only after owned wait/reap. The existing group-absence check also
    /// refuses wait/cleanup errors whose SubprocessResult carries no reap proof.
    pub(crate) fn settle_after_reap(&mut self) -> Result<()> {
        if self.settled {
            return Ok(());
        }
        ryeos_app::process::assert_reaped_process_group_absent(&self.identity)?;
        let current_binding = self
            .state
            .state_store
            .thread_workspace_binding(&self.thread_id)?;
        if let Some(original) = self.workspace_binding.as_ref() {
            if current_binding.as_ref() != Some(original) {
                anyhow::bail!("reaped subprocess workspace binding changed before settlement");
            }
        } else if let Some(binding) = current_binding.as_ref() {
            // A projectless controller may create its first workspace through
            // its authenticated callback. Only that same exact launch owner
            // can add this membership while the controller is running.
            let owner =
                lillux::canonical_json(&serde_json::to_value(&binding.borrower_launch_owner)?)?;
            if owner != self.launch_owner {
                anyhow::bail!("reaped subprocess acquired a workspace under another launch owner");
            }
        }
        let cleared = if let Some(binding) = current_binding.as_ref() {
            self.state
                .state_store
                .settle_reaped_thread_workspace_owned(&self.thread_id, binding, &self.identity)?
        } else {
            self.state
                .state_store
                .clear_thread_process_if_matches_owned(
                    &self.thread_id,
                    &self.identity,
                    &self.launch_owner,
                )?
        };
        if !cleared {
            anyhow::bail!(
                "reaped subprocess retains changed authority or unsettled workspace descendants"
            );
        }
        self.settled = true;
        Ok(())
    }
}

impl Drop for AttachedProcessGuard {
    fn drop(&mut self) {
        if !self.settled {
            tracing::warn!(
                thread_id = %self.thread_id,
                "attached-process owner dropped without explicit reap settlement; retaining recovery authority"
            );
        }
    }
}

/// Settle a durable stop tombstone using its monotonic effective intent.
///
/// Returns `true` when a stop was present (including a row another racing path
/// already finalized). Attach-failure paths call this before classifying the
/// refusal as a launch failure, so Cancel/Kill cannot be overwritten by
/// `attach_failed` or `pre_runtime_failure`.
pub(crate) fn finalize_requested_stop_if_present(
    state: &AppState,
    thread_id: &str,
) -> Result<bool> {
    let Some(thread) = state.threads.get_thread(thread_id)? else {
        anyhow::bail!("thread disappeared while settling durable stop: {thread_id}");
    };
    let Some(intent) = thread.runtime.stop_intent else {
        return Ok(false);
    };
    if is_terminal_status(&thread.status) {
        return Ok(true);
    }

    let (status, reason) = match intent {
        StopIntent::Cancel => ("cancelled", "cancelled_before_process_attachment"),
        StopIntent::Kill => ("killed", "killed_before_process_attachment"),
    };
    match state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: status.to_string(),
        outcome_code: Some(status.to_string()),
        result: None,
        error: Some(json!({ "reason": reason })),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    }) {
        Ok(_) => Ok(true),
        Err(error) => {
            let terminal = state
                .threads
                .get_thread(thread_id)?
                .is_some_and(|current| is_terminal_status(&current.status));
            if terminal {
                Ok(true)
            } else {
                Err(error).context("finalize durable stop after attachment refusal")
            }
        }
    }
}

/// Spawn, durably attach, wait, and compare-clear one callback-capable
/// subprocess inside a single blocking owner.
pub(crate) fn run_lillux_attached(
    state: &AppState,
    thread_id: &str,
    launch_owner: &str,
    request: ryeos_engine::isolation::IsolationRequestAwaitingAttachment,
    workspace_lifeline: Option<Arc<TempDirGuard>>,
) -> Result<lillux::SubprocessResult> {
    let spawned = match request.spawn() {
        Ok(spawned) => spawned,
        Err(result) => return Ok(result),
    };
    run_spawned_lillux_attached(state, thread_id, launch_owner, spawned, workspace_lifeline)
}

pub(crate) fn run_spawned_lillux_attached(
    state: &AppState,
    thread_id: &str,
    launch_owner: &str,
    spawned: lillux::ProcessAwaitingAttachment,
    workspace_lifeline: Option<Arc<TempDirGuard>>,
) -> Result<lillux::SubprocessResult> {
    let _workspace_lifeline = workspace_lifeline;
    #[cfg(target_os = "linux")]
    let identity_result = ryeos_app::process::capture_execution_process_identity_from_pidfd(
        spawned.pid() as i64,
        Some(spawned.pgid()),
        spawned.pidfd(),
    )
    .context("capture held callback subprocess identity from Lillux pidfd");
    #[cfg(not(target_os = "linux"))]
    let identity_result = ryeos_app::process::capture_execution_process_identity(
        spawned.pid() as i64,
        Some(spawned.pgid()),
    )
    .context("capture held callback subprocess identity");
    let identity = match identity_result {
        Ok(identity) => identity,
        Err(error) => {
            let cleanup = spawned.abort_and_reap().err();
            return Err(match cleanup {
                Some(cleanup) => {
                    error.context(format!("pending-process cleanup failed: {cleanup}"))
                }
                None => error,
            });
        }
    };

    if let Err(error) = state.threads.attach_new_process_owned(
        &ThreadAttachProcessParams {
            thread_id: thread_id.to_string(),
            pid: spawned.pid() as i64,
            pgid: spawned.pgid(),
            process_identity: Some(identity.clone()),
            metadata: None,
            launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata::default(),
        },
        launch_owner,
    ) {
        let cleanup = spawned.abort_and_reap().err();
        if let Some(cleanup) = cleanup {
            return Err(error.context(format!(
                "pending-process cleanup failed; retaining authority: {cleanup}"
            )));
        }
        let stop_settlement = finalize_requested_stop_if_present(state, thread_id);
        let error = match stop_settlement {
            Ok(true) => error.context(
                "callback subprocess attachment refused after durable stop request",
            ),
            Ok(false) => error.context("attach held callback subprocess identity"),
            Err(stop_error) => error.context(format!(
                "attach held callback subprocess identity; stop settlement also failed: {stop_error:#}"
            )),
        };
        return Err(error);
    }
    let mut attachment =
        AttachedProcessGuard::new(state, thread_id, launch_owner, identity.clone())?;
    if let Err(error) =
        state
            .threads
            .authorize_process_release_owned(thread_id, &identity, launch_owner)
    {
        let cleanup = spawned
            .abort_and_reap()
            .map_err(anyhow::Error::from)
            .and_then(|_| attachment.settle_after_reap())
            .err();
        if let Some(cleanup) = cleanup {
            return Err(error.context(format!(
                "pending-process cleanup failed; retaining authority: {cleanup}"
            )));
        }
        let stop_settlement = finalize_requested_stop_if_present(state, thread_id);
        let error = match stop_settlement {
            Ok(true) => {
                anyhow::anyhow!("callback subprocess stopped before attachment release: {error}")
            }
            Ok(false) => {
                error.context("authorize callback subprocess release after durable attachment")
            }
            Err(stop_error) => error.context(format!(
                "authorize callback subprocess release after durable attachment; stop settlement also failed: {stop_error:#}"
            )),
        };
        return Err(error);
    }
    let spawned = match spawned.release_after_attachment() {
        Ok(spawned) => spawned,
        Err(error) => {
            // Lillux's release error retains a completed cleanup contract; we
            // additionally verify group absence before clearing durable state.
            let settlement = attachment.settle_after_reap();
            return Err(match settlement {
                Ok(()) => anyhow::Error::new(error)
                    .context("release callback subprocess after durable attachment"),
                Err(settlement) => anyhow::Error::new(error).context(format!(
                    "release failed; exact attachment retained: {settlement:#}"
                )),
            });
        }
    };
    let result = spawned.wait();
    attachment.settle_after_reap()?;
    Ok(result)
}
