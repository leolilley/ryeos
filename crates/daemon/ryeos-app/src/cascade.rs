//! Cancel/kill cascade: signal a thread's live descendants so a cancelled or
//! killed parent cannot keep running work through a child it spawned (the
//! clobber where a "cancelled" parent keeps authoring via a live inline child).
//!
//! Lineage comes from the runtime child-link table
//! ([`StateStore::descendant_thread_ids`]); each descendant's CURRENT pgid is
//! resolved from `thread_runtime` at signal time, never a stored copy — the pgid
//! attaches/updates after thread creation. A descendant with no live pgid (not
//! yet attached, or already gone) is skipped. The cascade only SIGNALS: a killed
//! child's own launcher, blocked waiting on the subprocess, observes the exit and
//! finalizes the row — the cascade does not finalize on its behalf.

use serde_json::{json, Value};

use ryeos_engine::contracts::CancellationMode;

use crate::process::signal_process_group;
use crate::state_store::StateStore;
use crate::state::AppState;
use crate::thread_lifecycle::ThreadFinalizeParams;

/// How hard to stop a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CascadeMode {
    /// SIGKILL immediately (a `kill`).
    Hard,
    /// One-shot SIGTERM — a cooperative `cancel`.
    Graceful,
}

/// The single signal to deliver for `mode`. Deliberately ONE-SHOT: never an
/// escalating SIGTERM→wait→SIGKILL. Escalation would block the async handler for
/// the grace period AND preempt the runtime's cooperative cancel — both runtimes
/// honour SIGTERM at their own boundary (a graph at its next node, a directive
/// by cutting its stream), so a forced SIGKILL after a fixed grace would kill a
/// long node mid-flight and settle it `failed` instead of `cancelled`. `kill` is
/// SIGKILL; a graceful `cancel` is SIGTERM unless the tool declared it can only
/// be hard-stopped (`Hard` cancellation mode), in which case a one-shot SIGKILL.
/// A hung target is escalated by the operator with an explicit `kill`.
fn cancel_signal(mode: CascadeMode, cancellation_mode: Option<CancellationMode>) -> i32 {
    match mode {
        CascadeMode::Hard => libc::SIGKILL,
        CascadeMode::Graceful => match cancellation_mode {
            Some(CancellationMode::Hard) => libc::SIGKILL,
            _ => libc::SIGTERM,
        },
    }
}

fn signal_name(signal: i32) -> &'static str {
    if signal == libc::SIGKILL {
        "SIGKILL"
    } else if signal == libc::SIGTERM {
        "SIGTERM"
    } else {
        "signal"
    }
}

/// Signal every live descendant of `root_thread_id` per `mode`, resolving each
/// child's current pgid at signal time. Returns a per-child report for the
/// caller's response/log. A store error on the descendant walk propagates; a
/// single child's missing pgid or failed signal is recorded in the report, not
/// raised — one unreachable child must not abort the cascade.
pub fn cascade_descendants(
    store: &StateStore,
    root_thread_id: &str,
    mode: CascadeMode,
) -> anyhow::Result<Vec<Value>> {
    let descendants = store.descendant_thread_ids(root_thread_id)?;
    Ok(descendants
        .iter()
        .map(|child| signal_thread(store, child, mode))
        .collect())
}

/// Cancel durable descendants that have not yet been admitted. Membership is
/// tombstoned as one store operation before lifecycle finalization, preventing
/// admission across crashes (and deliberately admitting no replacements).
pub fn cancel_queued_descendants(state: &AppState, root_thread_id: &str) -> anyhow::Result<Vec<String>> {
    let descendants = state.state_store.descendant_thread_ids(root_thread_id)?;
    let removed = state.state_store.launch_window_cancel_queued(&descendants, lillux::time::timestamp_millis())?;
    for chain_root in &removed {
        let Some(thread) = state.state_store.get_thread(chain_root)? else {
            state.state_store.discard_window_member(chain_root)?;
            continue;
        };
        if !crate::state_store::is_terminal_status(&thread.status) {
            state.threads.finalize_thread(&ThreadFinalizeParams {
            thread_id: thread.thread_id,
            status: "cancelled".to_string(),
            outcome_code: Some("cancelled".to_string()),
            result: None,
            error: Some(json!({"reason": "ancestor_cancelled_before_launch"})),
            metadata: None, artifacts: Vec::new(), final_cost: None, summary_json: None,
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
        if !crate::state_store::is_terminal_status(&thread.status) {
            state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread.thread_id, status: "cancelled".into(), outcome_code: Some("cancelled".into()),
                result: None, error: Some(json!({"reason":"ancestor_cancelled_before_launch"})), metadata: None,
                artifacts: Vec::new(), final_cost: None, summary_json: None,
            })?;
        }
        state.state_store.discard_window_member(&root)?;
    }
    Ok(())
}

/// Signal one thread's process group per `mode`, resolving its CURRENT pgid at
/// signal time. Returns a report value and NEVER errors: a missing/unreadable
/// row, a terminal thread, or an absent/non-positive pgid each yields a skip
/// marker, so one unreachable thread never aborts a wider cascade.
///
/// A terminal thread is skipped because its `pgid` is never cleared on finalize —
/// signalling it would target a possibly OS-recycled group (an unrelated
/// process). A non-positive pgid is skipped because `kill(-0, …)` would hit the
/// daemon's own group and a negative value an arbitrary PID (`kill_by_action`
/// guards only the daemon pgid, not `<= 0`).
pub fn signal_thread(store: &StateStore, thread_id: &str, mode: CascadeMode) -> Value {
    let thread = match store.get_thread(thread_id) {
        Ok(Some(thread)) => thread,
        Ok(None) => return json!({ "thread_id": thread_id, "skipped": "no_thread_row" }),
        Err(e) => {
            return json!({ "thread_id": thread_id, "skipped": "read_error", "error": e.to_string() })
        }
    };
    if crate::state_store::is_terminal_status(&thread.status) {
        return json!({ "thread_id": thread_id, "skipped": "terminal", "status": thread.status });
    }
    let pgid = match thread.runtime.pgid {
        Some(pgid) if pgid > 0 => pgid,
        Some(_) => return json!({ "thread_id": thread_id, "skipped": "invalid_pgid" }),
        None => return json!({ "thread_id": thread_id, "skipped": "no_pgid" }),
    };
    let cancellation_mode = thread
        .runtime
        .launch_metadata
        .as_ref()
        .and_then(|lm| lm.cancellation_mode);
    let signal = cancel_signal(mode, cancellation_mode);
    let result = signal_process_group(pgid, signal);
    json!({
        "thread_id": thread_id,
        "pgid": pgid,
        "signal": signal_name(signal),
        "result": result.as_str(),
    })
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
) -> anyhow::Result<Value> {
    let queued = cancel_queued_descendants(state, thread_id)?;
    let target = signal_thread(&state.state_store, thread_id, mode);
    let descendants = cascade_descendants(&state.state_store, thread_id, mode)?;
    Ok(json!({ "target": target, "descendants": descendants, "queued": queued }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_mode_always_sigkills_ignoring_declared_mode() {
        // Even a thread that declared a graceful mode is SIGKILLed under Hard.
        assert_eq!(
            cancel_signal(
                CascadeMode::Hard,
                Some(CancellationMode::Graceful { grace_secs: 30 })
            ),
            libc::SIGKILL
        );
        assert_eq!(cancel_signal(CascadeMode::Hard, None), libc::SIGKILL);
    }

    #[test]
    fn graceful_cancel_is_one_shot_sigterm_unless_the_tool_is_hard_only() {
        // The common case: a single SIGTERM, never an escalating SIGKILL.
        assert_eq!(cancel_signal(CascadeMode::Graceful, None), libc::SIGTERM);
        assert_eq!(
            cancel_signal(
                CascadeMode::Graceful,
                Some(CancellationMode::Graceful { grace_secs: 7 })
            ),
            libc::SIGTERM
        );
        // A tool that declared it can only be hard-stopped gets a one-shot
        // SIGKILL even on a cancel.
        assert_eq!(
            cancel_signal(CascadeMode::Graceful, Some(CancellationMode::Hard)),
            libc::SIGKILL
        );
    }
}
