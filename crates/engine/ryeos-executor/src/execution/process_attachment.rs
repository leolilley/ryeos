//! Shared ownership for subprocesses that can call back into the daemon.
//!
//! A callback runtime may self-attach over UDS before the in-process launcher
//! records the same identity. Attachment is immutable and exact repeats are
//! idempotent, so both paths converge on one durable identity. The local owner
//! always compare-clears that exact identity after its owned wait.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;

use ryeos_app::state::AppState;
use ryeos_app::state_store::{is_terminal_status, StopIntent};
use ryeos_app::temp_dir_guard::TempDirGuard;
use ryeos_app::thread_lifecycle::{ThreadAttachProcessParams, ThreadFinalizeParams};

/// RAII ownership for any inline lifecycle row whose async request may be
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
                "inline lifecycle owner preserved row for shutdown coordinator"
            ),
            Err(error) => tracing::error!(
                thread_id = %self.thread_id,
                error = %error,
                "inline lifecycle owner could not fully stop and settle execution tree"
            ),
        }
    }
}

struct AttachedProcessGuard<'a> {
    state: &'a AppState,
    thread_id: &'a str,
    identity: ryeos_app::process::ExecutionProcessIdentity,
}

impl Drop for AttachedProcessGuard<'_> {
    fn drop(&mut self) {
        match self
            .state
            .state_store
            .clear_thread_process_if_matches(self.thread_id, &self.identity)
        {
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

/// Capture the live owned identity, or adopt the exact identity a fast runtime
/// already established through its authenticated UDS self-attach.
///
/// The adoption path matters when the runtime exits before the in-process owner
/// reaches procfs capture: the UDS boundary pinned the live peer earlier, and
/// the owner still retains the Lillux handle needed to reap it.
pub(crate) fn capture_or_adopt_owned_identity(
    state: &AppState,
    thread_id: &str,
    pid: i64,
    pgid: i64,
) -> Result<ryeos_app::process::ExecutionProcessIdentity> {
    match ryeos_app::process::capture_execution_process_identity(pid, Some(pgid)) {
        Ok(identity) => Ok(identity),
        Err(
            capture_error @ ryeos_app::process::ExecutionProcessIdentityCaptureError::TargetAlreadyDeadOrStale,
        ) => {
            let existing = state.threads.get_thread(thread_id)?.and_then(|thread| {
                (thread.runtime.pid == Some(pid) && thread.runtime.pgid == Some(pgid))
                    .then_some(thread.runtime.process_identity)
                    .flatten()
            });
            existing.ok_or_else(|| {
                capture_error.context(
                    "capture owned process identity and no exact authenticated self-attach exists",
                )
            })
        }
        Err(error) => Err(error).context("capture owned process identity"),
    }
}

/// Spawn, durably attach, wait, and compare-clear one callback-capable
/// subprocess inside a single blocking owner.
pub(crate) fn run_lillux_attached(
    state: &AppState,
    thread_id: &str,
    request: lillux::SubprocessRequest,
    workspace_lifeline: Option<Arc<TempDirGuard>>,
) -> Result<lillux::SubprocessResult> {
    let _workspace_lifeline = workspace_lifeline;
    let spawned = match lillux::spawn(request) {
        Ok(spawned) => spawned,
        Err(result) => return Ok(result),
    };
    let identity =
        match capture_or_adopt_owned_identity(state, thread_id, spawned.pid as i64, spawned.pgid) {
            Ok(identity) => identity,
            Err(error) => {
                spawned.abort();
                return Err(error).context("capture callback subprocess identity");
            }
        };
    // Install compare-clear ownership before the in-process attach. A fast
    // runtime may already have self-attached over UDS; any later refusal must
    // still clear that exact authenticated identity after abort/reap.
    let _attachment = AttachedProcessGuard {
        state,
        thread_id,
        identity: identity.clone(),
    };
    if let Err(error) = state.threads.attach_process(&ThreadAttachProcessParams {
        thread_id: thread_id.to_string(),
        pid: spawned.pid as i64,
        pgid: spawned.pgid,
        process_identity: Some(identity.clone()),
        metadata: None,
        launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata::default(),
    }) {
        spawned.abort();
        if finalize_requested_stop_if_present(state, thread_id)? {
            anyhow::bail!("callback subprocess attachment refused after durable stop request");
        }
        return Err(error).context("attach callback subprocess identity");
    }
    Ok(spawned.wait())
}
