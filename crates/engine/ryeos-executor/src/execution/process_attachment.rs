//! Shared ownership for subprocesses that can call back into the daemon.
//!
//! Attachment-required subprocesses are held before user/runtime code can run.
//! The local owner captures and persists the exact held identity, releases the
//! process, and compare-clears that same identity after its owned wait.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;

use ryeos_app::state::AppState;
use ryeos_app::state_store::{is_terminal_status, StopIntent};
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
    callback_token: Option<String>,
    thread_auth_token: Option<String>,
}

impl LifecycleOwnerGuard {
    pub(crate) fn new(state: &AppState, thread_id: &str) -> Self {
        Self {
            state: state.clone(),
            thread_id: thread_id.to_string(),
            disarmed: false,
            callback_token: None,
            thread_auth_token: None,
        }
    }

    pub(crate) fn track_callback_token(&mut self, token: String) {
        self.callback_token = Some(token);
    }

    pub(crate) fn track_thread_auth_token(&mut self, token: String) {
        self.thread_auth_token = Some(token);
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
        match super::runner::stop_owner_dropped_execution_tree(&self.state, &self.thread_id) {
            Ok(super::runner::OwnerDropStopOutcome::Settled) => {}
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

struct AttachedProcessGuard<'a> {
    state: &'a AppState,
    thread_id: &'a str,
    launch_owner: &'a str,
    identity: ryeos_app::process::ExecutionProcessIdentity,
}

impl Drop for AttachedProcessGuard<'_> {
    fn drop(&mut self) {
        match self
            .state
            .state_store
            .clear_thread_process_if_matches_owned(
                self.thread_id,
                &self.identity,
                self.launch_owner,
            ) {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                thread_id = self.thread_id,
                "owned subprocess identity changed before compare-and-clear"
            ),
            Err(error) => tracing::error!(
                thread_id = self.thread_id,
                error = %error,
                "failed to clear owned subprocess identity after wait"
            ),
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
        return match cleanup {
            Some(cleanup) => {
                Err(error.context(format!("pending-process cleanup failed: {cleanup}")))
            }
            None => Err(error),
        };
    }
    let _attachment = AttachedProcessGuard {
        state,
        thread_id,
        launch_owner,
        identity: identity.clone(),
    };
    if let Err(error) =
        state
            .threads
            .authorize_process_release_owned(thread_id, &identity, launch_owner)
    {
        let cleanup = spawned.abort_and_reap().err();
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
        return match cleanup {
            Some(cleanup) => {
                Err(error.context(format!("pending-process cleanup failed: {cleanup}")))
            }
            None => Err(error),
        };
    }
    let spawned = spawned
        .release_after_attachment()
        .context("release callback subprocess after durable attachment")?;
    Ok(spawned.wait())
}
