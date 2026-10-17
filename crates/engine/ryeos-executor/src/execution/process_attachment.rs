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
        let proposed =
            ryeos_app::runtime_db::ProcessResourceCleanupEvidence::capture(&self.identity)?;
        let evidence = self.state.state_store.prove_thread_resource_owner_cleanup(
            &self.thread_id,
            &self.identity,
            &proposed,
        )?;
        ryeos_app::execution_resources::settle_process_resource_operations_after_cleanup(
            &self.state,
            &self.identity,
            &evidence,
        )?;
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

/// Class B composition. Every failure after process contact is a typed
/// [`LaunchFailure`]; callers must not finalize one whose cleanup is unsettled.
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
            let (pid, pgid) = (spawned.pid(), spawned.pgid());
            return Err(match spawned.abort_and_reap() {
                Ok(_) => error,
                Err(cleanup) => anyhow::Error::new(LaunchFailure::unsettled_uncaptured(
                    LaunchOwnerCoordinate::thread(thread_id, launch_owner),
                    pid,
                    pgid,
                    error.context(format!("pending-process cleanup failed: {cleanup}")),
                )),
            });
        }
    };
    let held = HeldLaunch::new(
        state,
        LaunchOwnerCoordinate::thread(thread_id, launch_owner),
        identity,
        spawned,
    );
    let launch = held
        .attach(AttachOptions::default())
        .and_then(AttachedLaunch::authorize_release)
        .and_then(AuthorizedLaunch::release)
        .map_err(anyhow::Error::new)?;
    let ReleasedLaunch {
        running,
        mut settlement,
    } = launch;
    let result = running.wait();
    settlement
        .settle_after_reap()
        .map_err(|error| anyhow::Error::new(settlement.into_unsettled_failure(error)))?;
    Ok(result)
}

// ── Admitted execution owner ─────────────────────────────────────────────

/// Durable coordinate that owns one launch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaunchOwnerCoordinate {
    Thread {
        thread_id: String,
        launch_owner: String,
    },
}

impl LaunchOwnerCoordinate {
    pub(crate) fn thread(thread_id: &str, launch_owner: &str) -> Self {
        Self::Thread {
            thread_id: thread_id.to_owned(),
            launch_owner: launch_owner.to_owned(),
        }
    }

    pub(crate) fn thread_id(&self) -> &str {
        match self {
            Self::Thread { thread_id, .. } => thread_id,
        }
    }

    pub(crate) fn launch_owner(&self) -> &str {
        match self {
            Self::Thread { launch_owner, .. } => launch_owner,
        }
    }
}

/// Keep the caller's durable attachment untouched unless the consuming
/// release owner proves both the child/wrapper and selected scope cleanup.
/// PID/group absence and diagnostic text do not establish that proof.
pub(crate) trait ReleaseTestimony {
    fn cleanup_is_settled(&self) -> bool;
}

impl ReleaseTestimony for lillux::exec::AttachmentReleaseError {
    fn cleanup_is_settled(&self) -> bool {
        lillux::exec::AttachmentReleaseError::cleanup_is_settled(self)
    }
}

impl ReleaseTestimony for ryeos_app::thread_lifecycle::ItemReleaseError {
    fn cleanup_is_settled(&self) -> bool {
        ryeos_app::thread_lifecycle::ItemReleaseError::cleanup_is_settled(self)
    }
}

/// One held (not yet released) process class.
pub(crate) trait HeldProcess: Sized {
    type Running;
    type ReleaseError: ReleaseTestimony + std::error::Error + Send + Sync + 'static;
    fn pid(&self) -> u32;
    fn pgid(&self) -> i64;
    /// Abort and reap before durable attachment, retiring class-owned
    /// reservations for this exact identity.
    fn abort_unattached(
        self,
        state: &AppState,
        identity: &ryeos_app::process::ExecutionProcessIdentity,
    ) -> Result<()>;
    /// Abort and reap after durable attachment. Attachment settlement follows.
    fn abort_attached(self) -> Result<()>;
    fn release(
        self,
        identity: &ryeos_app::process::ExecutionProcessIdentity,
    ) -> std::result::Result<Self::Running, Self::ReleaseError>;
}

impl HeldProcess for lillux::ProcessAwaitingAttachment {
    type Running = lillux::RunningProcess;
    type ReleaseError = lillux::exec::AttachmentReleaseError;
    fn pid(&self) -> u32 {
        lillux::ProcessAwaitingAttachment::pid(self)
    }
    fn pgid(&self) -> i64 {
        lillux::ProcessAwaitingAttachment::pgid(self)
    }
    fn abort_unattached(
        self,
        _state: &AppState,
        _identity: &ryeos_app::process::ExecutionProcessIdentity,
    ) -> Result<()> {
        self.abort_attached()
    }
    fn abort_attached(self) -> Result<()> {
        self.abort_and_reap()
            .map(drop)
            .map_err(anyhow::Error::from)
            .context("abort and reap held process")
    }
    fn release(
        self,
        _identity: &ryeos_app::process::ExecutionProcessIdentity,
    ) -> std::result::Result<lillux::RunningProcess, lillux::exec::AttachmentReleaseError> {
        self.release_after_attachment()
    }
}

impl HeldProcess for ryeos_app::thread_lifecycle::SpawnedItemAwaitingAttachment {
    type Running = ryeos_app::thread_lifecycle::RunningItem;
    type ReleaseError = ryeos_app::thread_lifecycle::ItemReleaseError;
    fn pid(&self) -> u32 {
        self.pid
    }
    fn pgid(&self) -> i64 {
        self.pgid
    }
    fn abort_unattached(
        self,
        _state: &AppState,
        _identity: &ryeos_app::process::ExecutionProcessIdentity,
    ) -> Result<()> {
        self.abort_attached()
    }
    fn abort_attached(self) -> Result<()> {
        self.abort_and_reap()
            .context("abort and reap attachment-pending item")
    }
    fn release(
        self,
        _identity: &ryeos_app::process::ExecutionProcessIdentity,
    ) -> std::result::Result<Self::Running, Self::ReleaseError> {
        self.release_after_attachment()
    }
}

/// The transition at which a held launch failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchStage {
    /// A class-specific check before durable attachment.
    PreAttach,
    Attach,
    WorkspaceActivation,
    /// A class-specific step on the attached launch.
    Attached,
    IssueResources,
    AuthorizeRelease,
    Release,
    /// Post-wait settlement of a released process.
    Settlement,
}

impl LaunchStage {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PreAttach => "validate held launch",
            Self::Attach => "attach process",
            Self::WorkspaceActivation => "activate workspace",
            Self::Attached => "attached launch step",
            Self::IssueResources => "issue resource operations",
            Self::AuthorizeRelease => "authorize release",
            Self::Release => "release attached process",
            Self::Settlement => "settle reaped process",
        }
    }
}

/// What physical cleanup proved for a failed launch.
#[derive(Debug)]
pub(crate) enum PhysicalDisposition {
    /// Abort and reap were proved and any attachment was compare-cleared.
    Aborted { stop_settled: bool },
    /// Cleanup was not proved; the row stays attached and non-terminal.
    UnsettledCleanup(UnsettledLaunchCleanup),
}

/// Names the recovery coordinate of a launch whose cleanup is unproved. The
/// owner-drop stop or reconcile settles it; this value settles nothing.
#[must_use]
#[derive(Debug)]
pub(crate) struct UnsettledLaunchCleanup {
    coord: LaunchOwnerCoordinate,
    process: UnsettledProcess,
}

/// The process coordinate an unsettled cleanup retains.
#[derive(Debug)]
enum UnsettledProcess {
    Exact(ryeos_app::process::ExecutionProcessIdentity),
    /// Identity capture itself failed; only the held numeric coordinate of
    /// the still-unreleased process exists.
    Uncaptured {
        pid: u32,
        pgid: i64,
    },
}

impl UnsettledLaunchCleanup {
    pub(crate) fn coordinate(&self) -> &LaunchOwnerCoordinate {
        &self.coord
    }

    #[cfg(test)]
    pub(crate) fn for_test(coord: LaunchOwnerCoordinate) -> Self {
        Self {
            coord,
            process: UnsettledProcess::Exact(release_failure_controls::identity()),
        }
    }
}

impl Drop for UnsettledLaunchCleanup {
    fn drop(&mut self) {
        // Before attachment no durable row names this process, so the log is
        // the only surviving coordinate: record the full exact identity.
        match &self.process {
            UnsettledProcess::Exact(identity) => tracing::error!(
                thread_id = %self.coord.thread_id(),
                boot_id = %identity.boot_id,
                target_pid = identity.target_pid,
                target_start_time_ticks = identity.target_start_time_ticks,
                group_leader_pid = identity.group_leader_pid,
                group_leader_start_time_ticks = identity.group_leader_start_time_ticks,
                process_scope = ?identity.process_scope,
                "launch cleanup is unproved; exact attachment retained for owner stop or reconcile"
            ),
            UnsettledProcess::Uncaptured { pid, pgid } => tracing::error!(
                thread_id = %self.coord.thread_id(),
                pid,
                pgid,
                "launch cleanup is unproved before identity capture; no durable attachment names this process"
            ),
        }
    }
}

/// A failed launch transition with its original error and physical outcome.
#[derive(Debug)]
pub(crate) struct LaunchFailure {
    pub error: anyhow::Error,
    pub stage: LaunchStage,
    pub disposition: PhysicalDisposition,
}

impl LaunchFailure {
    pub(crate) fn is_unsettled(&self) -> bool {
        matches!(self.disposition, PhysicalDisposition::UnsettledCleanup(_))
    }

    fn unsettled(
        coord: &LaunchOwnerCoordinate,
        identity: &ryeos_app::process::ExecutionProcessIdentity,
        stage: LaunchStage,
        error: anyhow::Error,
    ) -> Self {
        Self {
            error,
            stage,
            disposition: PhysicalDisposition::UnsettledCleanup(UnsettledLaunchCleanup {
                coord: coord.clone(),
                process: UnsettledProcess::Exact(identity.clone()),
            }),
        }
    }

    /// Identity capture failed and the abort of the held process could not be
    /// proved either.
    fn unsettled_uncaptured(
        coord: LaunchOwnerCoordinate,
        pid: u32,
        pgid: i64,
        error: anyhow::Error,
    ) -> Self {
        Self {
            error,
            stage: LaunchStage::PreAttach,
            disposition: PhysicalDisposition::UnsettledCleanup(UnsettledLaunchCleanup {
                coord,
                process: UnsettledProcess::Uncaptured { pid, pgid },
            }),
        }
    }

    /// Physical cleanup is proved; classify a durable stop that may have won.
    fn aborted(stage: LaunchStage, error: anyhow::Error, stop: Result<bool>) -> Self {
        let (error, stop_settled) = match stop {
            Ok(true) => (
                error.context("launch stopped by a durable stop request"),
                true,
            ),
            Ok(false) => (error, false),
            Err(stop_error) => (
                error.context(format!(
                    "durable stop settlement also failed: {stop_error:#}"
                )),
                false,
            ),
        };
        Self {
            error,
            stage,
            disposition: PhysicalDisposition::Aborted { stop_settled },
        }
    }
}

impl std::fmt::Display for LaunchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Only the outermost message: the rest of the chain is `source()`, so
        // an alternate render prints each cause once.
        write!(formatter, "{}", self.error)?;
        match &self.disposition {
            PhysicalDisposition::Aborted { stop_settled: true } => {
                write!(formatter, " (durable stop settled)")
            }
            PhysicalDisposition::Aborted {
                stop_settled: false,
            } => Ok(()),
            PhysicalDisposition::UnsettledCleanup(cleanup) => write!(
                formatter,
                " (physical cleanup unproved; attachment of {} retained)",
                cleanup.coordinate().thread_id()
            ),
        }
    }
}

impl std::error::Error for LaunchFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.chain().nth(1)
    }
}

/// True when `error` carries a launch failure whose cleanup is unproved.
/// Such a row must stay non-terminal and its lifecycle owner armed.
pub(crate) fn is_unsettled_launch_cleanup(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<LaunchFailure>()
            .is_some_and(LaunchFailure::is_unsettled)
    })
}

/// Inputs of the durable attachment transition.
#[derive(Default)]
pub(crate) struct AttachOptions<'a> {
    pub workspace_lifeline: Option<&'a Arc<TempDirGuard>>,
    pub owns_workspace: bool,
    pub launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    pub rearm_resume_budget: bool,
}

/// A held process with its exact identity, before durable attachment.
pub(crate) struct HeldLaunch<P: HeldProcess> {
    state: AppState,
    coord: LaunchOwnerCoordinate,
    identity: ryeos_app::process::ExecutionProcessIdentity,
    process: P,
}

/// A durably attached process; its guard settles the attachment after reap.
pub(crate) struct AttachedLaunch<P: HeldProcess> {
    state: AppState,
    coord: LaunchOwnerCoordinate,
    identity: ryeos_app::process::ExecutionProcessIdentity,
    process: P,
    guard: AttachedProcessGuard,
}

/// An attached launch whose single-use store release fence was consumed.
/// Only [`AttachedLaunch::authorize_release`] constructs it.
pub(crate) struct AuthorizedLaunch<P: HeldProcess>(AttachedLaunch<P>);

/// A released process and its attachment settlement owner.
pub(crate) struct ReleasedLaunch<R> {
    pub running: R,
    pub settlement: AttachedProcessGuard,
}

impl<P: HeldProcess> HeldLaunch<P> {
    pub(crate) fn new(
        state: &AppState,
        coord: LaunchOwnerCoordinate,
        identity: ryeos_app::process::ExecutionProcessIdentity,
        process: P,
    ) -> Self {
        Self {
            state: state.clone(),
            coord,
            identity,
            process,
        }
    }

    pub(crate) fn process(&self) -> &P {
        &self.process
    }

    pub(crate) fn process_mut(&mut self) -> &mut P {
        &mut self.process
    }

    /// Refuse before durable attachment: abort/reap, then classify any stop.
    pub(crate) fn abort(self, stage: LaunchStage, error: anyhow::Error) -> LaunchFailure {
        let Self {
            state,
            coord,
            identity,
            process,
        } = self;
        match process.abort_unattached(&state, &identity) {
            Err(cleanup) => LaunchFailure::unsettled(
                &coord,
                &identity,
                stage,
                error.context(format!(
                    "held-process cleanup failed; retaining authority: {cleanup:#}"
                )),
            ),
            Ok(()) => LaunchFailure::aborted(
                stage,
                error,
                finalize_requested_stop_if_present(&state, coord.thread_id()),
            ),
        }
    }

    /// Durably attach the held identity, activate an owned workspace, and
    /// only then build the settlement guard over the activated binding.
    pub(crate) fn attach(
        self,
        options: AttachOptions<'_>,
    ) -> std::result::Result<AttachedLaunch<P>, LaunchFailure> {
        let attach_params = ThreadAttachProcessParams {
            thread_id: self.coord.thread_id().to_owned(),
            pid: self.process.pid() as i64,
            pgid: self.process.pgid(),
            process_identity: Some(self.identity.clone()),
            metadata: None,
            launch_metadata: options.launch_metadata,
        };
        let attached = if options.rearm_resume_budget {
            self.state
                .threads
                .attach_new_process_owned_rearming_resume_budget(
                    &attach_params,
                    self.coord.launch_owner(),
                )
        } else {
            self.state
                .threads
                .attach_new_process_owned(&attach_params, self.coord.launch_owner())
        };
        if let Err(error) = attached {
            return Err(self.abort(
                LaunchStage::Attach,
                error.context("attach held process identity"),
            ));
        }
        if let Err(error) = super::runner::activate_workspace_after_process_attachment(
            &self.state,
            options.workspace_lifeline,
            options.owns_workspace,
            self.coord.thread_id(),
            self.coord.launch_owner(),
            &self.identity,
        ) {
            return Err(self.abort_without_guard(
                LaunchStage::WorkspaceActivation,
                error.context("activate workspace after process attachment"),
            ));
        }
        match AttachedProcessGuard::new(
            &self.state,
            self.coord.thread_id(),
            self.coord.launch_owner(),
            self.identity.clone(),
        ) {
            Ok(guard) => Ok(AttachedLaunch {
                state: self.state,
                coord: self.coord,
                identity: self.identity,
                process: self.process,
                guard,
            }),
            Err(error) => Err(self.abort_without_guard(
                LaunchStage::Attach,
                error.context("build attached-process owner"),
            )),
        }
    }

    /// Attached, but no settlement guard exists yet.
    fn abort_without_guard(self, stage: LaunchStage, error: anyhow::Error) -> LaunchFailure {
        let Self {
            state,
            coord,
            identity,
            process,
        } = self;
        let cleanup = process.abort_attached().and_then(|()| {
            clear_finished_process(&state, coord.thread_id(), &identity, coord.launch_owner())
        });
        match cleanup {
            Err(cleanup) => LaunchFailure::unsettled(
                &coord,
                &identity,
                stage,
                error.context(format!(
                    "attached-process cleanup failed; retaining authority: {cleanup:#}"
                )),
            ),
            Ok(()) => LaunchFailure::aborted(
                stage,
                error,
                finalize_requested_stop_if_present(&state, coord.thread_id()),
            ),
        }
    }
}

impl<P: HeldProcess> AttachedLaunch<P> {
    /// Refuse after durable attachment: abort/reap, settle the exact
    /// attachment, then classify any stop.
    pub(crate) fn abort(self, stage: LaunchStage, error: anyhow::Error) -> LaunchFailure {
        let Self {
            state,
            coord,
            identity,
            process,
            mut guard,
        } = self;
        match process
            .abort_attached()
            .and_then(|()| guard.settle_after_reap())
        {
            Err(cleanup) => LaunchFailure::unsettled(
                &coord,
                &identity,
                stage,
                error.context(format!(
                    "attached-process cleanup failed; retaining authority: {cleanup:#}"
                )),
            ),
            Ok(()) => LaunchFailure::aborted(
                stage,
                error,
                finalize_requested_stop_if_present(&state, coord.thread_id()),
            ),
        }
    }

    /// Issue resource operations and consume the single-use release fence.
    pub(crate) fn authorize_release(
        self,
    ) -> std::result::Result<AuthorizedLaunch<P>, LaunchFailure> {
        if let Err(error) = ryeos_app::execution_resources::issue_process_resource_operations(
            &self.state,
            &self.identity.resource_operations,
        ) {
            return Err(self.abort(
                LaunchStage::IssueResources,
                error.context("issue process resource operations"),
            ));
        }
        if let Err(error) = ryeos_app::execution_resources::authorize_process_resource_release(
            &self.state,
            &self.identity.resource_operations,
        )
        .and_then(|()| {
            self.state.threads.authorize_process_release_owned(
                self.coord.thread_id(),
                &self.identity,
                self.coord.launch_owner(),
            )
        }) {
            return Err(self.abort(
                LaunchStage::AuthorizeRelease,
                error.context("authorize release after durable attachment"),
            ));
        }
        Ok(AuthorizedLaunch(self))
    }
}

impl<P: HeldProcess> AuthorizedLaunch<P> {
    /// One-shot release. An unproved release cleanup is never settled,
    /// compare-cleared, or terminalized.
    pub(crate) fn release(self) -> std::result::Result<ReleasedLaunch<P::Running>, LaunchFailure> {
        let AttachedLaunch {
            state,
            coord,
            identity,
            process,
            mut guard,
        } = self.0;
        match process.release(&identity) {
            Ok(running) => Ok(ReleasedLaunch {
                running,
                settlement: guard,
            }),
            Err(error) => Err(release_failure(
                &coord,
                &identity,
                error,
                || guard.settle_after_reap(),
                || finalize_requested_stop_if_present(&state, coord.thread_id()),
            )),
        }
    }
}

impl AttachedProcessGuard {
    /// Classify a failed post-wait settlement. The row stays attached.
    pub(crate) fn into_unsettled_failure(self, error: anyhow::Error) -> LaunchFailure {
        let coord = LaunchOwnerCoordinate::thread(&self.thread_id, &self.launch_owner);
        LaunchFailure::unsettled(
            &coord,
            &self.identity,
            LaunchStage::Settlement,
            error.context("settle exact attachment after reap"),
        )
    }
}

fn release_failure<E>(
    coord: &LaunchOwnerCoordinate,
    identity: &ryeos_app::process::ExecutionProcessIdentity,
    error: E,
    settle: impl FnOnce() -> Result<()>,
    settle_stop: impl FnOnce() -> Result<bool>,
) -> LaunchFailure
where
    E: ReleaseTestimony + std::error::Error + Send + Sync + 'static,
{
    if !error.cleanup_is_settled() {
        return LaunchFailure::unsettled(
            coord,
            identity,
            LaunchStage::Release,
            anyhow::Error::new(error)
                .context("subprocess release cleanup is unproved; retaining authority"),
        );
    }
    match settle() {
        Ok(()) => LaunchFailure::aborted(
            LaunchStage::Release,
            anyhow::Error::new(error).context("release subprocess after durable attachment"),
            settle_stop(),
        ),
        Err(settlement) => LaunchFailure::unsettled(
            coord,
            identity,
            LaunchStage::Release,
            anyhow::Error::new(error).context(format!(
                "release failed; exact attachment retained: {settlement:#}"
            )),
        ),
    }
}

/// Guard-construction-failure cleanup for an attached identity that has no
/// settlement guard yet. Waited processes settle through their guard.
pub(crate) fn clear_finished_process(
    state: &AppState,
    thread_id: &str,
    process_identity: &ryeos_app::process::ExecutionProcessIdentity,
    launch_owner: &str,
) -> Result<()> {
    let settled = (|| -> Result<bool> {
        ryeos_app::process::assert_reaped_process_group_absent(process_identity)?;
        let proposed =
            ryeos_app::runtime_db::ProcessResourceCleanupEvidence::capture(process_identity)?;
        let evidence = state.state_store.prove_thread_resource_owner_cleanup(
            thread_id,
            process_identity,
            &proposed,
        )?;
        ryeos_app::execution_resources::settle_process_resource_operations_after_cleanup(
            state,
            process_identity,
            &evidence,
        )?;
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

#[cfg(test)]
mod release_failure_controls {
    use super::*;
    use lillux::exec::SubprocessResult;

    #[derive(Debug)]
    struct ReleaseFixture {
        settled: bool,
        result: SubprocessResult,
    }
    impl std::fmt::Display for ReleaseFixture {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.result.stderr)
        }
    }
    impl std::error::Error for ReleaseFixture {}
    impl ReleaseTestimony for ReleaseFixture {
        fn cleanup_is_settled(&self) -> bool {
            self.settled
        }
    }

    fn fixture(settled: bool) -> ReleaseFixture {
        ReleaseFixture {
            settled,
            result: SubprocessResult {
                success: false,
                stdout: String::new(),
                stderr: "target absent; wrapper/scope disposition unresolved".into(),
                exit_code: -1,
                duration_ms: 0.0,
                pid: 0,
                timed_out: false,
                launcher_refusal: None,
                aborted_before_attachment: None,
                output_limit_exceeded: None,
                stdout_truncated: false,
                stderr_truncated: false,
            },
        }
    }

    pub(super) fn identity() -> ryeos_app::process::ExecutionProcessIdentity {
        ryeos_app::process::ExecutionProcessIdentity {
            schema_version: ryeos_app::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            boot_id: "test-boot".to_owned(),
            target_pid: 4242,
            target_start_time_ticks: 10,
            group_leader_pid: 4242,
            group_leader_start_time_ticks: 10,
            process_scope: None,
            resource_selections: Vec::new(),
            resource_settlement_authority: None,
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        }
    }

    fn coord() -> LaunchOwnerCoordinate {
        LaunchOwnerCoordinate::thread("T-release-control", "owner")
    }

    #[test]
    fn absent_target_without_wrapper_and_scope_cleanup_proof_keeps_attachment() {
        let error = fixture(false);
        assert_eq!(error.result.pid, 0);
        assert!(error.result.aborted_before_attachment.is_none());
        let mut settlement_called = false;
        let mut stop_called = false;
        let failure = release_failure(
            &coord(),
            &identity(),
            error,
            || {
                settlement_called = true;
                Ok(())
            },
            || {
                stop_called = true;
                Ok(false)
            },
        );
        assert!(
            !settlement_called,
            "unproved physical cleanup must not clear durable attachment"
        );
        assert!(
            !stop_called,
            "unproved physical cleanup must not settle a durable stop"
        );
        assert!(failure.is_unsettled());
        assert_eq!(failure.stage, LaunchStage::Release);
        assert!(failure.error.downcast_ref::<ReleaseFixture>().is_some());
        assert!(failure.error.to_string().contains("cleanup is unproved"));
        let carried = anyhow::Error::new(failure);
        assert!(is_unsettled_launch_cleanup(&carried));
        assert!(
            is_unsettled_launch_cleanup(&carried.context("spawn/attach")),
            "classification must survive added context"
        );
    }

    #[test]
    fn proved_release_with_failed_settlement_is_unsettled() {
        let mut stop_called = false;
        let failure = release_failure(
            &coord(),
            &identity(),
            fixture(true),
            || anyhow::bail!("workspace binding changed"),
            || {
                stop_called = true;
                Ok(false)
            },
        );
        assert!(!stop_called);
        assert!(failure.is_unsettled());
        assert!(failure.error.downcast_ref::<ReleaseFixture>().is_some());
        assert!(format!("{:#}", failure.error).contains("workspace binding changed"));
    }

    #[test]
    fn proved_release_and_settlement_is_aborted_with_stop_classified() {
        let failure = release_failure(&coord(), &identity(), fixture(true), || Ok(()), || Ok(true));
        assert!(matches!(
            failure.disposition,
            PhysicalDisposition::Aborted { stop_settled: true }
        ));
        assert!(!is_unsettled_launch_cleanup(&anyhow::Error::new(failure)));
    }

    #[test]
    fn class_c_release_error_keeps_engine_testimony() {
        use ryeos_app::thread_lifecycle::ItemReleaseError;
        let incomplete = ItemReleaseError::IncompleteOccupancyContract;
        assert!(!ReleaseTestimony::cleanup_is_settled(&incomplete));
        let opaque = ItemReleaseError::Release(ryeos_engine::error::EngineError::ExecutionFailed {
            reason: "no process remains".to_owned(),
        });
        assert!(!ReleaseTestimony::cleanup_is_settled(&opaque));
        let failure = release_failure(&coord(), &identity(), opaque, || Ok(()), || Ok(false));
        assert!(failure.is_unsettled());
        assert!(failure.error.downcast_ref::<ItemReleaseError>().is_some());
    }

    #[test]
    fn launch_failure_alternate_render_prints_each_cause_once() {
        let failure = release_failure(
            &coord(),
            &identity(),
            fixture(true),
            || anyhow::bail!("workspace binding changed"),
            || Ok(false),
        );
        let rendered = format!("{:#}", anyhow::Error::new(failure).context("spawn/attach"));
        assert_eq!(
            rendered.matches("workspace binding changed").count(),
            1,
            "{rendered}"
        );
        assert_eq!(
            rendered
                .matches("wrapper/scope disposition unresolved")
                .count(),
            1,
            "{rendered}"
        );
        assert!(rendered.contains("physical cleanup unproved"), "{rendered}");
    }

    #[test]
    fn class_b_capture_and_abort_failure_is_typed_unsettled() {
        let failure = LaunchFailure::unsettled_uncaptured(
            coord(),
            4242,
            4242,
            anyhow::anyhow!("capture held callback subprocess identity")
                .context("pending-process cleanup failed: reap unproved"),
        );
        assert!(failure.is_unsettled());
        assert_eq!(failure.stage, LaunchStage::PreAttach);
        assert!(matches!(
            &failure.disposition,
            PhysicalDisposition::UnsettledCleanup(UnsettledLaunchCleanup {
                process: UnsettledProcess::Uncaptured {
                    pid: 4242,
                    pgid: 4242
                },
                coord: _,
            })
        ));
        let carried = anyhow::Error::new(failure).context("spawn/attach");
        assert!(is_unsettled_launch_cleanup(&carried));
    }

    #[test]
    fn class_a_and_b_unsettled_failures_are_never_finalizable() {
        for stage in [
            LaunchStage::Attach,
            LaunchStage::Release,
            LaunchStage::Settlement,
        ] {
            let failure = LaunchFailure::unsettled(
                &coord(),
                &identity(),
                stage,
                anyhow::anyhow!("cleanup unproved"),
            );
            assert!(failure.is_unsettled());
            assert_eq!(failure.stage, stage);
            assert!(is_unsettled_launch_cleanup(&anyhow::Error::new(failure)));
        }
        let aborted = LaunchFailure::aborted(
            LaunchStage::Attach,
            anyhow::anyhow!("attach refused"),
            Ok(true),
        );
        assert!(!aborted.is_unsettled());
        assert!(!is_unsettled_launch_cleanup(&anyhow::Error::new(aborted)));
    }
}
