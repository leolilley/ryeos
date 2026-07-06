use anyhow::Result;
use serde_json::json;

use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
use ryeos_app::process::pgid_alive;
use ryeos_app::state::AppState;
use ryeos_app::state_store::ThreadDetail;
use ryeos_app::thread_lifecycle::ThreadFinalizeParams;
use ryeos_state::objects::ThreadStatus;

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
///   - it is some non-cleared waiter's child chain root.
/// A row failing any clause is left to the normal `reconcile` / native-resume /
/// finalize paths — never skipped here and never relaunched.
fn is_pending_follow_child(state: &AppState, thread: &ThreadDetail) -> bool {
    if thread.status != ThreadStatus::Created.as_str() {
        return false;
    }
    if thread.runtime.pgid.is_some() {
        return false;
    }
    if thread
        .runtime
        .launch_metadata
        .as_ref()
        .and_then(|m| m.native_resume.as_ref())
        .is_some()
    {
        return false;
    }
    matches!(
        state
            .state_store
            .get_follow_waiter_by_child_chain(&thread.chain_root_id),
        Ok(Some(w)) if w.child_thread_id.as_deref() == Some(thread.thread_id.as_str())
    )
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

/// Decision the reconciler makes for a single dead thread.
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
    /// Status of the thread row at reconcile time. The dispatcher uses
    /// this to decide whether to call `mark_running` after attaching
    /// the new spawn — `created` rows have never been transitioned,
    /// and `drain_running_threads` only sees `running` rows.
    pub prior_status: String,
    /// Which launch path to take post-listener.
    pub kind: ResumeKind,
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
) {
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
    if let Err(err) = state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: "failed".to_string(),
        outcome_code: Some(outcome_code.to_string()),
        result: None,
        error: Some(error_obj),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    }) {
        tracing::warn!(
            thread_id,
            error = %err,
            "failed to finalize thread"
        );
    } else {
        *reconciled += 1;
    }
}

/// Reconcile daemon state after restart.
///
/// Three phases:
/// 1. Catch up projection from CAS (repair any projection drift)
/// 2. Find threads left in non-terminal status whose processes are dead
///    (or whose spawn was interrupted before pid/pgid attach)
/// 3. For each: either finalize-as-failed, or collect a `ResumeIntent`
///    that the caller dispatches AFTER the daemon's listeners are bound
///
/// Returns the `ResumeIntent`s the caller must dispatch. The reconciler
/// itself never calls `run_existing_detached` — it would race the
/// daemon's listener startup, and a resumed subprocess making its first
/// callback before the UDS / HTTP server is bound would fail.
#[tracing::instrument(name = "state:reconcile", skip(state))]
pub async fn reconcile(state: &AppState) -> Result<Vec<ResumeIntent>> {
    // Catch up projection from CAS.
    let cas_root = state.state_store.cas_root()?;
    let refs_root = state.state_store.refs_root()?;

    state.state_store.with_projection(|projection| {
        let catch_up =
            ryeos_state::rebuild::catch_up_projection(projection, &cas_root, &refs_root)?;

        if catch_up.chains_updated > 0 {
            tracing::info!(
                chains_checked = catch_up.chains_checked,
                chains_updated = catch_up.chains_updated,
                threads_restored = catch_up.threads_restored,
                events_projected = catch_up.events_projected,
                "projection caught up from CAS"
            );
        }

        Ok::<_, anyhow::Error>(())
    })?;

    // Clear stale launch claims BEFORE collecting intents: a restart proves every
    // prior in-process launcher is gone, so any surviving (even unexpired) claim is
    // stale. Without this, a `created` successor whose launcher crashed after
    // claiming but before `mark_running` would have its reconcile relaunch blocked
    // by `AlreadyClaimed` until the lease expired — stranding accepted work.
    match state.state_store.clear_all_launch_claims() {
        Ok(n) if n > 0 => tracing::info!(cleared = n, "cleared stale launch claims at startup"),
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "failed to clear stale launch claims at startup"),
    }

    // Orphan thread cleanup.
    let running_threads = state
        .state_store
        .list_threads_by_status(&["created", "running"])?;

    if running_threads.is_empty() {
        tracing::debug!("no orphaned threads");
        return Ok(Vec::new());
    }

    tracing::info!(
        count = running_threads.len(),
        "checking non-terminal threads"
    );

    let mut reconciled = 0usize;
    let mut intents: Vec<ResumeIntent> = Vec::new();

    for thread in &running_threads {
        let pgid = thread.runtime.pgid;

        // A thread is "dead" when:
        //  - it has a pgid and that pgid is no longer alive, OR
        //  - it has NO pid/pgid: spawn was interrupted before attach
        //    (created/running but never made it to attach_process).
        // Both cases route through the same decide_resume path.
        let process_dead = match pgid {
            Some(p) => !pgid_alive(p),
            None => true,
        };

        if !process_dead {
            continue;
        }

        let interrupted_spawn = pgid.is_none();

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
                let upstream_id = thread
                    .upstream_thread_id
                    .as_deref()
                    .expect("continuation_shape implies upstream is Some");
                if let Ok(Some(src)) = state.threads.get_thread(upstream_id) {
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
                        Err(err) => {
                            tracing::warn!(
                                thread_id = %thread.thread_id,
                                error = %err,
                                "follow-resume marker read failed — leaving pending"
                            );
                            continue;
                        }
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
                            .get_continuation_fingerprint(upstream_id)
                            .ok()
                            .flatten()
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
                            prior_status: thread.status.clone(),
                            kind: ResumeKind::OperatorContinuation,
                        });
                        reconciled += 1;
                        continue;
                    }
                }
            }
        }

        // A follow CHILD row created but never launched (crash in the pre-launch
        // window) has no native_resume policy yet, so `decide_resume` below would
        // finalize it failed. Leave it to `reconcile_follow`, which relaunches it
        // fresh via `launch_follow_child`.
        if is_pending_follow_child(state, thread) {
            tracing::info!(
                thread_id = %thread.thread_id,
                "follow child stranded pre-launch — leaving for reconcile_follow relaunch"
            );
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
                .launch_window_is_queued(&thread.chain_root_id)
                .unwrap_or(false)
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
                );
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
                );
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
                );
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
                );
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
                    );
                    continue;
                }
                // Pull the resume_context for the dispatcher.
                let resume_context = thread
                    .runtime
                    .launch_metadata
                    .as_ref()
                    .and_then(|m| m.resume_context.clone())
                    .expect("decide_resume==Resume implies resume_context is Some");
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
    Ok(intents)
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
        let action = match w.phase.as_str() {
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
                Some(FollowReconcileAction::Resume {
                    follow_key: w.follow_key.clone(),
                })
            }
            follow_phase::WAITING => waiting_follow_action(state, &w)?,
            follow_phase::RESERVED => converge_reserved_follow(state, &w)?,
            other => {
                tracing::debug!(
                    follow_key = %w.follow_key,
                    phase = %other,
                    "follow waiter in an unrecognized phase — skipped"
                );
                None
            }
        };
        match action {
            Some(a) => actions.push(a),
            // Age-based escalation: a waiter that yields NO recovery action AND
            // whose issuing parent row is gone is an orphan whose lineage can
            // never be reconstructed. Clear it loudly once stale rather than
            // warn-skipping it forever (the table must converge to empty on an
            // idle daemon). A parent-present skip is left alone — it may still be
            // recoverable on a later pass.
            None => {
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
) -> Result<Option<FollowReconcileAction>> {
    let child_id = match w.child_thread_id.as_deref() {
        Some(id) => id,
        None => {
            tracing::warn!(follow_key = %w.follow_key, "waiting follow waiter has no child thread recorded");
            return Ok(None);
        }
    };
    match state.state_store.get_thread(child_id)? {
        // Pre-launch window: the child provably never launched.
        Some(child) if is_pending_follow_child(state, &child) => {
            tracing::info!(
                follow_key = %w.follow_key,
                child_thread_id = %child_id,
                "follow child stranded pre-launch — collecting relaunch"
            );
            Ok(Some(FollowReconcileAction::RelaunchChild {
                child_thread_id: child_id.to_string(),
            }))
        }
        // Launching / running are owned by native resume + the finalize kick. But a
        // NON-continued terminal never recorded (the crash window between finalize-
        // persist and record_follow_child_terminal, which reconcile skips) would hang
        // the parent forever — recover it.
        Some(_) => {
            let chain_root = w.child_chain_root_id.as_deref().unwrap_or(child_id);
            match state.threads.recover_terminal_follow_child(chain_root)? {
                Some(follow_key) => {
                    tracing::info!(
                        follow_key = %follow_key,
                        "follow child chain terminal but unrecorded — recovered, collecting parent-resume"
                    );
                    Ok(Some(FollowReconcileAction::Resume { follow_key }))
                }
                None => Ok(None),
            }
        }
        None => {
            tracing::warn!(follow_key = %w.follow_key, child_thread_id = %child_id, "waiting follow waiter's child row is missing");
            Ok(None)
        }
    }
}

/// Converge a `reserved` follow waiter left by a partial spawn (crash between
/// `reserve_follow` and `mark_follow_waiting`). If the parent has NOT settled
/// `continued`, reconcile's native resume of the parent re-drives the idempotent
/// `spawn_follow_child` and converges it — leave it. If the parent HAS continued
/// (suspended past its follow node, so it will never re-drive), converge here: adopt
/// its follow-resume successor if unrecorded, mark `waiting`, then apply the waiting
/// recovery. A continued parent with no child row is unreconstructable corruption —
/// clear the orphan.
fn converge_reserved_follow(
    state: &AppState,
    w: &ryeos_app::runtime_db::FollowWaiter,
) -> Result<Option<FollowReconcileAction>> {
    let parent_continued = w.parent_successor_thread_id.is_some()
        || matches!(
            state.state_store.get_thread(&w.parent_thread_id),
            Ok(Some(p)) if p.status == ThreadStatus::Continued.as_str()
        );
    if !parent_continued {
        tracing::debug!(follow_key = %w.follow_key, "reserved follow waiter, parent not yet continued — parent resume re-drives");
        return Ok(None);
    }
    if w.child_thread_id.is_none() {
        tracing::warn!(follow_key = %w.follow_key, "reserved waiter: parent continued but no child recorded — clearing orphan");
        let _ = state.state_store.clear_follow_waiter(&w.follow_key);
        return Ok(None);
    }
    // Adopt the parent's follow-resume successor if step 3 created it but did not
    // record it on the waiter.
    if w.parent_successor_thread_id.is_none() {
        if let Ok(Some(parent)) = state.state_store.get_thread(&w.parent_thread_id) {
            if let Some(succ) = parent.successor_thread_id {
                if state
                    .state_store
                    .is_follow_resume_successor(&w.parent_thread_id, &succ)
                    .unwrap_or(false)
                {
                    let _ = state
                        .state_store
                        .set_follow_parent_successor(&w.follow_key, &succ);
                }
            }
        }
    }
    if let Err(e) = state.state_store.mark_follow_waiting(&w.follow_key) {
        tracing::warn!(follow_key = %w.follow_key, error = %e, "reserved waiter: could not converge to waiting (missing child/successor) — leaving");
        return Ok(None);
    }
    tracing::info!(follow_key = %w.follow_key, "converged a stuck reserved follow waiter to waiting");
    match state.state_store.get_follow_waiter_by_key(&w.follow_key)? {
        Some(updated) => waiting_follow_action(state, &updated),
        None => Ok(None),
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
        ResumeContext {
            kind: "tool_run".into(),
            item_ref: "ns/foo".into(),
            launch_mode: "detached".into(),
            parameters: serde_json::json!({}),
            project_context: ProjectContext::LocalPath {
                path: PathBuf::from("/tmp/p"),
            },
            original_snapshot_hash: None,
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:a".into(),
            origin_site_id: "site:a".into(),
            requested_by: principal(),
            execution_hints: ExecutionHints::default(),
            effective_caps: Vec::new(),
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
            created_at: "t".into(),
            updated_at: "t".into(),
            started_at: None,
            finished_at: None,
            runtime: ryeos_app::state_store::RuntimeInfo {
                pid: None,
                pgid: None,
                launch_metadata: lm,
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
            child_thread_id: None,
            child_chain_root_id: None,
            child_terminal_thread_id: None,
            child_terminal_status: None,
            terminal_envelope: None,
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
