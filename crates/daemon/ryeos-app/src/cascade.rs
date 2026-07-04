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

use crate::process::{kill_by_action, resolve_shutdown_action, ShutdownAction};
use crate::state_store::StateStore;

/// How hard to stop each descendant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CascadeMode {
    /// SIGKILL immediately (a `kill`).
    Hard,
    /// Each child's own graceful shutdown action (a `cancel`).
    Graceful,
}

/// Resolve the shutdown action for one descendant: `Hard` is an unconditional
/// SIGKILL; `Graceful` honours the child's own declared cancellation mode
/// (defaulting to a graceful SIGTERM-then-SIGKILL when unset).
fn shutdown_action_for(
    mode: CascadeMode,
    cancellation_mode: Option<CancellationMode>,
) -> ShutdownAction {
    match mode {
        CascadeMode::Hard => ShutdownAction::Hard,
        CascadeMode::Graceful => resolve_shutdown_action(cancellation_mode),
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
    let result = kill_by_action(pgid, shutdown_action_for(mode, cancellation_mode));
    json!({
        "thread_id": thread_id,
        "pgid": pgid,
        "method": result.method,
        "success": result.success,
    })
}

/// Stop a thread and its live descendants: signal the target per `mode`, then
/// cascade to every live descendant. `cancel` → graceful (SIGTERM, which both
/// runtimes now honour cooperatively); `kill` → hard SIGKILL. The single shared
/// entry point for `commands.submit` and `runtime.submit_command`, so both the
/// operator and runtime control paths stop the target itself — not only its
/// children.
pub fn stop_thread_and_descendants(
    store: &StateStore,
    thread_id: &str,
    mode: CascadeMode,
) -> anyhow::Result<Value> {
    let target = signal_thread(store, thread_id, mode);
    let descendants = cascade_descendants(store, thread_id, mode)?;
    Ok(json!({ "target": target, "descendants": descendants }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn hard_mode_always_sigkills_ignoring_declared_mode() {
        // Even a child that declared a graceful mode is hard-killed under Hard.
        assert_eq!(
            shutdown_action_for(
                CascadeMode::Hard,
                Some(CancellationMode::Graceful { grace_secs: 30 })
            ),
            ShutdownAction::Hard
        );
        assert_eq!(
            shutdown_action_for(CascadeMode::Hard, None),
            ShutdownAction::Hard
        );
    }

    #[test]
    fn graceful_mode_honours_the_childs_declared_cancellation_mode() {
        assert_eq!(
            shutdown_action_for(CascadeMode::Graceful, Some(CancellationMode::Hard)),
            ShutdownAction::Hard
        );
        assert_eq!(
            shutdown_action_for(
                CascadeMode::Graceful,
                Some(CancellationMode::Graceful { grace_secs: 7 })
            ),
            ShutdownAction::Graceful(Duration::from_secs(7))
        );
        // Unset → the default graceful window.
        assert_eq!(
            shutdown_action_for(CascadeMode::Graceful, None),
            ShutdownAction::Graceful(Duration::from_secs(3))
        );
    }
}
