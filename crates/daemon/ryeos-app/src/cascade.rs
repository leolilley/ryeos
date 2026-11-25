//! Cancel/kill cascade: signal a thread's live descendants so a cancelled or
//! killed parent cannot keep running work through a child it spawned (the
//! clobber where a "cancelled" parent keeps authoring via a live inline child).
//!
//! Lineage comes from the runtime child-link table
//! ([`StateStore::descendant_thread_ids`]); each descendant's current durable
//! process identity is resolved from `thread_runtime` at stop time. A
//! descendant without an attached identity is skipped. Each stop is proven
//! (signal, grace, group kill, group observed empty); a killed child's own
//! launcher, blocked waiting on the subprocess, observes the exit and
//! finalizes the row. Only [`settle_stopped_descendants`] finalizes a row on
//! its behalf, and only after the stop is proven.

use std::collections::BTreeSet;

use std::time::Duration;

use anyhow::Context as _;
use serde_json::{Value, json};

use ryeos_engine::contracts::CancellationMode;

use crate::process::{ShutdownAction, kill_by_action, resolve_shutdown_action};
use crate::runtime_db::StopOrigin;
use crate::state::AppState;
use crate::state_store::StateStore;
use crate::thread_lifecycle::ThreadFinalizeParams;

/// How hard to stop a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CascadeMode {
    /// SIGKILL immediately (a `kill`).
    Hard,
    /// SIGTERM, the launch's grace, then SIGKILL of the group — a `cancel`.
    Graceful,
}

/// The shutdown for `mode`. Every stop is proven: the target is signalled, a
/// graceful stop gets the launch's grace so the runtime can cancel
/// cooperatively at its own boundary, and the pinned process group (the target's
/// tools and helpers) is then killed; the stop counts only once the group is
/// observed empty. `kill` is SIGKILL at once, as is a `cancel` whose recorded
/// stop policy is `Hard`.
fn stop_action(mode: CascadeMode, cancellation_mode: Option<CancellationMode>) -> ShutdownAction {
    match mode {
        CascadeMode::Hard => ShutdownAction::Hard,
        CascadeMode::Graceful => resolve_shutdown_action(cancellation_mode),
    }
}

/// Signal every live descendant of `root_thread_id` per `mode`, resolving each
/// child's current durable process identity at signal time. Returns a per-child report for the
/// caller's response/log. A store error on the descendant walk propagates; a
/// single child's missing pgid or failed signal is recorded in the report, not
/// raised — one unreachable child must not abort the cascade.
pub fn cascade_descendants(
    state: &AppState,
    root_thread_id: &str,
    mode: CascadeMode,
) -> anyhow::Result<Vec<Value>> {
    const MAX_FIXED_POINT_PASSES: usize = 16;
    let mut seen = BTreeSet::new();
    let mut report = Vec::new();
    for _ in 0..MAX_FIXED_POINT_PASSES {
        let descendants = state.state_store.descendant_thread_ids(root_thread_id)?;
        let new_children = descendants
            .into_iter()
            .filter(|child| seen.insert(child.clone()))
            .collect::<Vec<_>>();
        if new_children.is_empty() {
            if report.iter().any(descendant_stop_unproven) {
                anyhow::bail!("descendant stop or scoped-child settlement unproven: {report:?}");
            }
            return Ok(report);
        }
        // Descendants stop in parallel, so the wait is the longest grace, not
        // their sum.
        let pass = std::thread::scope(|scope| {
            let handles = new_children
                .iter()
                .map(|child| scope.spawn(move || signal_descendant(state, child, mode)))
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .zip(&new_children)
                .map(|(handle, child)| {
                    handle.join().unwrap_or_else(|_| {
                        json!({ "thread_id": child, "result": STOP_UNPROVEN, "error": "stop worker panicked" })
                    })
                })
                .collect::<Vec<_>>()
        });
        report.extend(pass);
    }
    anyhow::bail!(
        "descendant stop did not reach a fixed point after {MAX_FIXED_POINT_PASSES} passes for {root_thread_id}: {report:?}"
    )
}

/// Result marker for a stop whose process group was not proved gone.
const STOP_UNPROVEN: &str = "unproven";

/// Every thread of `thread_id`'s chain (continuation segments), including
/// `thread_id` itself. A follow child is linked to whichever segment suspended
/// when it was admitted, so the whole run's lineage is the union of these
/// segments' descendants.
pub fn chain_segment_ids(state: &AppState, thread_id: &str) -> anyhow::Result<Vec<String>> {
    chain_segment_ids_in(&state.state_store, thread_id)
}

/// [`chain_segment_ids`] over a bare store, for callers that hold no
/// `AppState`.
fn chain_segment_ids_in(store: &StateStore, thread_id: &str) -> anyhow::Result<Vec<String>> {
    let Some(thread) = store.get_thread(thread_id)? else {
        return Ok(vec![thread_id.to_string()]);
    };
    let mut ids: Vec<String> = store
        .list_chain_threads(&thread.chain_root_id)?
        .into_iter()
        .map(|segment| segment.thread_id)
        .collect();
    if !ids.iter().any(|id| id == thread_id) {
        ids.push(thread_id.to_string());
    }
    Ok(ids)
}

/// Wait until every descendant of `root_thread_id` has a terminal row. A
/// descendant whose process is already proven gone (the caller ran a settled
/// cascade) but whose launcher has not finalized it within `wait` is finalized
/// here with the stop intent's status, so no row outlives its stop. A
/// descendant that cannot be finalized fails the call.
pub fn settle_stopped_descendants(
    state: &AppState,
    root_thread_id: &str,
    mode: CascadeMode,
    wait: Duration,
) -> anyhow::Result<()> {
    let status = match mode {
        CascadeMode::Graceful => "cancelled",
        CascadeMode::Hard => "killed",
    };
    let deadline = std::time::Instant::now() + wait;
    loop {
        let mut pending = Vec::new();
        for id in state.state_store.descendant_thread_ids(root_thread_id)? {
            if let Some(thread) = state.state_store.get_thread(&id)?
                && !crate::state_store::is_terminal_status(&thread.status)
            {
                pending.push(thread);
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        for thread in pending {
            let finalized = state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread.thread_id.clone(),
                status: status.to_string(),
                outcome_code: Some(status.to_string()),
                result: None,
                error: Some(json!({"reason": "ancestor_stopped"})),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            });
            if let Err(error) = finalized {
                let terminal = state
                    .state_store
                    .get_thread(&thread.thread_id)?
                    .is_some_and(|current| crate::state_store::is_terminal_status(&current.status));
                if !terminal {
                    return Err(error).with_context(|| {
                        format!("settle stopped descendant {}", thread.thread_id)
                    });
                }
            }
        }
        return Ok(());
    }
}

fn scoped_child_settlement_failed(report: &Value) -> bool {
    report
        .get("scoped_child_settlement")
        .and_then(Value::as_str)
        == Some("failed")
}

fn descendant_stop_unproven(report: &Value) -> bool {
    scoped_child_settlement_failed(report)
        || report.get("result").and_then(Value::as_str) == Some(STOP_UNPROVEN)
        || matches!(
            report.get("skipped").and_then(Value::as_str),
            Some(
                "stop_tombstone_failed"
                    | "read_error"
                    | "no_thread_row"
                    | "in_process_handler_running"
            )
        )
}

/// Cancel durable descendants that have not yet been admitted. Membership is
/// tombstoned as one store operation before lifecycle finalization, preventing
/// admission across crashes (and deliberately admitting no replacements).
pub fn cancel_queued_descendants(
    state: &AppState,
    root_thread_id: &str,
) -> anyhow::Result<Vec<String>> {
    let descendants = state.state_store.descendant_thread_ids(root_thread_id)?;
    // Admission is only a durable marker, not proof of a spawn. Include admitted
    // members only while the authoritative row is still created and unattached.
    let mut cancellable = Vec::new();
    for root in descendants {
        let Some(thread) = state.state_store.get_thread(&root)? else {
            continue;
        };
        // A durable process identity proves the launch crossed attachment,
        // even if its target has exited while Lillux still owns the retained
        // wrapper/group cleanup. Never reclassify such a row as unspawned.
        if thread.status == "created" && thread.runtime.process_identity.is_none() {
            cancellable.push(root);
        }
    }
    let removed = state
        .state_store
        .launch_window_cancel_members(&cancellable, lillux::time::timestamp_millis())?;
    for chain_root in &removed {
        let Some(thread) = state.state_store.get_thread(chain_root)? else {
            state.state_store.discard_window_member(chain_root)?;
            continue;
        };
        let launch_in_flight = state
            .state_store
            .get_launch_claim(&thread.thread_id)?
            .is_some_and(|claim| claim.lease_expires_at_ms > lillux::time::timestamp_millis());
        if launch_in_flight {
            // The launcher owns the sole spawn authorization. Keep the tombstone
            // durable and let attach_process observe it; never finalize a row
            // while that launcher may be between spawn and ledger attachment.
            continue;
        }
        if !crate::state_store::is_terminal_status(&thread.status) {
            let (outcome_code, reason) = unlaunched_cancel_outcome(state, &thread.thread_id)?;
            state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread.thread_id,
                status: "cancelled".to_string(),
                outcome_code: Some(outcome_code.to_string()),
                result: None,
                error: Some(json!({ "reason": reason })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            })?;
        }
        state.state_store.discard_window_member(chain_root)?;
    }
    Ok(removed)
}

/// The outcome code and error reason for cancelling a never-launched
/// descendant because an ancestor was cancelled. A flagged `dispatch` child is
/// cancelled because its session ended, and says so; any other descendant keeps
/// the plain cancel outcome.
fn unlaunched_cancel_outcome(
    state: &AppState,
    thread_id: &str,
) -> anyhow::Result<(&'static str, &'static str)> {
    Ok(
        if state
            .state_store
            .flagged_dispatch_parent(thread_id)?
            .is_some()
        {
            (PARENT_END_OUTCOME_CODE, PARENT_END_REASON)
        } else {
            ("cancelled", "ancestor_cancelled_before_launch")
        },
    )
}

pub fn repair_cancelled_window_members(state: &AppState) -> anyhow::Result<()> {
    for root in state.state_store.list_cancelled_window_members()? {
        let Some(thread) = state.state_store.get_thread(&root)? else {
            state.state_store.discard_window_member(&root)?;
            continue;
        };
        // Never settle a process-bearing descendant here: it must remain on the
        // normal signal cascade and let its launcher observe process termination.
        if !crate::state_store::is_terminal_status(&thread.status)
            && thread.status == "created"
            && thread.runtime.process_identity.is_none()
        {
            let (outcome_code, reason) = unlaunched_cancel_outcome(state, &thread.thread_id)?;
            state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread.thread_id.clone(),
                status: "cancelled".into(),
                outcome_code: Some(outcome_code.to_string()),
                result: None,
                error: Some(json!({ "reason": reason })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            })?;
        }
        if crate::state_store::is_terminal_status(&thread.status) || thread.status == "created" {
            state.state_store.discard_window_member(&root)?;
        }
    }
    Ok(())
}

/// Signal one thread's verified execution identity per `mode`, resolving it at
/// signal time. Returns a report value and NEVER errors: a missing/unreadable
/// row or an absent/incomplete identity yields a skip marker, so one unreachable
/// thread never aborts a wider cascade. Stop intent is tombstoned before the
/// identity decision, closing the created→attach race for every caller.
///
/// A terminal row is still signalled when it retains an attached identity: a
/// managed runtime may self-finalize before the outer owned wrapper has exited.
pub fn signal_thread(state: &AppState, thread_id: &str, mode: CascadeMode) -> Value {
    signal_thread_with_origin(state, thread_id, mode, StopOrigin::Requested)
}

/// As [`signal_thread`], recording who wrote the stop tombstone. The first
/// writer's origin is kept.
fn signal_thread_with_origin(
    state: &AppState,
    thread_id: &str,
    mode: CascadeMode,
    origin: StopOrigin,
) -> Value {
    let store = &state.state_store;
    let thread = match store.get_thread(thread_id) {
        Ok(Some(thread)) => thread,
        Ok(None) => return json!({ "thread_id": thread_id, "skipped": "no_thread_row" }),
        Err(e) => {
            return json!({ "thread_id": thread_id, "skipped": "read_error", "error": format!("{e:#}") });
        }
    };
    let stop_intent = match mode {
        CascadeMode::Graceful => crate::state_store::StopIntent::Cancel,
        CascadeMode::Hard => crate::state_store::StopIntent::Kill,
    };
    let runtime = match store.request_thread_stop_with_origin(thread_id, stop_intent, origin) {
        Ok(runtime) => runtime,
        Err(error) => {
            return json!({
                "thread_id": thread_id,
                "skipped": "stop_tombstone_failed",
                "error": format!("{error:#}"),
            });
        }
    };
    let mut report = if crate::state_store::is_terminal_status(&thread.status)
        && runtime.process_identity.is_none()
    {
        json!({ "thread_id": thread_id, "skipped": "terminal", "status": thread.status })
    } else if let Some(pgid) = runtime.pgid.filter(|pgid| *pgid > 0) {
        if let Some(identity) = runtime.process_identity.as_ref() {
            let cancellation_mode = runtime
                .launch_metadata
                .as_ref()
                .and_then(|lm| lm.cancellation_mode);
            let effective_mode = match runtime.stop_intent {
                Some(crate::state_store::StopIntent::Kill) => CascadeMode::Hard,
                Some(crate::state_store::StopIntent::Cancel) => CascadeMode::Graceful,
                None => mode,
            };
            let action = stop_action(effective_mode, cancellation_mode);
            let stopped = kill_by_action(identity, action);
            json!({
                "thread_id": thread_id,
                "pgid": pgid,
                "action": format!("{action:?}"),
                "method": stopped.method,
                "result": if stopped.success { "stopped" } else { STOP_UNPROVEN },
            })
        } else {
            json!({ "thread_id": thread_id, "pgid": pgid, "skipped": "no_process_identity" })
        }
    } else {
        json!({
            "thread_id": thread_id,
            "skipped": if runtime.pgid.is_some() { "invalid_pgid" } else { "no_pgid" },
        })
    };
    match crate::scoped_producer_stop::stop_scoped_producer_for_root(
        state,
        thread_id,
        Duration::from_secs(60),
    ) {
        Ok(()) => report["scoped_child_settlement"] = json!("settled"),
        Err(error) => {
            report["scoped_child_settlement"] = json!("failed");
            report["scoped_child_error"] = json!(format!("{error:#}"));
        }
    }
    report
}

/// Signal one descendant of a cascade. An inline in-process child is a
/// descendant like any other thread in its parent's chain, but its handler is
/// a daemon-owned task with no cancellation contract, so the store refuses a
/// stop request on its row and there is nothing to signal. Its stop is
/// reported from its own row instead: a terminal handler is a proven stop, a
/// running one is not. A direct stop of such a thread as the cascade target
/// still goes through `signal_thread` and keeps the store's refusal.
fn signal_descendant(state: &AppState, thread_id: &str, mode: CascadeMode) -> Value {
    let thread = match state.state_store.get_thread(thread_id) {
        Ok(Some(thread)) => thread,
        Ok(None) => return json!({ "thread_id": thread_id, "skipped": "no_thread_row" }),
        Err(e) => {
            return json!({ "thread_id": thread_id, "skipped": "read_error", "error": format!("{e:#}") });
        }
    };
    let in_process_handler = thread
        .runtime
        .launch_metadata
        .as_ref()
        .and_then(|metadata| metadata.launch_driver)
        == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler);
    if !in_process_handler {
        // A flagged child reached by a cascade is stopped because the session
        // above it is being stopped: the same parent-end stop the sweep
        // writes. Any other descendant keeps the plain stop.
        let origin = match state.state_store.flagged_dispatch_parent(thread_id) {
            Ok(Some(_)) => StopOrigin::ParentEnd,
            Ok(None) | Err(_) => StopOrigin::Requested,
        };
        return signal_thread_with_origin(state, thread_id, mode, origin);
    }
    in_process_descendant_report(thread_id, &thread.status)
}

/// The cascade report for an in-process handler descendant at `status`.
/// Terminal: the stop is proven by the row itself. Not terminal: the
/// handler is still running with no cancellation contract, so the stop is
/// unproven and the cascade says so.
fn in_process_descendant_report(thread_id: &str, status: &str) -> Value {
    if crate::state_store::is_terminal_status(status) {
        json!({
            "thread_id": thread_id,
            "in_process_handler": "terminal",
            "status": status,
        })
    } else {
        json!({
            "thread_id": thread_id,
            "skipped": "in_process_handler_running",
            "status": status,
        })
    }
}

/// Stop a thread and its live descendants: signal the target per `mode`, then
/// cascade to every live descendant. `cancel` → graceful (SIGTERM, which both
/// runtimes now honour cooperatively); `kill` → hard SIGKILL. The single shared
/// entry point for `commands.submit` and `runtime.submit_command`, so both the
/// operator and runtime control paths stop the target itself — not only its
/// children.
pub fn stop_thread_and_descendants(
    state: &AppState,
    thread_id: &str,
    mode: CascadeMode,
) -> anyhow::Result<(Value, Vec<String>)> {
    stop_thread_and_descendants_with_origin(state, thread_id, mode, StopOrigin::Requested)
}

/// As [`stop_thread_and_descendants`], recording who wrote the target's stop.
/// The parent-end sweep is the only caller that states
/// [`StopOrigin::ParentEnd`] for a target; descendants take their origin from
/// their own link.
fn stop_thread_and_descendants_with_origin(
    state: &AppState,
    thread_id: &str,
    mode: CascadeMode,
    origin: StopOrigin,
) -> anyhow::Result<(Value, Vec<String>)> {
    let queued = cancel_queued_descendants(state, thread_id)?;
    let target = signal_thread_with_origin(state, thread_id, mode, origin);
    let descendants = cascade_descendants(state, thread_id, mode)?;
    if descendant_stop_unproven(&target) {
        anyhow::bail!("target stop or scoped-child settlement unproven for {thread_id}: {target}");
    }
    Ok((
        json!({ "target": target, "descendants": descendants, "queued": queued }),
        queued,
    ))
}

/// Outcome code of a child stopped because the session that spawned it ended.
/// The child finalizes as stopped (cancelled or killed per its stop intent),
/// never as failed, whether the stop was graceful or forced.
pub const PARENT_END_OUTCOME_CODE: &str = "cancelled_by_parent_end";

/// Error reason recorded on a child stopped because its session ended.
pub const PARENT_END_REASON: &str = "parent_session_ended";

/// Grace a flagged child gets to stop cooperatively before it is forced, when
/// the node does not state one.
pub const PARENT_END_GRACE_DEFAULT: Duration = Duration::from_secs(30);

/// Hard ceiling on the grace; a larger configured value is clamped to it.
pub const PARENT_END_GRACE_MAX: Duration = Duration::from_secs(300);

/// The grace to apply for a configured value: the default when none is
/// configured, otherwise the value clamped to [`PARENT_END_GRACE_MAX`].
pub fn parent_end_grace(configured: Option<Duration>) -> Duration {
    configured.map_or(PARENT_END_GRACE_DEFAULT, |grace| {
        grace.min(PARENT_END_GRACE_MAX)
    })
}

/// How long a stopped child's row is given to reach a terminal on its own
/// before it is finalized on its behalf: the grace, at most this long.
const PARENT_END_SETTLE_WAIT_MAX: Duration = Duration::from_secs(5);

/// Poll interval while waiting for a stopped child to reach a terminal.
const PARENT_END_POLL: Duration = Duration::from_millis(50);

/// Links read per page when sweeping for children of ended parents.
const PARENT_END_SWEEP_BATCH: usize = 256;

/// What a parent-end stop did, per flagged child.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParentEndStopReport {
    /// Ended parents the report covers (a sweep may cover several).
    pub parents: usize,
    /// Children this call saw reach a stopped terminal.
    pub stopped: Vec<String>,
    /// Children that were already terminal and were left untouched.
    pub already_terminal: Vec<String>,
    /// Children that could not be stopped, with the reason. Each stays
    /// non-terminal (or unproven) and is retried by the next sweep.
    pub failed: Vec<(String, String)>,
}

impl ParentEndStopReport {
    fn absorb(&mut self, other: ParentEndStopReport) {
        self.parents += other.parents;
        self.stopped.extend(other.stopped);
        self.already_terminal.extend(other.already_terminal);
        self.failed.extend(other.failed);
    }
}

/// Whether the session chain `thread_id` belongs to has ended: every segment
/// is terminal and at least one is not a `continued` hand-off, so the tail
/// segment is over and has no successor. A chain whose tail is still live, or
/// whose last segment is `continued` with no successor yet, has not ended. A
/// thread with no row is not an ended session.
pub fn session_chain_has_ended(store: &StateStore, thread_id: &str) -> anyhow::Result<bool> {
    if store.get_thread(thread_id)?.is_none() {
        return Ok(false);
    }
    let mut tail_ended = false;
    for segment in chain_segment_ids_in(store, thread_id)? {
        let Some(row) = store.get_thread(&segment)? else {
            continue;
        };
        if !crate::state_store::is_terminal_status(&row.status) {
            return Ok(false);
        }
        if row.status != ryeos_state::objects::ThreadStatus::Continued.as_str() {
            tail_ended = true;
        }
    }
    Ok(tail_ended)
}

/// The flagged `dispatch` children of every segment of the session chain
/// `thread_id` belongs to, in segment order then spawn order. A session that
/// continued keeps its flagged children linked to the segment that launched
/// them; the session they belong to is the whole chain.
pub fn session_flagged_dispatch_children(
    store: &StateStore,
    thread_id: &str,
) -> anyhow::Result<Vec<String>> {
    let mut children = Vec::new();
    for segment in chain_segment_ids_in(store, thread_id)? {
        children.extend(store.flagged_dispatch_children(&segment)?);
    }
    Ok(children)
}

/// Stop the flagged `dispatch` children of an ended session. Unflagged
/// children and children of any other relation are never touched.
///
/// `parent_thread_id` names any segment of the session's chain. The session
/// ends with its TAIL: the flagged children of every segment are stopped when
/// the chain has ended (see [`session_chain_has_ended`]), never at a
/// `continued` hand-off, and nothing is stopped while the chain is live. The
/// session chain is resolved from the daemon's own thread records, the same
/// chain root the session's spend draws are keyed by.
///
/// Each live child is stopped gracefully (its descendants with it); one still
/// not terminal after `grace` (clamped to [`PARENT_END_GRACE_MAX`]) is forced.
/// Either way it finalizes as stopped with outcome
/// [`PARENT_END_OUTCOME_CODE`], never failed. A zero grace forces at once.
///
/// This takes no hosted-operation gate and holds no store lock across a wait,
/// so it is safe to run while the parent is still terminalizing. A child that
/// cannot be stopped is recorded in the report and does not affect the others;
/// only failing to list the parent's children is an error.
pub fn stop_flagged_children_on_parent_end(
    state: &AppState,
    parent_thread_id: &str,
    grace: Duration,
) -> anyhow::Result<ParentEndStopReport> {
    let grace = grace.min(PARENT_END_GRACE_MAX);
    let mut report = ParentEndStopReport {
        parents: 1,
        ..ParentEndStopReport::default()
    };
    if !session_chain_has_ended(&state.state_store, parent_thread_id)
        .with_context(|| format!("resolve the session chain of {parent_thread_id}"))?
    {
        return Ok(ParentEndStopReport::default());
    }
    let mut live = Vec::new();
    for child in session_flagged_dispatch_children(&state.state_store, parent_thread_id)
        .with_context(|| format!("list the flagged children of {parent_thread_id}"))?
    {
        match state.state_store.get_thread(&child) {
            Ok(Some(thread)) if crate::state_store::is_terminal_status(&thread.status) => {
                report.already_terminal.push(child);
            }
            Ok(Some(_)) => live.push(child),
            Ok(None) => report.failed.push((child, "no thread row".to_string())),
            Err(error) => report.failed.push((child, format!("{error:#}"))),
        }
    }
    // Children stop in parallel, so the wait is the longest grace, not their
    // sum, and one child's failure never holds up another.
    let results = std::thread::scope(|scope| {
        let handles = live
            .iter()
            .map(|child| scope.spawn(move || stop_flagged_child(state, child, grace)))
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .zip(&live)
            .map(|(handle, child)| {
                let result = handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("stop worker panicked")));
                (child.clone(), result)
            })
            .collect::<Vec<_>>()
    });
    for (child, result) in results {
        match result {
            Ok(()) => report.stopped.push(child),
            Err(error) => {
                tracing::warn!(
                    parent_thread_id,
                    child_thread_id = %child,
                    error = %format!("{error:#}"),
                    "failed to stop a flagged child at the end of its session; the next sweep retries"
                );
                report.failed.push((child, format!("{error:#}")));
            }
        }
    }
    Ok(report)
}

/// Stop the flagged children of every ended parent that still has one live:
/// the children found by walking the unsettled child links, grouped by parent,
/// and stopped with [`stop_flagged_children_on_parent_end`]. A parent counts as
/// ended when its whole session chain has ended (see [`session_chain_has_ended`]),
/// not when it is merely a continued segment. Pages through
/// the links in bounded batches; idempotent, and the repair for a parent that
/// ended without its own stop running (a crash between its terminal commit and
/// the stop, or a child linked after the parent ended).
pub fn stop_flagged_children_of_ended_parents(
    state: &AppState,
    grace: Duration,
) -> anyhow::Result<ParentEndStopReport> {
    // One entry per session chain, keyed by chain root.
    let mut parents = std::collections::BTreeMap::new();
    let mut after = String::new();
    loop {
        let page = state
            .state_store
            .unsettled_child_links(&after, PARENT_END_SWEEP_BATCH)?;
        let Some(last) = page.last() else {
            break;
        };
        after.clone_from(last);
        let full_page = page.len() == PARENT_END_SWEEP_BATCH;
        for child in &page {
            let Some(parent_id) = state.state_store.flagged_dispatch_parent(child)? else {
                continue;
            };
            let Some(thread) = state.state_store.get_thread(child)? else {
                continue;
            };
            if crate::state_store::is_terminal_status(&thread.status) {
                continue;
            }
            if let Some(parent) = state.state_store.get_thread(&parent_id)?
                && session_chain_has_ended(&state.state_store, &parent_id)?
            {
                parents.insert(parent.chain_root_id, parent_id);
            }
        }
        if !full_page {
            break;
        }
    }
    let mut report = ParentEndStopReport::default();
    for parent in parents.values() {
        report.absorb(stop_flagged_children_on_parent_end(state, parent, grace)?);
    }
    Ok(report)
}

fn child_is_terminal(state: &AppState, child: &str) -> anyhow::Result<bool> {
    let thread = state
        .state_store
        .get_thread(child)?
        .ok_or_else(|| anyhow::anyhow!("thread row missing for {child}"))?;
    Ok(crate::state_store::is_terminal_status(&thread.status))
}

/// How long to wait for a stopped child's row before finalizing it on its
/// behalf.
fn parent_end_settle_wait(grace: Duration) -> Duration {
    grace.min(PARENT_END_SETTLE_WAIT_MAX)
}

/// Wait until `child` is terminal or the grace runs out; `true` means it ran
/// out. Once the graceful stop has returned (the process group is proven gone)
/// the remaining wait shrinks to the settle wait: only the child's own
/// launcher can still be finalizing it.
fn wait_for_child_terminal(
    state: &AppState,
    child: &str,
    grace: Duration,
    graceful: &std::thread::ScopedJoinHandle<'_, anyhow::Result<()>>,
) -> anyhow::Result<bool> {
    let mut deadline = std::time::Instant::now() + grace;
    let mut graceful_returned = false;
    loop {
        if child_is_terminal(state, child)? {
            return Ok(false);
        }
        let now = std::time::Instant::now();
        if !graceful_returned && graceful.is_finished() {
            graceful_returned = true;
            deadline = deadline.min(now + parent_end_settle_wait(grace));
        }
        if now >= deadline {
            return Ok(true);
        }
        std::thread::sleep(PARENT_END_POLL.min(deadline - now));
    }
}

/// Stop one flagged child and its descendants and see it terminal.
fn stop_flagged_child(state: &AppState, child: &str, grace: Duration) -> anyhow::Result<()> {
    stop_flagged_child_with(state, child, grace, || {
        stop_thread_and_descendants_with_origin(
            state,
            child,
            CascadeMode::Graceful,
            StopOrigin::ParentEnd,
        )
        .map(|_| ())
    })
}

/// [`stop_flagged_child`] with the graceful stop supplied by the caller, so a
/// test can stand in for a launch that ignores the graceful stop and never
/// finalizes. Production always passes the real graceful stop.
#[doc(hidden)]
pub fn stop_flagged_child_with<G>(
    state: &AppState,
    child: &str,
    grace: Duration,
    graceful_stop: G,
) -> anyhow::Result<()>
where
    G: FnOnce() -> anyhow::Result<()> + Send,
{
    if grace.is_zero() {
        return force_flagged_child(state, child, grace);
    }
    std::thread::scope(|scope| {
        let graceful = scope.spawn(graceful_stop);
        let timed_out = match wait_for_child_terminal(state, child, grace, &graceful) {
            Ok(timed_out) => timed_out,
            Err(error) => {
                let _ = graceful.join();
                return Err(error);
            }
        };
        if !timed_out {
            // Terminal: the graceful stop returns once its group is gone.
            return graceful
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("graceful stop worker panicked")));
        }
        if graceful.is_finished() {
            // Stopped and proven, but nothing finalized the row: settle it on
            // its behalf as a graceful stop.
            graceful
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("graceful stop worker panicked")))?;
            return settle_flagged_child(state, child, CascadeMode::Graceful, grace);
        }
        // The grace ran out with the graceful stop still working: force it.
        // The hard stop makes the graceful one return.
        let forced = force_flagged_child(state, child, grace);
        let _ = graceful.join();
        forced
    })
}

/// Force-stop one flagged child and its descendants and settle it.
fn force_flagged_child(state: &AppState, child: &str, grace: Duration) -> anyhow::Result<()> {
    stop_thread_and_descendants_with_origin(state, child, CascadeMode::Hard, StopOrigin::ParentEnd)
        .with_context(|| format!("force-stop flagged child {child}"))?;
    settle_flagged_child(state, child, CascadeMode::Hard, grace)
}

/// After a proven stop, give the child's own launcher the settle wait to
/// finalize its row, then finalize it on its behalf as stopped. The durable
/// stop intent decides cancelled or killed and the parent-end outcome code;
/// the values passed here only name the intended shape.
fn settle_flagged_child(
    state: &AppState,
    child: &str,
    mode: CascadeMode,
    grace: Duration,
) -> anyhow::Result<()> {
    let wait = parent_end_settle_wait(grace);
    settle_stopped_descendants(state, child, mode, wait)?;
    let deadline = std::time::Instant::now() + wait;
    while !child_is_terminal(state, child)? {
        if std::time::Instant::now() >= deadline {
            let status = match mode {
                CascadeMode::Graceful => "cancelled",
                CascadeMode::Hard => "killed",
            };
            let finalized = state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: child.to_string(),
                status: status.to_string(),
                outcome_code: Some(PARENT_END_OUTCOME_CODE.to_string()),
                result: None,
                error: Some(json!({ "reason": PARENT_END_REASON })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            });
            if let Err(error) = finalized
                && !child_is_terminal(state, child)?
            {
                return Err(error).with_context(|| format!("settle stopped flagged child {child}"));
            }
            return Ok(());
        }
        std::thread::sleep(PARENT_END_POLL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_end_grace_defaults_to_thirty_seconds_and_clamps_to_three_hundred() {
        assert_eq!(parent_end_grace(None), Duration::from_secs(30));
        assert_eq!(
            parent_end_grace(Some(Duration::from_secs(45))),
            Duration::from_secs(45)
        );
        assert_eq!(
            parent_end_grace(Some(Duration::from_secs(300))),
            Duration::from_secs(300)
        );
        assert_eq!(
            parent_end_grace(Some(Duration::from_secs(301))),
            Duration::from_secs(300)
        );
        assert_eq!(
            parent_end_grace(Some(Duration::from_secs(86_400))),
            Duration::from_secs(300)
        );
        assert_eq!(parent_end_grace(Some(Duration::ZERO)), Duration::ZERO);
    }

    #[test]
    fn parent_end_outcome_is_stopped_never_failed() {
        assert!(!ryeos_state::event_types::outcome_code_is_failure(
            PARENT_END_OUTCOME_CODE
        ));
    }

    #[test]
    fn hard_mode_always_kills_ignoring_declared_mode() {
        // Even a thread that declared a graceful mode is SIGKILLed under Hard.
        assert_eq!(
            stop_action(
                CascadeMode::Hard,
                Some(CancellationMode::Graceful { grace_secs: 30 })
            ),
            ShutdownAction::Hard
        );
        assert_eq!(stop_action(CascadeMode::Hard, None), ShutdownAction::Hard);
    }

    #[test]
    fn graceful_cancel_escalates_to_a_group_kill_after_its_grace() {
        // Every cancel is a proven stop: SIGTERM, grace, then the group kill,
        // never a signal with nothing awaited.
        assert!(matches!(
            stop_action(CascadeMode::Graceful, None),
            ShutdownAction::Graceful(_)
        ));
        assert_eq!(
            stop_action(
                CascadeMode::Graceful,
                Some(CancellationMode::Graceful { grace_secs: 2 })
            ),
            ShutdownAction::Graceful(Duration::from_secs(2))
        );
        // A launch that declared it can only be hard-stopped is killed at once.
        assert_eq!(
            stop_action(CascadeMode::Graceful, Some(CancellationMode::Hard)),
            ShutdownAction::Hard
        );
    }

    #[test]
    fn descendant_failure_cannot_be_reported_as_a_clean_cascade() {
        for skipped in ["stop_tombstone_failed", "read_error", "no_thread_row"] {
            assert!(descendant_stop_unproven(&json!({ "skipped": skipped })));
        }
        assert!(descendant_stop_unproven(
            &json!({ "scoped_child_settlement": "failed" })
        ));
        assert!(descendant_stop_unproven(
            &json!({ "result": STOP_UNPROVEN, "method": "sigkill_delivered_unsettled" })
        ));
        assert!(!descendant_stop_unproven(
            &json!({ "result": "stopped", "method": "already_dead" })
        ));
        assert!(!descendant_stop_unproven(&json!({
            "signal": "SIGTERM",
            "result": "sent",
            "scoped_child_settlement": "settled"
        })));
    }

    #[test]
    fn in_process_descendant_stop_is_proven_only_by_its_terminal() {
        for status in ["running", "created"] {
            let report = in_process_descendant_report("T-in-process-child", status);
            assert_eq!(report["skipped"], "in_process_handler_running");
            assert!(
                descendant_stop_unproven(&report),
                "a {status} in-process descendant must not count as stopped"
            );
        }
        for status in ["cancelled", "killed", "completed", "failed"] {
            let report = in_process_descendant_report("T-in-process-child", status);
            assert_eq!(report["in_process_handler"], "terminal");
            assert_eq!(report["status"], status);
            assert!(
                !descendant_stop_unproven(&report),
                "a {status} in-process descendant is a proven stop"
            );
        }
    }
}
