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
use crate::state::AppState;
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
    let Some(thread) = state.state_store.get_thread(thread_id)? else {
        return Ok(vec![thread_id.to_string()]);
    };
    let mut ids: Vec<String> = state
        .state_store
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
            state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread.thread_id,
                status: "cancelled".to_string(),
                outcome_code: Some("cancelled".to_string()),
                result: None,
                error: Some(json!({"reason": "ancestor_cancelled_before_launch"})),
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
            state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread.thread_id.clone(),
                status: "cancelled".into(),
                outcome_code: Some("cancelled".into()),
                result: None,
                error: Some(json!({"reason":"ancestor_cancelled_before_launch"})),
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
    let runtime = match store.request_thread_stop(thread_id, stop_intent) {
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
        return signal_thread(state, thread_id, mode);
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
    let queued = cancel_queued_descendants(state, thread_id)?;
    let target = signal_thread(state, thread_id, mode);
    let descendants = cascade_descendants(state, thread_id, mode)?;
    if descendant_stop_unproven(&target) {
        anyhow::bail!("target stop or scoped-child settlement unproven for {thread_id}: {target}");
    }
    Ok((
        json!({ "target": target, "descendants": descendants, "queued": queued }),
        queued,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
