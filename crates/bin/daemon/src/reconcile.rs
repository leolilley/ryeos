use std::collections::BTreeSet;

use anyhow::Result;
use serde_json::json;

use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
use ryeos_app::process::{
    execution_group_liveness, execution_identity_is_current_boot, execution_liveness,
    kill_by_action, resolve_shutdown_action, IdentityLiveness,
};
use ryeos_app::runtime_db::WorkspaceState;
use ryeos_app::state::AppState;
use ryeos_app::state_store::ThreadDetail;
use ryeos_app::thread_lifecycle::ThreadFinalizeParams;
use ryeos_state::objects::ThreadStatus;

/// A runtime owner gets a short, durable post-exit window to freeze/fold back
/// and settle. A second live sweep after this boundary may revoke a provably
/// dead exact owner; an in-memory task registration alone is not a heartbeat.
const ACTIVE_OWNER_DEAD_SETTLEMENT_GRACE_MS: i64 = 5_000;
const PROJECT_AUTHORITY_WAIT_MS: i64 = 15 * 60 * 1_000;

fn live_project_authority_available(resume: &ResumeContext) -> Result<(), String> {
    let ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
        canonical_root,
        authored_project_identity,
        live_access,
        ..
    } = &resume.project_authority
    else {
        return Ok(());
    };
    let root = lillux::PinnedDirectory::open(canonical_root)
        .map_err(|error| format!("open authorized live project root: {error:#}"))?
        .ok_or_else(|| {
            format!(
                "authorized live project root is unavailable: {}",
                canonical_root.display()
            )
        })?;
    if !root
        .entry_names()
        .map_err(|error| format!("inspect authorized live project root: {error:#}"))?
        .iter()
        .any(|name| name.as_os_str() == std::ffi::OsStr::new(ryeos_engine::AI_DIR))
    {
        return Err(format!(
            "authorized live project root no longer contains {}: {}",
            ryeos_engine::AI_DIR,
            canonical_root.display()
        ));
    }
    let expected = format!("local:{}", canonical_root.display());
    if authored_project_identity != &expected {
        return Err("authorized live project identity no longer matches its canonical root".into());
    }
    live_access
        .validate()
        .map_err(|error| format!("invalid persisted live access authority: {error:#}"))?;
    Ok(())
}

fn bounded_recovery_detail(detail: &str) -> String {
    let normalized = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(2048).collect()
}

/// The structural shape of a continuation successor stranded `created`: never
/// launched, links upstream, and carries a captured `ResumeContext`. Lineage wins
/// over checkpoint resume here — a `created` successor is launched as a
/// continuation even when its runtime declares `native_resume` (a graph successor
/// is both). Checkpoint resume (`decide_resume`) owns RUNNING/crashed
/// same-threads, which this `created` gate excludes — a successor is a NEW thread,
/// not a re-spawn of one that already ran. NECESSARY but not sufficient — an
/// operator follow-up successor shares this shape, so [`is_machine_successor`] /
/// [`is_operator_successor`] are the proofs applied on top.
fn continuation_shape(thread: &ThreadDetail) -> bool {
    thread.status == ThreadStatus::Created.as_str()
        && thread.upstream_thread_id.is_some()
        && thread
            .runtime
            .launch_metadata
            .as_ref()
            .is_some_and(|m| m.resume_context.is_some())
}

/// A follow CHILD row that provably NEVER launched — the pre-launch crash window.
/// Shared by `reconcile`'s skip and `reconcile_follow`'s relaunch so they agree
/// exactly. All of:
///   - status `created` (not yet running),
///   - NO attached pgid — a launch attaches the process group BEFORE flipping the
///     row to `running`, so a present pgid means a launch is in flight or ran (a
///     duplicate relaunch would spawn a second child),
///   - NO `native_resume` policy — set only once the launch runs (a launched-then-
///     crashed child carries it and is recovered by the native-resume path),
///   - complete current-format resume, sealed-root, and parent authority whose
///     identities agree with the authoritative row, and
///   - an exact sealed-authority match with its non-cleared waiter slot.
///
/// A row failing any clause is left to the normal `reconcile` / native-resume /
/// finalize paths — never skipped here and never relaunched.
fn is_pending_follow_child(state: &AppState, thread: &ThreadDetail) -> Result<bool> {
    if thread.status != ThreadStatus::Created.as_str() {
        return Ok(false);
    }
    if thread.runtime.pgid.is_some() {
        return Ok(false);
    }
    let Some(metadata) = thread.runtime.launch_metadata.as_ref() else {
        return Ok(false);
    };
    if metadata.native_resume.is_some() {
        return Ok(false);
    }
    let Some(waiter) = state
        .state_store
        .get_follow_waiter_by_child_chain(&thread.chain_root_id)?
    else {
        return Ok(false);
    };
    let Some(slot) = waiter
        .children
        .iter()
        .find(|child| child.child_thread_id == thread.thread_id)
    else {
        return Ok(false);
    };
    let (Some(resume), Some(sealed), Some(parent_context)) = (
        metadata.resume_context.as_ref(),
        metadata.sealed_root_request.as_ref(),
        metadata.follow_parent_context.as_ref(),
    ) else {
        // Current-format child birth installs all three values before the
        // authoritative root becomes visible. A partial row is not launch
        // authority and must fall through to ordinary reconciliation.
        return Ok(false);
    };
    if slot.child_chain_root_id != thread.chain_root_id
        || resume.kind != thread.kind
        || resume.item_ref != thread.item_ref
        || resume.item_ref != slot.item_ref
        || resume.launch_mode != thread.launch_mode
        || resume.launch_mode != "detached"
        || resume.executor_ref.as_deref() != Some(sealed.executor_ref())
        || resume.runtime_ref.as_deref() != Some(sealed.runtime_ref())
        || sealed.item_ref() != slot.item_ref
        || parent_context.parent_thread_id != waiter.parent_thread_id
    {
        return Ok(false);
    }
    if serde_json::to_value(sealed)? != serde_json::to_value(&slot.sealed_root_request)? {
        anyhow::bail!(
            "follow child {} sealed launch authority differs from waiter {} slot",
            thread.thread_id,
            waiter.follow_key
        );
    }
    Ok(true)
}

/// Machine-only proof that `successor` is an autonomous MACHINE continuation of
/// `upstream` (not an operator follow-up): the source was settled `continued` and
/// points back at this successor. A machine handoff settles the source to
/// `continued`; an operator follow-up PRESERVES the source's completed/failed
/// status — so this never matches an operator turn, preventing reconcile from
/// relaunching one via `launch_successor` (which sets `suppress_stimulus=true`
/// and would silently drop the operator's input).
fn is_machine_successor(successor: &ThreadDetail, upstream: &ThreadDetail) -> bool {
    upstream.status == ThreadStatus::Continued.as_str()
        && upstream.successor_thread_id.as_deref() == Some(successor.thread_id.as_str())
}

/// Pure proof that `successor` is an OPERATOR follow-up of `upstream` (not a
/// machine continuation): the source is a terminal `completed`/`failed` turn
/// (operator follow-up PRESERVES the source status) that points back at this
/// successor. The caller additionally confirms the persisted request fingerprint
/// before recovering — only operator `create_or_get` records one.
fn is_operator_successor(successor: &ThreadDetail, upstream: &ThreadDetail) -> bool {
    (upstream.status == ThreadStatus::Completed.as_str()
        || upstream.status == ThreadStatus::Failed.as_str())
        && upstream.successor_thread_id.as_deref() == Some(successor.thread_id.as_str())
}

/// Decision the reconciler makes for a single interrupted thread after any
/// attached prior-daemon execution has been terminated or safely cleared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDecision {
    /// No `native_resume` policy on the persisted spec — finalize.
    NoResumePolicy,
    /// Spec declared `native_resume` but no `ResumeContext` was
    /// captured at spawn (corruption, partial spawn, schema drift).
    /// Cannot rebuild `ExecutionParams` — finalize.
    MissingResumeContext,
    /// Thread is eligible for auto-resume; the reconciler should
    /// increment the counter and re-spawn with `RYEOS_RESUME=1`.
    Resume { next_attempt: u32, max: u32 },
    /// Spec declared `native_resume` and a `ResumeContext` is present,
    /// but the retry budget is exhausted — finalize.
    Exhausted { attempts: u32, max: u32 },
}

/// Pure mapping from `(launch_metadata, current_attempts)` to a
/// reconciler action. Extracted so it can be unit-tested without
/// touching the daemon state.
pub fn decide_resume(
    metadata: Option<&RuntimeLaunchMetadata>,
    current_attempts: u32,
) -> ResumeDecision {
    let m = match metadata {
        Some(m) if m.declares_native_resume() => m,
        _ => return ResumeDecision::NoResumePolicy,
    };
    let policy = m
        .native_resume
        .as_ref()
        .expect("declares_native_resume implies native_resume.is_some()");
    // resume_context is required to actually re-spawn; without it we
    // can't reconstruct ExecutionParams.
    if metadata.and_then(|m| m.resume_context.as_ref()).is_none() {
        return ResumeDecision::MissingResumeContext;
    }
    if current_attempts >= policy.max_auto_resume_attempts {
        return ResumeDecision::Exhausted {
            attempts: current_attempts,
            max: policy.max_auto_resume_attempts,
        };
    }
    ResumeDecision::Resume {
        next_attempt: current_attempts + 1,
        max: policy.max_auto_resume_attempts,
    }
}

/// What re-dispatch a `ResumeIntent` represents — the two take different launch
/// paths once the daemon's listeners are bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeKind {
    /// A fresh admitted root whose signed birth committed but whose first
    /// process never attached. Launch exactly once from its sealed capsule.
    AdmittedRoot,
    /// Checkpoint resume of a `native_resume` thread: re-run the SAME thread from
    /// its checkpoint via `run_existing_detached`.
    NativeResume,
    /// MACHINE continuation successor stranded `created` by a crash between create
    /// and live launch: launch via `launch_successor` (folds the chain, no
    /// stimulus). Gated by the auto-machine-continuation flag.
    Continuation,
    /// OPERATOR follow-up successor stranded `created` by a crash between
    /// create-or-get and the spawned launch: launch via `launch_operator_successor`
    /// (injects the seeded operator input as stimulus). NOT gated — accepted
    /// operator work must be recovered, not finalized as failed and lost.
    OperatorContinuation,
}

/// A thread that the reconciler decided to auto-resume. The dispatcher
/// (in `main.rs`, AFTER the daemon's listeners are bound) consumes
/// this and calls `runner::run_existing_detached` (native resume) or
/// `launch::launch_successor` (continuation), per `kind`.
#[derive(Debug, Clone)]
pub struct ResumeIntent {
    pub thread_id: String,
    pub chain_root_id: String,
    pub resume_context: ResumeContext,
    /// Executor boundary admitted with the launch capsule. Dispatch must never
    /// infer this from item kind or ref spelling during recovery.
    pub launch_driver: ryeos_state::objects::ExecutionLaunchDriver,
    /// Status of the thread row at reconcile time. The dispatcher uses
    /// this to decide whether to call `mark_running` after attaching the new
    /// spawn: `created` rows have never made that transition, while resumed
    /// `running` rows already have.
    pub prior_status: String,
    /// Which launch path to take post-listener.
    pub kind: ResumeKind,
}

/// Complete result of one indexed active-thread reconciliation pass.
/// `active_thread_ids` is the initial `created|running` set owned by process
/// recovery, including rows classified into follow/window ownership rather
/// than a resume intent. Daemon-owned non-execution roots are deliberately
/// absent: their subsystem owns their lifecycle and process recovery has no
/// claim/process invariant to prove for them.
#[derive(Debug, Clone)]
pub struct ActiveThreadReconcileReport {
    pub active_thread_ids: BTreeSet<String>,
    pub resume_intents: Vec<ResumeIntent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveReconcileMode {
    /// No process from the prior daemon can retain callback authority. Prove
    /// its identity, stop it, then classify the row for resume/finalization.
    Startup,
    /// Processes launched by this daemon remain owned. Only rows with no live
    /// process and no active launch claim are re-driven.
    Live,
}

/// Finalize a dead thread as failed. Centralized so the `Finalize` /
/// `FinalizeExhausted` / orphan branches share identical bookkeeping.
fn finalize_dead(
    state: &AppState,
    thread_id: &str,
    pgid: Option<i64>,
    prior_status: &str,
    extra: Option<(&'static str, serde_json::Value)>,
    reconciled: &mut usize,
) -> Result<()> {
    let (outcome_code, error_extra) = match extra {
        Some((code, val)) => (code, Some(val)),
        None => ("daemon_reconciled", None),
    };
    let mut error_obj = serde_json::json!({
        "message": "thread in non-terminal state after daemon restart; process is dead",
        "reconciled_from_status": prior_status,
        "pgid": pgid,
    });
    if let Some(extra) = error_extra {
        error_obj["details"] = extra;
    }
    state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: "failed".to_string(),
        outcome_code: Some(outcome_code.to_string()),
        result: None,
        error: Some(error_obj),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    })?;
    *reconciled += 1;
    Ok(())
}

fn finalize_recovered_stop(
    state: &AppState,
    thread_id: &str,
    prior_status: &str,
    stop_intent: ryeos_app::state_store::StopIntent,
    reconciled: &mut usize,
) {
    let terminal_status = match stop_intent {
        ryeos_app::state_store::StopIntent::Cancel => "cancelled",
        ryeos_app::state_store::StopIntent::Kill => "killed",
    };
    if let Err(error) = state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: terminal_status.to_string(),
        outcome_code: Some(terminal_status.to_string()),
        result: None,
        error: Some(json!({
            "reason": "recovered_durable_stop_request",
            "stop_intent": stop_intent.as_str(),
            "reconciled_from_status": prior_status,
        })),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    }) {
        tracing::warn!(thread_id, error = %error, "failed to finalize recovered stop request");
    } else {
        *reconciled += 1;
    }
}

fn dead_identity_safe_to_clear(
    identity: &ryeos_app::process::ExecutionProcessIdentity,
    thread_id: &str,
) -> bool {
    match execution_identity_is_current_boot(identity) {
        Ok(false) => true,
        Ok(true)
            if ryeos_app::process::process_group_presence(identity.pgid())
                == IdentityLiveness::DeadOrStale =>
        {
            true
        }
        Ok(true) => {
            tracing::warn!(
                thread_id,
                "dead same-boot group leader still has a present or unprovable process group; quarantining"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                thread_id,
                error = %error,
                "cannot compare process-identity boot; quarantining"
            );
            false
        }
    }
}

/// Reconcile daemon state after restart.
///
/// Two phases, against an already-current projection:
/// 1. Find threads left in non-terminal status whose processes are dead
///    (or whose spawn was interrupted before pid/pgid attach)
/// 2. For each: either finalize-as-failed, or collect a `ResumeIntent`
///    that the caller dispatches AFTER the daemon's listeners are bound
///
/// Returns the `ResumeIntent`s the caller must dispatch. The reconciler
/// itself never calls `run_existing_detached` — it would race the
/// daemon's listener startup, and a resumed subprocess making its first
/// callback before the UDS / HTTP server is bound would fail.
#[tracing::instrument(name = "state:reconcile", skip(state))]
pub async fn reconcile_active_threads(state: &AppState) -> Result<ActiveThreadReconcileReport> {
    reconcile_active_threads_inner(state, ActiveReconcileMode::Startup).await
}

/// Mid-life counterpart used by the supervised periodic recovery task. It
/// never clears claims or stops a verified live process owned by this daemon.
pub async fn reconcile_live_threads(state: &AppState) -> Result<ActiveThreadReconcileReport> {
    reconcile_active_threads_inner(state, ActiveReconcileMode::Live).await
}

async fn reconcile_active_threads_inner(
    state: &AppState,
    mode: ActiveReconcileMode,
) -> Result<ActiveThreadReconcileReport> {
    repair_detached_spawn_links(state)?;
    let blocked_freezes = reconcile_execution_workspaces(state, mode)?;
    // Orphan thread cleanup.
    let mut running_threads = state
        .state_store
        .list_threads_by_status(&["created", "running"])?;
    let active_thread_ids = running_threads
        .iter()
        .filter(|thread| {
            let kind_profile = state.threads.kind_profiles().get(&thread.kind);
            !is_daemon_owned_non_execution_thread(thread, kind_profile)
        })
        .map(|thread| thread.thread_id.clone())
        .collect::<BTreeSet<_>>();
    for thread_id in state.state_store.list_attached_thread_ids()? {
        if running_threads
            .iter()
            .any(|thread| thread.thread_id == thread_id)
        {
            continue;
        }
        if let Some(thread) = state.state_store.get_thread(&thread_id)? {
            running_threads.push(thread);
        }
    }

    if running_threads.is_empty() {
        tracing::debug!("no orphaned threads");
        return Ok(ActiveThreadReconcileReport {
            active_thread_ids,
            resume_intents: Vec::new(),
        });
    }

    tracing::info!(
        count = running_threads.len(),
        "checking non-terminal threads"
    );

    let mut reconciled = 0usize;
    let mut intents: Vec<ResumeIntent> = Vec::new();

    for thread in &running_threads {
        if blocked_freezes.contains(&thread.thread_id) {
            tracing::error!(
                thread_id = %thread.thread_id,
                "thread remains quarantined until its journaled workspace freeze can be recovered"
            );
            continue;
        }
        let kind_profile = state.threads.kind_profiles().get(&thread.kind);
        if is_daemon_owned_non_execution_thread(thread, kind_profile) {
            tracing::debug!(
                thread_id = %thread.thread_id,
                kind = %thread.kind,
                status = %thread.status,
                "daemon-owned non-execution thread — leaving to its lifecycle owner"
            );
            continue;
        }

        let pgid = thread.runtime.pgid;
        let identity = thread.runtime.process_identity.as_ref();
        let target_liveness = identity
            .map(execution_liveness)
            .unwrap_or(IdentityLiveness::DeadOrStale);
        let group_liveness = identity
            .map(execution_group_liveness)
            .unwrap_or(IdentityLiveness::DeadOrStale);
        let launch_claim = state.state_store.get_launch_claim(&thread.thread_id)?;

        if mode == ActiveReconcileMode::Live && thread.runtime.stop_requested_at_ms.is_none() {
            if let Some(claim) = launch_claim
                .as_ref()
                .filter(|claim| state.state_store.is_launch_owner_active(&claim.claimed_by))
            {
                let Some(identity) = identity else {
                    // Resolution and materialization before attach are
                    // deliberately unbounded; absence of a process is not a
                    // timeout authority.
                    continue;
                };
                if target_liveness == IdentityLiveness::Alive
                    || target_liveness == IdentityLiveness::Unavailable
                    || group_liveness == IdentityLiveness::Unavailable
                {
                    continue;
                }
                let now_ms = lillux::time::timestamp_millis();
                let Some(observed_at_ms) = state.state_store.observe_active_owner_dead_process(
                    &thread.thread_id,
                    &claim.claim_id,
                    &claim.claimed_by,
                    identity,
                    now_ms,
                )?
                else {
                    continue;
                };
                if now_ms.saturating_sub(observed_at_ms) < ACTIVE_OWNER_DEAD_SETTLEMENT_GRACE_MS {
                    continue;
                }
                if group_liveness == IdentityLiveness::Alive {
                    let killed = ryeos_app::process::kill_by_action(
                        identity,
                        ryeos_app::process::ShutdownAction::Hard,
                    );
                    if !killed.success
                        || execution_group_liveness(identity) != IdentityLiveness::DeadOrStale
                    {
                        tracing::error!(
                            thread_id = %thread.thread_id,
                            launch_owner = %claim.claimed_by,
                            method = killed.method,
                            "dead runtime target retained a live execution group that could not be terminated; preserving ownership"
                        );
                        continue;
                    }
                }
                if !state.state_store.release_active_thread_launch_claim(
                    &thread.thread_id,
                    &claim.claim_id,
                    &claim.claimed_by,
                )? {
                    continue;
                }
                tracing::warn!(
                    thread_id = %thread.thread_id,
                    launch_owner = %claim.claimed_by,
                    observed_at_ms,
                    "revoked a current-daemon owner whose exact runtime remained dead past the settlement grace"
                );
            }
        }

        // Explicit cancellation/kill intent survives a daemon crash. Recover it
        // before any normal resume decision so a stop-requested row can never
        // launch a replacement. Exact pidfd group signalling fails closed.
        if thread.runtime.stop_requested_at_ms.is_some() {
            let stop_intent = thread
                .runtime
                .stop_intent
                .expect("runtime decode enforces complete stop tombstones");
            if let Some(identity) = identity {
                if group_liveness == IdentityLiveness::Alive {
                    let action = match stop_intent {
                        ryeos_app::state_store::StopIntent::Kill => {
                            ryeos_app::process::ShutdownAction::Hard
                        }
                        ryeos_app::state_store::StopIntent::Cancel => resolve_shutdown_action(
                            thread
                                .runtime
                                .launch_metadata
                                .as_ref()
                                .and_then(|metadata| metadata.cancellation_mode),
                        ),
                    };
                    let result = kill_by_action(identity, action);
                    if !result.success {
                        tracing::warn!(
                            thread_id = %thread.thread_id,
                            method = result.method,
                            "durable stop recovery could not prove process-group termination"
                        );
                        continue;
                    }
                } else if group_liveness == IdentityLiveness::Unavailable
                    || target_liveness != IdentityLiveness::DeadOrStale
                {
                    tracing::warn!(
                        thread_id = %thread.thread_id,
                        target_liveness = ?target_liveness,
                        group_liveness = ?group_liveness,
                        "durable stop process liveness is unavailable; failing closed"
                    );
                    continue;
                } else if !dead_identity_safe_to_clear(identity, &thread.thread_id) {
                    continue;
                }
                match state
                    .state_store
                    .clear_thread_process_if_matches(&thread.thread_id, identity)
                {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            thread_id = %thread.thread_id,
                            "durable stop identity changed before compare-and-clear"
                        );
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(
                            thread_id = %thread.thread_id,
                            error = %error,
                            "failed to clear durable stop identity"
                        );
                        continue;
                    }
                }
            }
            if let Some(claim) = launch_claim.as_ref() {
                state
                    .state_store
                    .release_thread_launch_claim(&thread.thread_id, &claim.claim_id)?;
            }
            if !ryeos_app::state_store::is_terminal_status(&thread.status) {
                finalize_recovered_stop(
                    state,
                    &thread.thread_id,
                    &thread.status,
                    stop_intent,
                    &mut reconciled,
                );
            }
            continue;
        }

        if ryeos_app::state_store::is_terminal_status(&thread.status) {
            if let Some(identity) = identity {
                if group_liveness == IdentityLiveness::Alive {
                    let result = kill_by_action(identity, ryeos_app::process::ShutdownAction::Hard);
                    if !result.success {
                        tracing::warn!(
                            thread_id = %thread.thread_id,
                            method = result.method,
                            "failed to drain terminal attached process during reconcile"
                        );
                        continue;
                    }
                } else if group_liveness == IdentityLiveness::Unavailable
                    || target_liveness != IdentityLiveness::DeadOrStale
                {
                    tracing::warn!(
                        thread_id = %thread.thread_id,
                        target_liveness = ?target_liveness,
                        group_liveness = ?group_liveness,
                        "terminal process liveness is unavailable; failing closed"
                    );
                    continue;
                } else if !dead_identity_safe_to_clear(identity, &thread.thread_id) {
                    continue;
                }
                if !state
                    .state_store
                    .clear_thread_process_if_matches(&thread.thread_id, identity)?
                {
                    tracing::warn!(
                        thread_id = %thread.thread_id,
                        "terminal identity changed before reconcile compare-and-clear"
                    );
                }
            }
            if let Some(claim) = launch_claim.as_ref() {
                state
                    .state_store
                    .release_thread_launch_claim(&thread.thread_id, &claim.claim_id)?;
            }
            continue;
        }

        // The single-daemon state lock proves an exact still-live attached group
        // belongs to the interrupted prior daemon instance. Tear that group down
        // through its verified pidfd before relaunching; never adopt an orphan
        // whose stdout/wait ownership was lost in the crash. If the same-boot
        // leader is already gone, descendants cannot be proven absent and the
        // conservative quarantine below remains mandatory.
        if let Some(identity) = identity {
            if mode == ActiveReconcileMode::Live && group_liveness == IdentityLiveness::Alive {
                continue;
            }
            match group_liveness {
                IdentityLiveness::Alive => {
                    let result = kill_by_action(identity, ryeos_app::process::ShutdownAction::Hard);
                    if !result.success {
                        tracing::warn!(
                            thread_id = %thread.thread_id,
                            method = result.method,
                            "startup could not terminate exact orphan process group"
                        );
                        continue;
                    }
                }
                IdentityLiveness::DeadOrStale => {
                    if target_liveness != IdentityLiveness::DeadOrStale
                        || !dead_identity_safe_to_clear(identity, &thread.thread_id)
                    {
                        continue;
                    }
                }
                IdentityLiveness::Unavailable => {
                    tracing::warn!(
                        thread_id = %thread.thread_id,
                        target_liveness = ?target_liveness,
                        group_liveness = ?group_liveness,
                        "process liveness is unavailable; refusing reconcile resume"
                    );
                    continue;
                }
            }
            match state
                .state_store
                .clear_thread_process_if_matches(&thread.thread_id, identity)
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        thread_id = %thread.thread_id,
                        "attached identity changed before reconcile compare-and-clear"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        thread_id = %thread.thread_id,
                        error = %error,
                        "failed to clear terminated process identity before reconcile"
                    );
                    continue;
                }
            }
        }

        // Reaching this boundary proves there is no live attached owner. A
        // surviving exact claim is an abandoned pre-attach/attach owner from a
        // cancelled task or prior daemon generation. Revoke only that claim;
        // never delete another epoch wholesale.
        if let Some(claim) = launch_claim.as_ref() {
            if state
                .state_store
                .release_thread_launch_claim(&thread.thread_id, &claim.claim_id)?
            {
                tracing::info!(
                    thread_id = %thread.thread_id,
                    claim_id = %claim.claim_id,
                    "revoked abandoned exact launch claim during reconciliation"
                );
            }
        }

        let interrupted_spawn = pgid.is_none();

        if let Some(resume) = thread
            .runtime
            .launch_metadata
            .as_ref()
            .and_then(|metadata| metadata.resume_context.as_ref())
        {
            if resume.lifecycle_authority.recovery
                == ryeos_state::objects::ExecutionRecoveryAuthority::None
            {
                state.state_store.clear_recovery_wait(&thread.thread_id)?;
                finalize_dead(
                    state,
                    &thread.thread_id,
                    pgid,
                    &thread.status,
                    Some((
                        "execution_not_restart_recoverable",
                        json!({
                            "reason": "the admitted execution lifecycle authority forbids restart recovery"
                        }),
                    )),
                    &mut reconciled,
                )?;
                continue;
            }
            match live_project_authority_available(resume) {
                Ok(()) => {
                    if thread.runtime.recovery_wait.is_some() {
                        state.state_store.clear_recovery_wait(&thread.thread_id)?;
                    }
                }
                Err(detail) => {
                    let now_ms = lillux::time::timestamp_millis();
                    let wait = state.state_store.wait_for_project_authority(
                        &thread.thread_id,
                        "waiting_for_project_authority",
                        &bounded_recovery_detail(&detail),
                        now_ms,
                        now_ms.saturating_add(PROJECT_AUTHORITY_WAIT_MS),
                    )?;
                    if now_ms >= wait.deadline_at_ms {
                        finalize_dead(
                            state,
                            &thread.thread_id,
                            pgid,
                            &thread.status,
                            Some((
                                "project_authority_unavailable",
                                json!({
                                    "reason": wait.reason,
                                    "detail": wait.detail,
                                    "started_at_ms": wait.started_at_ms,
                                    "deadline_at_ms": wait.deadline_at_ms,
                                }),
                            )),
                            &mut reconciled,
                        )?;
                        state.state_store.clear_recovery_wait(&thread.thread_id)?;
                    } else {
                        tracing::warn!(
                            thread_id = %thread.thread_id,
                            deadline_at_ms = wait.deadline_at_ms,
                            detail = %wait.detail,
                            "thread is waiting for its admitted live project authority"
                        );
                    }
                    continue;
                }
            }
        }

        // Continuation successor safety net: a `created` thread that links upstream
        // and carries a captured `ResumeContext`. A crash between create and the
        // spawned launch strands it here. Lineage is classified BEFORE
        // `decide_resume`: both MACHINE (source settled `continued`) and OPERATOR
        // (source preserved completed/failed) successors are launched as
        // continuations here — even when the runtime declares `native_resume` (a
        // graph successor is both), so a NEW successor is never mistaken for a
        // checkpoint-resume of the same thread. A `created` successor that matches
        // neither proof falls through to `decide_resume`.
        {
            let lm = thread.runtime.launch_metadata.as_ref();
            if continuation_shape(thread) {
                let launch_driver =
                    lm.and_then(|metadata| metadata.launch_driver)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "continuation successor {} has no admitted launch driver",
                                thread.thread_id
                            )
                        })?;
                let upstream_id = thread
                    .upstream_thread_id
                    .as_deref()
                    .expect("continuation_shape implies upstream is Some");
                let src = state.threads.get_thread(upstream_id)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "continuation successor {} references missing upstream {upstream_id}",
                        thread.thread_id
                    )
                })?;
                // A follow-resume successor has the same shape as a stranded
                // machine continuation (source `continued`, points back), but it
                // must NOT be auto-launched here — it waits for the followed
                // child's result and is driven by the follow-resume path. Leave
                // it pending. Fail closed: a marker-read error must not let a
                // follow-resume successor slip into machine launch.
                match state
                    .state_store
                    .is_follow_resume_successor(upstream_id, &thread.thread_id)
                {
                    Ok(true) => {
                        tracing::info!(
                            thread_id = %thread.thread_id,
                            "follow-resume successor — leaving pending, not a machine continuation"
                        );
                        continue;
                    }
                    Ok(false) => {}
                    Err(err) => return Err(err),
                }
                if is_machine_successor(thread, &src) {
                    // Autonomous continuation is always-on, bounded by the
                    // chain-depth cap enforced at create time (an unbounded
                    // chain can no longer form), so reconcile recovers a
                    // stranded machine successor unconditionally. Collect the
                    // intent only — `launch_successor` (post-listener) owns the
                    // lease claim AND the attempt budget.
                    let resume_context = lm
                        .and_then(|m| m.resume_context.clone())
                        .expect("continuation_shape implies resume_context is Some");
                    tracing::info!(
                        thread_id = %thread.thread_id,
                        "machine continuation successor stranded by crash — collecting launch intent"
                    );
                    intents.push(ResumeIntent {
                        thread_id: thread.thread_id.clone(),
                        chain_root_id: thread.chain_root_id.clone(),
                        resume_context,
                        launch_driver,
                        prior_status: thread.status.clone(),
                        kind: ResumeKind::Continuation,
                    });
                    reconciled += 1;
                    continue;
                }
                // OPERATOR follow-up stranded by a crash before its launch task
                // ran. NOT gated — accepted operator work must be recovered, not
                // lost. Confirm via the persisted request fingerprint (only
                // operator `create_or_get` records one) so we never mistake some
                // other completed→created lineage for a follow-up.
                if is_operator_successor(thread, &src)
                    && state
                        .state_store
                        .get_continuation_fingerprint(upstream_id)?
                        .is_some()
                {
                    let resume_context = lm
                        .and_then(|m| m.resume_context.clone())
                        .expect("continuation_shape implies resume_context is Some");
                    tracing::info!(
                        thread_id = %thread.thread_id,
                        "operator follow-up successor stranded by crash — collecting launch intent"
                    );
                    intents.push(ResumeIntent {
                        thread_id: thread.thread_id.clone(),
                        chain_root_id: thread.chain_root_id.clone(),
                        resume_context,
                        launch_driver,
                        prior_status: thread.status.clone(),
                        kind: ResumeKind::OperatorContinuation,
                    });
                    reconciled += 1;
                    continue;
                }
            }
        }

        // A follow CHILD row created but never launched (crash in the pre-launch
        // window) has no native_resume policy yet, so `decide_resume` below would
        // finalize it failed. Leave it to `reconcile_follow`, which relaunches it
        // fresh via `launch_follow_child`.
        if is_pending_follow_child(state, thread)? {
            tracing::info!(
                thread_id = %thread.thread_id,
                "follow child stranded pre-launch — leaving for reconcile_follow relaunch"
            );
            continue;
        }

        // A fresh root can crash after its atomic admitted birth but before a
        // process attaches. That is not a checkpoint resume and does not need
        // a native-resume declaration: it has never begun executing. Relaunch
        // only when the complete current capsule authority is present.
        if thread.status == ThreadStatus::Created.as_str()
            && thread.upstream_thread_id.is_none()
            && thread.runtime.pgid.is_none()
            && thread.admitted_launch_capsule_hash.is_some()
            && thread
                .runtime
                .launch_metadata
                .as_ref()
                .is_some_and(|metadata| {
                    metadata.resume_context.is_some()
                        && metadata.sealed_root_request.is_some()
                        && metadata.launch_driver.is_some()
                })
        {
            let launch_metadata = thread
                .runtime
                .launch_metadata
                .as_ref()
                .expect("pending admitted root requires launch metadata");
            let resume_context = launch_metadata
                .resume_context
                .clone()
                .expect("pending admitted root requires resume context");
            let launch_driver = launch_metadata
                .launch_driver
                .expect("pending admitted root requires launch driver");
            tracing::info!(
                thread_id = %thread.thread_id,
                "fresh admitted root stranded before first launch — collecting exact capsule intent"
            );
            intents.push(ResumeIntent {
                thread_id: thread.thread_id.clone(),
                chain_root_id: thread.chain_root_id.clone(),
                resume_context,
                launch_driver,
                prior_status: thread.status.clone(),
                kind: ResumeKind::AdmittedRoot,
            });
            reconciled += 1;
            continue;
        }

        // A launch-window QUEUED detached child is deliberately unlaunched —
        // its admission comes from the window kick/sweep, so it must not be
        // finalized as an interrupted spawn. (A launched-then-dead window
        // member falls through and finalizes; that terminal releases its
        // slot via the post-reconcile sweep.)
        if thread.status == ThreadStatus::Created.as_str()
            && state
                .state_store
                .launch_window_is_queued(&thread.chain_root_id)?
        {
            tracing::info!(
                thread_id = %thread.thread_id,
                "launch-window queued child — leaving for window admission"
            );
            continue;
        }

        let attempts = match state.state_store.get_resume_attempts(&thread.thread_id) {
            Ok(n) => n,
            Err(err) => {
                tracing::error!(
                    thread_id = %thread.thread_id,
                    error = %err,
                    "failed to read resume_attempts; finalizing as failed instead of resuming"
                );
                finalize_dead(
                    state,
                    &thread.thread_id,
                    pgid,
                    &thread.status,
                    Some(("resume_counter_io_error", json!({"error": err.to_string()}))),
                    &mut reconciled,
                )?;
                continue;
            }
        };
        let decision = decide_resume(thread.runtime.launch_metadata.as_ref(), attempts);

        match decision {
            ResumeDecision::NoResumePolicy => {
                let extra = if interrupted_spawn {
                    Some((
                        "interrupted_spawn",
                        json!({"reason": "no pid/pgid attached"}),
                    ))
                } else {
                    None
                };
                tracing::info!(
                    thread_id = %thread.thread_id,
                    kind = %thread.kind,
                    status = %thread.status,
                    pgid = ?pgid,
                    interrupted_spawn,
                    "dead process, no native_resume policy — finalizing failed"
                );
                finalize_dead(
                    state,
                    &thread.thread_id,
                    pgid,
                    &thread.status,
                    extra,
                    &mut reconciled,
                )?;
            }
            ResumeDecision::MissingResumeContext => {
                tracing::warn!(
                    thread_id = %thread.thread_id,
                    kind = %thread.kind,
                    status = %thread.status,
                    pgid = ?pgid,
                    "native_resume declared but no ResumeContext captured — \
                     cannot rebuild ExecutionParams; finalizing failed"
                );
                finalize_dead(
                    state,
                    &thread.thread_id,
                    pgid,
                    &thread.status,
                    Some((
                        "missing_resume_context",
                        json!({"reason": "no resume_context in launch_metadata"}),
                    )),
                    &mut reconciled,
                )?;
            }
            ResumeDecision::Exhausted { attempts, max } => {
                tracing::warn!(
                    thread_id = %thread.thread_id,
                    attempts,
                    max,
                    "native_resume retry budget exhausted — finalizing failed"
                );
                finalize_dead(
                    state,
                    &thread.thread_id,
                    pgid,
                    &thread.status,
                    Some((
                        "resume_exhausted",
                        json!({"attempts": attempts, "max": max}),
                    )),
                    &mut reconciled,
                )?;
            }
            ResumeDecision::Resume { next_attempt, max } => {
                // Persist the bumped counter BEFORE scheduling the
                // re-spawn so a crash mid-resume does not grant an
                // infinite retry loop. Source of truth lives in
                // runtime_db.thread_runtime.resume_attempts.
                if let Err(err) = state.state_store.bump_resume_attempts(&thread.thread_id) {
                    tracing::warn!(
                        thread_id = %thread.thread_id,
                        error = %err,
                        "failed to persist resume_attempts; finalizing instead of resuming"
                    );
                    finalize_dead(
                        state,
                        &thread.thread_id,
                        pgid,
                        &thread.status,
                        Some(("resume_counter_io_error", json!({"error": err.to_string()}))),
                        &mut reconciled,
                    )?;
                    continue;
                }
                // Pull the resume_context for the dispatcher.
                let resume_context = thread
                    .runtime
                    .launch_metadata
                    .as_ref()
                    .and_then(|m| m.resume_context.clone())
                    .expect("decide_resume==Resume implies resume_context is Some");
                let launch_driver = thread
                    .runtime
                    .launch_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.launch_driver)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "native-resume thread {} has no admitted launch driver",
                            thread.thread_id
                        )
                    })?;
                tracing::info!(
                    thread_id = %thread.thread_id,
                    attempt = next_attempt,
                    max,
                    "native_resume eligible — collecting ResumeIntent for post-listener dispatch"
                );
                intents.push(ResumeIntent {
                    thread_id: thread.thread_id.clone(),
                    chain_root_id: thread.chain_root_id.clone(),
                    resume_context,
                    launch_driver,
                    prior_status: thread.status.clone(),
                    kind: ResumeKind::NativeResume,
                });
                reconciled += 1;
            }
        }
    }

    tracing::info!(
        count = reconciled,
        resume_intents = intents.len(),
        "reconciled orphaned threads"
    );
    Ok(ActiveThreadReconcileReport {
        active_thread_ids,
        resume_intents: intents,
    })
}

fn repair_detached_spawn_links(state: &AppState) -> Result<()> {
    for intent in state.state_store.detached_spawn_intents()? {
        let child = match state.state_store.get_thread(&intent.child_thread_id)? {
            Some(child) => child,
            None => {
                let (Some(authority), Some(capsule_hash), Some(metadata), Some(initial_events)) = (
                    intent.child_project_authority.as_ref(),
                    intent.admitted_launch_capsule_hash.as_ref(),
                    intent.launch_metadata.as_ref(),
                    intent.initial_events.as_ref(),
                ) else {
                    if state
                        .state_store
                        .abort_unsealed_detached_spawn_intent(&intent.operation_id)?
                    {
                        tracing::warn!(
                            operation_id = %intent.operation_id,
                            "aborted detached reservation that never crossed its sealed authority boundary"
                        );
                    }
                    continue;
                };
                let resume = metadata.resume_context.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "sealed detached operation {} has no resume context",
                        intent.operation_id
                    )
                })?;
                let sealed = metadata.sealed_root_request.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "sealed detached operation {} has no sealed root request",
                        intent.operation_id
                    )
                })?;
                let expected_capsule_hash = metadata
                    .admitted_launch_capsule()?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "sealed detached operation {} has no admitted launch capsule",
                            intent.operation_id
                        )
                    })?
                    .content_hash()?;
                if capsule_hash != &expected_capsule_hash {
                    anyhow::bail!(
                        "sealed detached operation {} capsule drift: intent={}, metadata={}",
                        intent.operation_id,
                        capsule_hash,
                        expected_capsule_hash
                    );
                }
                let execution =
                    ryeos_executor::execution::runner::execution_params_from_sealed_root_request(
                        state,
                        &intent.child_thread_id,
                        resume,
                        sealed,
                        None,
                    )?;
                state
                    .threads
                    .create_root_thread_with_events_and_launch_metadata(
                        &intent.child_thread_id,
                        &execution.resolved,
                        authority.clone(),
                        initial_events.clone(),
                        Some(metadata),
                    )?;
                state
                    .state_store
                    .get_thread(&intent.child_thread_id)?
                    .ok_or_else(|| anyhow::anyhow!("repaired detached child birth disappeared"))?
            }
        };
        if child.project_authority.as_ref() != intent.child_project_authority.as_ref() {
            anyhow::bail!(
                "detached child {} project authority differs from its sealed intent",
                child.thread_id
            );
        }
        if state
            .state_store
            .admitted_launch_capsule_hash(&child.thread_id)?
            .as_deref()
            != intent.admitted_launch_capsule_hash.as_deref()
        {
            anyhow::bail!(
                "detached child {} admitted capsule differs from its sealed intent",
                child.thread_id
            );
        }
        let _inherited_stop = state.state_store.record_child_link(
            &intent.parent_thread_id,
            &intent.child_thread_id,
            "dispatch",
        )?;
        if child.status == ryeos_state::objects::ThreadStatus::Created.as_str()
            && child.runtime.process_identity.is_none()
            && state
                .state_store
                .get_launch_claim(&intent.child_thread_id)?
                .is_none()
            && !state
                .state_store
                .launch_window_is_cancelled(&intent.child_thread_id)?
        {
            let window = state
                .state_store
                .get_launch_metadata(&intent.child_thread_id)?
                .and_then(|metadata| metadata.follow_launch_window);
            if let Some(window) = window {
                state.state_store.launch_window_insert_only(
                    &intent.child_thread_id,
                    &window.key,
                    window.width,
                    lillux::time::timestamp_millis(),
                )?;
            } else {
                let _ = ryeos_executor::execution::launch::prepare_and_spawn_follow_child_recovery(
                    state.clone(),
                    &intent.child_thread_id,
                )?;
            }
        }
    }
    Ok(())
}

fn reconcile_execution_workspaces(
    state: &AppState,
    mode: ActiveReconcileMode,
) -> Result<BTreeSet<String>> {
    let mut blocked_freezes = BTreeSet::new();
    for workspace in state.state_store.open_execution_workspaces()? {
        let recorded_identity = workspace
            .process_identity
            .as_deref()
            .map(serde_json::from_str::<ryeos_app::process::ExecutionProcessIdentity>)
            .transpose()?;
        let workspace_claim = workspace
            .thread_id
            .as_deref()
            .map(|thread_id| state.state_store.get_launch_claim(thread_id))
            .transpose()?
            .flatten();
        let current_owner_active = workspace_claim.as_ref().is_some_and(|claim| {
            workspace.launch_owner.as_deref() == Some(claim.claimed_by.as_str())
                && state.state_store.is_launch_owner_active(&claim.claimed_by)
        });
        let owner_was_replaced = workspace_claim.as_ref().is_some_and(|claim| {
            workspace.launch_owner.as_deref() != Some(claim.claimed_by.as_str())
        });
        if mode == ActiveReconcileMode::Live && current_owner_active {
            // The live owner may be between journaling `freezing`, stopping its
            // process group, publishing the frozen generation, and resuming.
            // A periodic sweep must never steal that in-process saga.
            continue;
        }
        if workspace.state == WorkspaceState::Closing {
            if let Err(error) = cleanup_dead_execution_workspace(state, &workspace) {
                tracing::warn!(
                    workspace_id = %workspace.workspace_id,
                    %error,
                    "verified workspace destroy could not finish idempotent root/row cleanup"
                );
            }
            continue;
        }
        let runtime_identity = if workspace_claim.as_ref().is_some_and(|claim| {
            workspace.launch_owner.as_deref() == Some(claim.claimed_by.as_str())
        }) {
            workspace
                .thread_id
                .as_deref()
                .map(|thread_id| state.state_store.get_thread(thread_id))
                .transpose()?
                .flatten()
                .and_then(|thread| thread.runtime.process_identity)
        } else {
            None
        };
        let liveness = runtime_identity
            .as_ref()
            .or(recorded_identity.as_ref())
            .map(execution_group_liveness)
            .unwrap_or(IdentityLiveness::DeadOrStale);
        if workspace.state == WorkspaceState::Freezing {
            let nonterminal_owner = workspace
                .thread_id
                .as_deref()
                .map(|thread_id| state.state_store.get_thread(thread_id))
                .transpose()?
                .flatten()
                .is_some_and(|thread| matches!(thread.status.as_str(), "created" | "running"));
            if nonterminal_owner && !owner_was_replaced {
                let identity = runtime_identity.as_ref().or(recorded_identity.as_ref());
                let mut quiesced_members = Vec::new();
                if liveness == IdentityLiveness::Alive {
                    let identity = identity.expect("alive liveness implies process identity");
                    let stopped = if ryeos_app::process::signal_exact_group(identity, libc::SIGSTOP)
                        == ryeos_app::process::SignalResult::Delivered
                    {
                        ryeos_app::process::wait_for_exact_group_quiesced(
                            identity,
                            std::time::Duration::from_secs(2),
                        )
                    } else {
                        Err(anyhow::anyhow!(
                            "exact process-group stop was not delivered"
                        ))
                    };
                    let Ok(members) = stopped else {
                        if let Some(thread_id) = workspace.thread_id.as_ref() {
                            blocked_freezes.insert(thread_id.clone());
                        }
                        tracing::error!(
                            workspace_id = %workspace.workspace_id,
                            "interrupted callback freeze owner could not be quiesced; preserving upper layer"
                        );
                        continue;
                    };
                    quiesced_members = members;
                } else if liveness == IdentityLiveness::Unavailable {
                    if let Some(thread_id) = workspace.thread_id.as_ref() {
                        blocked_freezes.insert(thread_id.clone());
                    }
                    tracing::error!(
                        workspace_id = %workspace.workspace_id,
                        "interrupted callback freeze liveness is unavailable; preserving upper layer"
                    );
                    continue;
                }
                match ryeos_executor::execution::recover_interrupted_workspace_freeze(
                    state, &workspace,
                ) {
                    Ok(snapshot_hash) => {
                        if !quiesced_members.is_empty() {
                            ryeos_app::process::terminate_exact_processes(
                                &quiesced_members,
                                std::time::Duration::from_secs(2),
                            )?;
                        }
                        tracing::warn!(
                            workspace_id = %workspace.workspace_id,
                            %snapshot_hash,
                            "recovered interrupted callback freeze and terminated its exact orphan process set"
                        );
                    }
                    Err(error) => {
                        if let Some(thread_id) = workspace.thread_id.as_ref() {
                            blocked_freezes.insert(thread_id.clone());
                        }
                        tracing::error!(
                            workspace_id = %workspace.workspace_id,
                            %error,
                            "interrupted callback freeze could not be recovered; preserving upper layer"
                        );
                    }
                }
                continue;
            }
        }
        if liveness == IdentityLiveness::Alive {
            continue;
        }
        if liveness == IdentityLiveness::Unavailable {
            tracing::warn!(
                workspace_id = %workspace.workspace_id,
                "workspace process-group liveness is unavailable; leaving it quarantined in place"
            );
            continue;
        }
        if mode == ActiveReconcileMode::Live
            && workspace.thread_id.is_none()
            && workspace.launch_owner.is_none()
        {
            // Materialization is deliberately unbounded. An unbound reservation
            // may still be owned by the request that is constructing its lower,
            // and elapsed wall time is never sufficient proof of abandonment.
            // Startup runs only after the previous daemon generation is gone and
            // may safely reconcile these pre-bind rows.
            continue;
        }
        if workspace.state != WorkspaceState::Orphaned {
            if let (Some(thread_id), Some(launch_owner)) = (
                workspace.thread_id.as_deref(),
                workspace.launch_owner.as_deref(),
            ) {
                state
                    .state_store
                    .transition_abandoned_execution_workspace_owned(
                        &workspace.workspace_id,
                        thread_id,
                        launch_owner,
                        &[workspace.state],
                        WorkspaceState::Orphaned,
                    )?;
            } else {
                state.state_store.transition_execution_workspace(
                    &workspace.workspace_id,
                    &[workspace.state],
                    WorkspaceState::Orphaned,
                    None,
                )?;
            }
            tracing::warn!(
                workspace_id = %workspace.workspace_id,
                thread_id = workspace.thread_id.as_deref().unwrap_or(""),
                root_path = %workspace.root_path,
                "execution workspace owner is dead; quarantined for verified cleanup"
            );
        }
        if let Err(error) = cleanup_dead_execution_workspace(state, &workspace) {
            tracing::warn!(
                workspace_id = %workspace.workspace_id,
                %error,
                "dead execution workspace could not be owner-verified and removed; retaining quarantine"
            );
        }
    }
    Ok(blocked_freezes)
}

fn cleanup_dead_execution_workspace(
    state: &AppState,
    workspace: &ryeos_app::runtime_db::WorkspaceRecord,
) -> Result<()> {
    if workspace.state == WorkspaceState::Closing {
        remove_exact_workspace_root_if_present(std::path::Path::new(&workspace.root_path))?;
        if let (Some(thread_id), Some(launch_owner)) = (
            workspace.thread_id.as_deref(),
            workspace.launch_owner.as_deref(),
        ) {
            state
                .state_store
                .transition_abandoned_execution_workspace_owned(
                    &workspace.workspace_id,
                    thread_id,
                    launch_owner,
                    &[WorkspaceState::Closing],
                    WorkspaceState::Closed,
                )?;
        } else {
            state.state_store.transition_execution_workspace(
                &workspace.workspace_id,
                &[WorkspaceState::Closing],
                WorkspaceState::Closed,
                None,
            )?;
        }
        return Ok(());
    }
    if workspace.thread_id.is_none() && workspace.launch_owner.is_none() {
        state.state_store.transition_execution_workspace(
            &workspace.workspace_id,
            &[WorkspaceState::Orphaned],
            WorkspaceState::Closing,
            None,
        )?;
        remove_exact_workspace_root_if_present(std::path::Path::new(&workspace.root_path))?;
        state.state_store.transition_execution_workspace(
            &workspace.workspace_id,
            &[WorkspaceState::Closing],
            WorkspaceState::Closed,
            None,
        )?;
        return Ok(());
    }
    let thread_id = workspace
        .thread_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace has no bound thread owner"))?;
    let launch_owner = workspace
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace has no launch owner"))?;
    if let Some(claim) = state.state_store.get_launch_claim(thread_id)? {
        if claim.claimed_by == launch_owner
            && state.state_store.is_launch_owner_active(&claim.claimed_by)
        {
            anyhow::bail!("workspace launch owner is still active in this daemon");
        }
    }
    let root = std::path::PathBuf::from(&workspace.root_path);
    let layout = ryeos_executor::execution::workspace::WorkspaceLayout::from_root(root.clone());
    let (captured_backend_id, captured_backend_version) = state
        .isolation
        .workspace_backend_identity()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    if workspace.backend_id.as_deref() != Some(captured_backend_id)
        || workspace.backend_version.as_deref() != Some(captured_backend_version)
    {
        anyhow::bail!(
            "workspace selected backend differs from the daemon's exact captured adapter build"
        );
    }
    // A daemon can die after the backend selection is durable but before the
    // Create response is journaled. Create is deliberately idempotent for an
    // exact owner and pinned roots, so recovery re-drives it to recover the
    // identities required to prove a safe Destroy. No abandoned upper is ever
    // folded back.
    let recovered_create =
        if workspace.pinned_root_identities.is_none() || workspace.mount_identity.is_none() {
            if workspace.state != WorkspaceState::Constructing {
                anyhow::bail!("bound workspace is missing durable backend creation evidence");
            }
            Some(
                state
                    .isolation
                    .workspace_lifecycle(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
                        operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create,
                        workspace_id: &workspace.workspace_id,
                        launch_owner,
                        lower_snapshot: &workspace.lower_snapshot,
                        lower_path: &layout.lower,
                        upper_path: &layout.upper,
                        work_path: &layout.work,
                    })
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?,
            )
        } else {
            None
        };
    let response = state
        .isolation
        .workspace_lifecycle(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
            operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Destroy,
            workspace_id: &workspace.workspace_id,
            launch_owner,
            lower_snapshot: &workspace.lower_snapshot,
            lower_path: &layout.lower,
            upper_path: &layout.upper,
            work_path: &layout.work,
        })
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let pinned = lillux::canonical_json(&serde_json::to_value(&response.pinned_root_identities)?)?;
    let expected_pinned = match recovered_create.as_ref() {
        Some(created) => {
            lillux::canonical_json(&serde_json::to_value(&created.pinned_root_identities)?)?
        }
        None => workspace
            .pinned_root_identities
            .clone()
            .ok_or_else(|| anyhow::anyhow!("workspace has no pinned root identities"))?,
    };
    let expected_mount = recovered_create
        .as_ref()
        .map(|created| created.mount_identity.as_str())
        .or(workspace.mount_identity.as_deref())
        .ok_or_else(|| anyhow::anyhow!("workspace has no backend mount identity"))?;
    if workspace.backend_id.as_deref() != Some(response.backend_id.as_str())
        || workspace.backend_version.as_deref() != Some(response.backend_version.as_str())
        || expected_pinned != pinned
        || expected_mount != response.mount_identity
    {
        anyhow::bail!("workspace destroy evidence differs from its durable journal");
    }
    state
        .state_store
        .transition_abandoned_execution_workspace_owned(
            &workspace.workspace_id,
            thread_id,
            launch_owner,
            &[WorkspaceState::Orphaned],
            WorkspaceState::Closing,
        )?;
    remove_exact_workspace_root_if_present(&root)?;
    state
        .state_store
        .transition_abandoned_execution_workspace_owned(
            &workspace.workspace_id,
            thread_id,
            launch_owner,
            &[WorkspaceState::Closing],
            WorkspaceState::Closed,
        )?;
    Ok(())
}

fn remove_exact_workspace_root_if_present(root_path: &std::path::Path) -> Result<()> {
    let name = root_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("workspace root has no final component"))?;
    let parent_path = root_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("workspace root has no parent"))?;
    let Some(parent) = lillux::PinnedDirectory::open(parent_path)? else {
        return Ok(());
    };
    let Some(root) = parent.open_child_directory(name)? else {
        return Ok(());
    };
    root.remove_contents_recursive()?;
    if !parent.remove_empty_child_if_same(name, &root)? {
        anyhow::bail!("workspace root remained non-empty");
    }
    Ok(())
}

/// Process reconciliation owns executable threads and any row that carries
/// process/launch state. A daemon-owned non-execution root (for example an
/// operator seat) deliberately has neither: its subsystem owns liveness and
/// terminal settlement, so treating the absent pid as an interrupted spawn
/// would corrupt a healthy thread on every daemon restart.
fn is_daemon_owned_non_execution_thread(
    thread: &ThreadDetail,
    profile: Option<&ryeos_app::kind_profiles::ThreadKindProfile>,
) -> bool {
    profile.is_some_and(|profile| !profile.root_executable)
        && thread.runtime.pid.is_none()
        && thread.runtime.pgid.is_none()
        && thread.runtime.process_identity.is_none()
        && thread.runtime.launch_metadata.is_none()
}

/// A follow-recovery action collected at startup and driven post-listener (the
/// launch itself owns the claim, so a race with the live path is a benign skip).
#[derive(Debug, Clone)]
pub enum FollowReconcileAction {
    /// A waiter whose parent-resume successor is ready to launch (child terminal
    /// stored): `ready`/`resuming`. Driven by `launch_follow_resume_successor`.
    Resume { follow_key: String },
    /// A `waiting` waiter whose CHILD row was created but never launched (crash in
    /// the pre-launch window). Driven by `launch_follow_child(.., None, None)` — a
    /// fresh detached launch with no parent clamp (reconcile parity).
    RelaunchChild { child_thread_id: String },
}

/// Startup crash-recovery for daemon-managed follow.
///
/// - `ready`/`resuming`: the child terminal is stored but the parent resume never
///   ran (or was interrupted) → drive `launch_follow_resume_successor`.
/// - `waiting` + child row still `created`: the detached child launch never ran
///   (crash after `mark_follow_waiting`, before/into the spawn). `reconcile` proper
///   deliberately skips such a row (it has no native_resume policy, so it would
///   else be finalized failed) — relaunch it here via `launch_follow_child`. A
///   launched-then-crashed child (native_resume set) is recovered by `reconcile`'s
///   native resume instead, so it is left alone.
/// - `waiting` + child chain already terminal-but-unrecorded: the crash window
///   between finalize-persist and `record_follow_child_terminal`. Recovered via
///   `recover_terminal_follow_child` (degraded envelope) → `ready` → resume.
/// - `reserved`: a partial spawn (crash between `reserve_follow` and
///   `mark_follow_waiting`). Converged by `converge_reserved_follow` — left to the
///   parent's own native resume when the parent hasn't continued, converged to
///   `waiting` (+ successor adoption) when it has, or cleared if orphaned.
pub fn reconcile_follow(state: &AppState) -> Result<Vec<FollowReconcileAction>> {
    use ryeos_app::runtime_db::follow_phase;

    let now_ms = lillux::time::timestamp_millis();
    let mut actions: Vec<FollowReconcileAction> = Vec::new();
    for w in state.state_store.list_follow_waiters()? {
        let waiter_actions = match w.phase.as_str() {
            follow_phase::READY | follow_phase::RESUMING => {
                // A `resuming` waiter re-drives its idempotent parent resume every
                // pass; one that stays `resuming` across the staleness window is
                // stuck, not in flight — escalate to a loud log while still
                // re-driving (the resume is idempotent by design).
                if w.phase == follow_phase::RESUMING && follow_waiter_is_stale(&w, now_ms) {
                    tracing::warn!(
                        follow_key = %w.follow_key,
                        age_ms = now_ms.saturating_sub(w.updated_at_ms),
                        "follow waiter stuck in 'resuming' past the staleness window — \
                         re-driving parent resume (idempotent)"
                    );
                } else {
                    tracing::info!(
                        follow_key = %w.follow_key,
                        phase = %w.phase,
                        "follow waiter carries a stored child result — collecting parent-resume"
                    );
                }
                vec![FollowReconcileAction::Resume {
                    follow_key: w.follow_key.clone(),
                }]
            }
            follow_phase::WAITING => waiting_follow_action(state, &w)?,
            follow_phase::RESERVED => converge_reserved_follow(state, &w)?,
            other => anyhow::bail!(
                "follow waiter {} has unrecognized phase {other}",
                w.follow_key
            ),
        };
        match waiter_actions.is_empty() {
            false => actions.extend(waiter_actions),
            // Age-based escalation: a waiter that yields NO recovery action AND
            // whose issuing parent row is gone is an orphan whose lineage can
            // never be reconstructed. Clear it loudly once stale rather than
            // warn-skipping it forever (the table must converge to empty on an
            // idle daemon). A parent-present skip is left alone — it may still be
            // recoverable on a later pass.
            true => {
                if follow_waiter_is_stale(&w, now_ms)
                    && state.state_store.get_thread(&w.parent_thread_id)?.is_none()
                {
                    tracing::error!(
                        follow_key = %w.follow_key,
                        phase = %w.phase,
                        age_ms = now_ms.saturating_sub(w.updated_at_ms),
                        "stale follow waiter with a missing parent row and no recovery \
                         action — clearing orphan"
                    );
                    state.state_store.clear_follow_waiter(&w.follow_key)?;
                }
            }
        }
    }
    if !actions.is_empty() {
        tracing::info!(count = actions.len(), "collected follow reconcile actions");
    }
    Ok(actions)
}

/// A follow waiter older than this since its last update is a candidate for
/// age-based escalation. A live follow converges in seconds (child terminal →
/// parent resume); a waiter untouched for this long across reconcile passes is
/// stuck, not in flight.
const FOLLOW_WAITER_STALE_MS: i64 = 15 * 60 * 1000; // 15 minutes

fn follow_waiter_is_stale(w: &ryeos_app::runtime_db::FollowWaiter, now_ms: i64) -> bool {
    now_ms.saturating_sub(w.updated_at_ms) >= FOLLOW_WAITER_STALE_MS
}

/// The recovery action for a `waiting` follow waiter, shared by the `waiting` arm and
/// reserved-convergence: relaunch a child stranded pre-launch, or recover a child
/// chain that reached a terminal never recorded.
fn waiting_follow_action(
    state: &AppState,
    w: &ryeos_app::runtime_db::FollowWaiter,
) -> Result<Vec<FollowReconcileAction>> {
    if w.children.is_empty() {
        anyhow::bail!(
            "waiting follow waiter {} has no child thread recorded",
            w.follow_key
        );
    }
    let mut actions = Vec::new();
    let mut resume = false;
    for slot in &w.children {
        if slot.terminal_status.is_some() {
            continue;
        }
        let child_id = &slot.child_thread_id;
        match state.state_store.get_thread(child_id)? {
            // Pre-launch window: the child provably never launched.
            Some(child)
                if is_pending_follow_child(state, &child)?
                    && state
                        .state_store
                        .launch_window_is_queued(&slot.child_chain_root_id)? => {}
            Some(child) if is_pending_follow_child(state, &child)? => {
                if state
                    .state_store
                    .launch_window_is_cancelled(&slot.child_chain_root_id)?
                {
                    continue;
                }
                if state
                    .state_store
                    .launch_window_is_member(&slot.child_chain_root_id)?
                {
                    // A row that is not queued has already been admitted, but no
                    // process attached: re-drive its claim-guarded launch.
                    actions.push(FollowReconcileAction::RelaunchChild {
                        child_thread_id: child_id.clone(),
                    });
                    continue;
                }
                if let Some(window) = state
                    .state_store
                    .get_launch_metadata(child_id)?
                    .and_then(|m| m.follow_launch_window)
                {
                    let inserted = state.state_store.launch_window_insert_only(
                        &slot.child_chain_root_id,
                        &window.key,
                        window.width,
                        lillux::time::timestamp_millis(),
                    )?;
                    if inserted {
                        tracing::warn!(follow_key = %w.follow_key, child_thread_id = %child_id,
                        "repaired missing follow launch-window membership");
                    }
                    continue;
                }
                tracing::info!(
                    follow_key = %w.follow_key,
                    child_thread_id = %child_id,
                    "follow child stranded pre-launch — collecting relaunch"
                );
                actions.push(FollowReconcileAction::RelaunchChild {
                    child_thread_id: child_id.to_string(),
                });
            }
            // Launching / running are owned by native resume + the finalize kick. But a
            // NON-continued terminal never recorded (the crash window between finalize-
            // persist and record_follow_child_terminal, which reconcile skips) would hang
            // the parent forever — recover it.
            Some(_) => {
                if let Some(follow_key) = state
                    .threads
                    .recover_terminal_follow_child(&slot.child_chain_root_id)?
                {
                    tracing::info!(
                        follow_key = %follow_key,
                        "follow child chain terminal but unrecorded — recovered, collecting parent-resume"
                    );
                    resume = true;
                }
            }
            None => anyhow::bail!(
                "waiting follow waiter {} references missing child {child_id}",
                w.follow_key
            ),
        }
    }
    if resume {
        if let Some(reloaded) = state.state_store.get_follow_waiter_by_key(&w.follow_key)? {
            if matches!(
                reloaded.phase.as_str(),
                ryeos_app::runtime_db::follow_phase::READY
                    | ryeos_app::runtime_db::follow_phase::RESUMING
            ) {
                actions.push(FollowReconcileAction::Resume {
                    follow_key: w.follow_key.clone(),
                });
            }
        }
    }
    Ok(actions)
}

/// Converge a `reserved` follow waiter left by a partial spawn (crash between
/// `reserve_follow` and `mark_follow_waiting`). If the parent has NOT settled
/// `continued`, reconcile's native resume of the parent re-drives the idempotent
/// `spawn_follow_child` and converges it — leave it. If the parent HAS continued
/// (suspended past its follow node, so it will never re-drive), converge here: adopt
/// its follow-resume successor if unrecorded, commit the durable `waiting`/`ready`
/// phase, then immediately emit the matching recovery action. A continued parent
/// with no child row is unreconstructable corruption — clear the orphan.
fn converge_reserved_follow(
    state: &AppState,
    w: &ryeos_app::runtime_db::FollowWaiter,
) -> Result<Vec<FollowReconcileAction>> {
    let parent = state
        .state_store
        .get_thread(&w.parent_thread_id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "reserved follow waiter {} references missing parent {}",
                w.follow_key,
                w.parent_thread_id
            )
        })?;
    let parent_continued =
        w.parent_successor_thread_id.is_some() || parent.status == ThreadStatus::Continued.as_str();
    if !parent_continued {
        tracing::debug!(follow_key = %w.follow_key, "reserved follow waiter, parent not yet continued — parent resume re-drives");
        return Ok(Vec::new());
    }
    if w.children.len() != w.expected_children as usize
        || w.children
            .iter()
            .enumerate()
            .any(|(i, c)| c.item_index != i as u32)
    {
        tracing::warn!(follow_key = %w.follow_key, "reserved waiter: parent continued but no child recorded — clearing orphan");
        state.state_store.clear_follow_waiter(&w.follow_key)?;
        return Ok(Vec::new());
    }
    // Adopt the parent's follow-resume successor if step 3 created it but did not
    // record it on the waiter.
    if w.parent_successor_thread_id.is_none() {
        if let Some(successor_id) = parent.successor_thread_id {
            if state
                .state_store
                .is_follow_resume_successor(&w.parent_thread_id, &successor_id)?
            {
                state
                    .state_store
                    .set_follow_parent_successor(&w.follow_key, &successor_id)?;
            }
        }
    }
    let phase = state.state_store.mark_follow_waiting(&w.follow_key)?;
    if phase == ryeos_app::runtime_db::follow_phase::READY {
        tracing::info!(follow_key = %w.follow_key, "converged a stuck reserved follow waiter directly to ready");
        return Ok(vec![FollowReconcileAction::Resume {
            follow_key: w.follow_key.clone(),
        }]);
    }
    tracing::info!(follow_key = %w.follow_key, "converged a stuck reserved follow waiter to waiting");
    match state.state_store.get_follow_waiter_by_key(&w.follow_key)? {
        Some(updated) => waiting_follow_action(state, &updated),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::launch_metadata::ResumeContext;
    use ryeos_engine::contracts::{
        EffectivePrincipal, ExecutionHints, NativeResumeSpec, Principal, ProjectContext,
    };
    use std::path::PathBuf;

    fn principal() -> EffectivePrincipal {
        EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test".into(),
            scopes: vec!["execute".into()],
        })
    }

    fn ctx() -> ResumeContext {
        let project_context = ProjectContext::LocalPath {
            path: PathBuf::from("/tmp/p"),
        };
        let stable_project_identity = Some(
            ryeos_app::launch_metadata::StableProjectIdentity::from_path(
                std::path::Path::new("/tmp/p"),
                "site:a",
            )
            .unwrap(),
        );
        let local_overlay_root = Some(PathBuf::from("/tmp/p"));
        let project_root = PathBuf::from("/tmp/p");
        let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "site:a:/tmp/p".to_string(),
            Some(project_root.clone()),
            "a".repeat(64),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
                project_authority_id: lillux::sha256_hex(b"live-project\0site:a:/tmp/p\0/tmp/p"),
                source_identity: "dotenv:/tmp/p/.env".to_string(),
                include_operator_vault: true,
                name_authority: ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
            },
            Vec::new(),
        )
        .unwrap();
        ResumeContext {
            kind: "tool_run".into(),
            item_ref: "ns/foo".into(),
            ref_bindings: std::collections::BTreeMap::new(),
            launch_mode: "detached".into(),
            parameters: serde_json::json!({}),
            project_context,
            project_authority,
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity,
            local_overlay_root,
            original_snapshot_hash: Some("a".repeat(64)),
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:a".into(),
            origin_site_id: "site:a".into(),
            requested_by: principal(),
            execution_hints: ExecutionHints::default(),
            effective_caps: Vec::new(),
            parent_delegation_caps: None,
            executor_ref: None,
            runtime_ref: None,
        }
    }

    fn thread_detail(
        thread_id: &str,
        status: &str,
        upstream: Option<&str>,
        successor: Option<&str>,
        lm: Option<RuntimeLaunchMetadata>,
    ) -> ThreadDetail {
        ThreadDetail {
            thread_id: thread_id.into(),
            chain_root_id: "C".into(),
            kind: "directive".into(),
            status: status.into(),
            item_ref: "ns/foo".into(),
            executor_ref: "ex".into(),
            launch_mode: "detached".into(),
            current_site_id: "site:a".into(),
            origin_site_id: "site:a".into(),
            upstream_thread_id: upstream.map(Into::into),
            successor_thread_id: successor.map(Into::into),
            requested_by: None,
            project_root: None,
            project_authority: None,
            lifecycle_authority: None,
            admitted_launch_capsule_hash: None,
            created_at: "t".into(),
            updated_at: "t".into(),
            started_at: None,
            finished_at: None,
            runtime: ryeos_app::state_store::RuntimeInfo {
                pid: None,
                pgid: None,
                process_identity: None,
                process_dead_observed_at_ms: None,
                stop_requested_at_ms: None,
                stop_intent: None,
                launch_metadata: lm,
                recovery_wait: None,
            },
        }
    }

    fn native_resume_lm() -> RuntimeLaunchMetadata {
        RuntimeLaunchMetadata {
            native_resume: Some(NativeResumeSpec {
                checkpoint_interval_secs: 30,
                max_auto_resume_attempts: 1,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn daemon_owned_non_execution_thread_is_not_process_reconciled() {
        let mut seat = thread_detail("seat", "running", None, None, None);
        seat.kind = "seat_session".into();
        let non_execution = ryeos_app::kind_profiles::ThreadKindProfile {
            root_executable: false,
            supports_interrupt: false,
            supports_continuation: false,
            supports_operator_followup: false,
        };
        assert!(is_daemon_owned_non_execution_thread(
            &seat,
            Some(&non_execution)
        ));

        // Any launch metadata moves the row back under process recovery. This
        // fails closed if a supposedly daemon-owned kind ever starts carrying
        // executable lifecycle state.
        seat.runtime.launch_metadata = Some(RuntimeLaunchMetadata::default());
        assert!(!is_daemon_owned_non_execution_thread(
            &seat,
            Some(&non_execution)
        ));

        let executable = ryeos_app::kind_profiles::ThreadKindProfile {
            root_executable: true,
            ..non_execution
        };
        seat.runtime.launch_metadata = None;
        assert!(!is_daemon_owned_non_execution_thread(
            &seat,
            Some(&executable)
        ));
    }

    #[test]
    fn continuation_shape_matches_created_upstream_with_resume_context() {
        let lm = RuntimeLaunchMetadata::default().with_resume_context(ctx());
        let t = thread_detail("S", "created", Some("SRC"), None, Some(lm));
        assert!(continuation_shape(&t));
    }

    #[test]
    fn continuation_shape_rejects_running_or_missing_pieces() {
        let lm = || RuntimeLaunchMetadata::default().with_resume_context(ctx());
        // Already launched (running) — not a stranded `created` successor.
        assert!(!continuation_shape(&thread_detail(
            "S",
            "running",
            Some("SRC"),
            None,
            Some(lm())
        )));
        // No upstream link — a fresh root, not a continuation.
        assert!(!continuation_shape(&thread_detail(
            "S",
            "created",
            None,
            None,
            Some(lm())
        )));
        // No captured ResumeContext — not launchable as a continuation.
        assert!(!continuation_shape(&thread_detail(
            "S",
            "created",
            Some("SRC"),
            None,
            Some(RuntimeLaunchMetadata::default())
        )));
    }

    #[test]
    fn continuation_shape_matches_native_resume_successor() {
        // Lineage wins over checkpoint resume: a `created` successor that links
        // upstream and carries a ResumeContext IS a continuation even when its
        // runtime declares native_resume (the graph case). A new successor must be
        // launched as a continuation, not checkpoint-resumed as the same thread.
        let nr = native_resume_lm().with_resume_context(ctx());
        assert!(continuation_shape(&thread_detail(
            "S",
            "created",
            Some("SRC"),
            None,
            Some(nr)
        )));
        // But a RUNNING native_resume thread stays out — the `created` gate hands
        // crashed same-threads to the decide_resume (checkpoint) path.
        let nr_running = native_resume_lm().with_resume_context(ctx());
        assert!(!continuation_shape(&thread_detail(
            "S",
            "running",
            Some("SRC"),
            None,
            Some(nr_running)
        )));
    }

    #[test]
    fn continuation_shape_rejects_fresh_native_resume_root_with_context() {
        // Native-resume launches now capture a ResumeContext even on a FRESH root
        // (so a follow-resume successor can copy the parent's launch identity). A
        // fresh root has no upstream, so it must NOT be misclassified as a
        // continuation successor — it is checkpoint-resumed as the same thread.
        let nr = native_resume_lm().with_resume_context(ctx());
        assert!(!continuation_shape(&thread_detail(
            "R",
            "created",
            None,
            None,
            Some(nr)
        )));
    }

    #[test]
    fn is_machine_successor_true_when_source_continued_and_points_back() {
        let succ = thread_detail("S", "created", Some("SRC"), None, None);
        let src = thread_detail("SRC", "continued", None, Some("S"), None);
        assert!(is_machine_successor(&succ, &src));
    }

    #[test]
    fn is_machine_successor_false_for_operator_followup() {
        // An operator follow-up PRESERVES its source's completed/failed status, so
        // a same-shaped stranded operator successor is NOT a machine continuation
        // and must not be relaunched with suppressed stimulus.
        let succ = thread_detail("S", "created", Some("SRC"), None, None);
        let completed_src = thread_detail("SRC", "completed", None, Some("S"), None);
        assert!(!is_machine_successor(&succ, &completed_src));
        let failed_src = thread_detail("SRC", "failed", None, Some("S"), None);
        assert!(!is_machine_successor(&succ, &failed_src));
    }

    #[test]
    fn is_machine_successor_false_when_source_points_elsewhere() {
        // A `continued` source that links to a DIFFERENT successor is not proof
        // for this one (stale/mismatched lineage).
        let succ = thread_detail("S", "created", Some("SRC"), None, None);
        let src = thread_detail("SRC", "continued", None, Some("OTHER"), None);
        assert!(!is_machine_successor(&succ, &src));
    }

    #[test]
    fn is_operator_successor_true_for_terminal_source_pointing_back() {
        // An operator follow-up PRESERVES its source's completed/failed status.
        let succ = thread_detail("S", "created", Some("SRC"), None, None);
        for status in ["completed", "failed"] {
            let src = thread_detail("SRC", status, None, Some("S"), None);
            assert!(is_operator_successor(&succ, &src), "source {status}");
        }
        // Points at a different successor → not proof for this one.
        let other = thread_detail("SRC", "completed", None, Some("OTHER"), None);
        assert!(!is_operator_successor(&succ, &other));
    }

    #[test]
    fn machine_and_operator_classification_are_mutually_exclusive() {
        // The two recovery paths key off the SOURCE status: `continued` ⇒ machine,
        // `completed`/`failed` ⇒ operator. A given source can only be one.
        let succ = thread_detail("S", "created", Some("SRC"), None, None);
        let continued = thread_detail("SRC", "continued", None, Some("S"), None);
        assert!(is_machine_successor(&succ, &continued));
        assert!(!is_operator_successor(&succ, &continued));
        let completed = thread_detail("SRC", "completed", None, Some("S"), None);
        assert!(is_operator_successor(&succ, &completed));
        assert!(!is_machine_successor(&succ, &completed));
    }

    #[test]
    fn no_metadata_finalizes() {
        assert_eq!(decide_resume(None, 0), ResumeDecision::NoResumePolicy);
    }

    #[test]
    fn no_native_resume_finalizes() {
        let m = RuntimeLaunchMetadata::default();
        assert_eq!(decide_resume(Some(&m), 0), ResumeDecision::NoResumePolicy);
    }

    #[test]
    fn missing_resume_context_distinct_variant() {
        let m = RuntimeLaunchMetadata {
            native_resume: Some(NativeResumeSpec {
                checkpoint_interval_secs: 30,
                max_auto_resume_attempts: 1,
            }),
            ..Default::default()
        };
        assert_eq!(
            decide_resume(Some(&m), 0),
            ResumeDecision::MissingResumeContext
        );
    }

    #[test]
    fn within_budget_resumes() {
        let m = RuntimeLaunchMetadata {
            native_resume: Some(NativeResumeSpec {
                checkpoint_interval_secs: 30,
                max_auto_resume_attempts: 3,
            }),
            resume_context: Some(ctx()),
            ..Default::default()
        };
        assert_eq!(
            decide_resume(Some(&m), 0),
            ResumeDecision::Resume {
                next_attempt: 1,
                max: 3
            }
        );
        assert_eq!(
            decide_resume(Some(&m), 2),
            ResumeDecision::Resume {
                next_attempt: 3,
                max: 3
            }
        );
    }

    #[test]
    fn over_budget_exhausts() {
        let m = RuntimeLaunchMetadata {
            native_resume: Some(NativeResumeSpec {
                checkpoint_interval_secs: 30,
                max_auto_resume_attempts: 2,
            }),
            resume_context: Some(ctx()),
            ..Default::default()
        };
        assert_eq!(
            decide_resume(Some(&m), 2),
            ResumeDecision::Exhausted {
                attempts: 2,
                max: 2
            }
        );
        assert_eq!(
            decide_resume(Some(&m), 5),
            ResumeDecision::Exhausted {
                attempts: 5,
                max: 2
            }
        );
    }

    fn stale_test_waiter(updated_at_ms: i64) -> ryeos_app::runtime_db::FollowWaiter {
        ryeos_app::runtime_db::FollowWaiter {
            follow_key: "P/gr/n/1".into(),
            parent_thread_id: "P".into(),
            parent_chain_root_id: "C".into(),
            parent_successor_thread_id: None,
            follow_node: "n".into(),
            graph_run_id: "gr".into(),
            step_count: 1,
            frontier_id: None,
            fanout: false,
            expected_children: 1,
            child_project_authority: None,
            children: Vec::new(),
            phase: ryeos_app::runtime_db::follow_phase::WAITING.into(),
            created_at_ms: 0,
            updated_at_ms,
        }
    }

    #[test]
    fn follow_waiter_staleness_is_age_based() {
        let now = 100 * FOLLOW_WAITER_STALE_MS;
        // Fresh (updated moments ago) is never stale.
        assert!(!follow_waiter_is_stale(&stale_test_waiter(now - 1), now));
        // Exactly at the window and beyond is stale.
        assert!(follow_waiter_is_stale(
            &stale_test_waiter(now - FOLLOW_WAITER_STALE_MS),
            now
        ));
        assert!(follow_waiter_is_stale(&stale_test_waiter(0), now));
        // A clock skew (future update) is not stale — saturating avoids underflow.
        assert!(!follow_waiter_is_stale(
            &stale_test_waiter(now + 5_000),
            now
        ));
    }
}
