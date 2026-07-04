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
/// caller's response/log. A store error on the descendant walk or a runtime-info
/// read propagates; a single child's missing pgid or failed signal is recorded
/// in the report, not raised — one unreachable child must not abort the cascade.
pub fn cascade_descendants(
    store: &StateStore,
    root_thread_id: &str,
    mode: CascadeMode,
) -> anyhow::Result<Vec<Value>> {
    let descendants = store.descendant_thread_ids(root_thread_id)?;
    let mut report = Vec::with_capacity(descendants.len());
    for child in descendants {
        let Some(thread) = store.get_thread(&child)? else {
            report.push(json!({ "thread_id": child, "skipped": "no_thread_row" }));
            continue;
        };
        let Some(pgid) = thread.runtime.pgid else {
            report.push(json!({ "thread_id": child, "skipped": "no_pgid" }));
            continue;
        };
        let cancellation_mode = thread
            .runtime
            .launch_metadata
            .as_ref()
            .and_then(|lm| lm.cancellation_mode);
        let result = kill_by_action(pgid, shutdown_action_for(mode, cancellation_mode));
        report.push(json!({
            "thread_id": child,
            "pgid": pgid,
            "method": result.method,
            "success": result.success,
        }));
    }
    Ok(report)
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
