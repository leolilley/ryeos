//! The run deadline as the daemon settles it.
//!
//! The execution tree's absolute deadline lives in the accounting ledger. Each
//! thread ends earlier by the hold-back sealed in its launch metadata, so its
//! ancestors keep a window for their own wrap-up. This module derives a
//! thread's effective end from those two facts and builds the typed terminal
//! record for a thread whose end passed before it could launch or resume.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::accounting_db::AccountingDb;
use crate::launch_metadata::RuntimeLaunchMetadata;
use crate::thread_lifecycle::ThreadFinalizeParams;
use ryeos_engine::launch_envelope_types::DeadlineLimits;

/// Terminal status of a thread stopped by its run deadline.
pub const DEADLINE_TERMINAL_STATUS: &str = "timed_out";
/// Outcome code and result `outcome` of a thread stopped by its run deadline.
pub const DEADLINE_OUTCOME: &str = "deadline";
/// Phase recorded when the end passed before a launch or resume could spawn.
pub const PHASE_ELAPSED_BEFORE_LAUNCH: &str = "elapsed_before_launch";
/// Reason recorded when the end passed while the node was down.
pub const REASON_ELAPSED_WHILE_DOWN: &str = "deadline_elapsed_while_down";
/// Reason recorded when a launch or resume found its end already passed.
pub const REASON_ELAPSED_BEFORE_LAUNCH: &str = "deadline_elapsed_before_launch";

/// The effective end of a thread: the tree deadline minus the hold-back sealed
/// for that thread. `None` when the tree has no deadline.
pub fn effective_end_at_ms(tree_deadline_at_ms: Option<i64>, hold_back_ms: u64) -> Option<i64> {
    tree_deadline_at_ms.map(|deadline| DeadlineLimits::effective_at_ms(deadline, hold_back_ms))
}

/// Whether an effective end has been reached at `now_ms`.
pub fn has_elapsed(end_at_ms: Option<i64>, now_ms: i64) -> bool {
    end_at_ms.is_some_and(|end| now_ms >= end)
}

/// The effective end of the thread described by `metadata`, read from the
/// execution budget row of its accounting scope. `None` when the thread has no
/// scope, the node has no ledger, or the tree has no deadline.
pub fn thread_end_at_ms(
    accounting: Option<&AccountingDb>,
    metadata: Option<&RuntimeLaunchMetadata>,
) -> Result<Option<i64>> {
    let (Some(accounting), Some(metadata)) = (accounting, metadata) else {
        return Ok(None);
    };
    let Some(scope) = metadata.accounting_scope.as_ref() else {
        return Ok(None);
    };
    let tree_deadline_at_ms = accounting
        .execution_resource_budget_snapshot(&scope.execution_budget_id)
        .context("read execution budget deadline")?
        .and_then(|budget| budget.deadline_at_ms);
    Ok(effective_end_at_ms(
        tree_deadline_at_ms,
        metadata.deadline_hold_back_ms.unwrap_or(0),
    ))
}

/// Outcome code of a tool call cut at its caller's hard stop.
pub const TOOL_INTERRUPTED_OUTCOME: &str =
    ryeos_runtime::callback_contract::TOOL_INTERRUPTED_AT_DEADLINE_OUTCOME;
/// Refusal code of a tool launch that found its caller's hard stop reached.
pub const TOOL_DEADLINE_ELAPSED_CODE: &str = "tool_deadline_elapsed";

/// The end of a tool called by a runtime whose own deadline share is `parent`:
/// the tree deadline minus the hold-back the parent's reserve rule gives a
/// callee at `now_ms`, which is the parent's hard stop. `None` when the tree
/// has no deadline. Derived only from sealed records, never from a number the
/// caller supplies.
pub fn tool_end_at_ms(
    tree_deadline_at_ms: Option<i64>,
    parent: &DeadlineLimits,
    now_ms: i64,
) -> Option<i64> {
    tree_deadline_at_ms.map(|deadline| {
        DeadlineLimits::effective_at_ms(deadline, parent.child_hold_back_ms(deadline, now_ms))
    })
}

/// How a tool's own timeout meets its caller's hard stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTimeout {
    /// No end applies, or the tool's own timeout is the shorter: unchanged.
    Own(u64),
    /// The hard stop is nearer than the tool's own timeout (or the tool has
    /// none): the tool is cut after this many whole seconds.
    CutAtDeadline(u64),
}

/// The effective tool timeout: the shorter of the tool's own timeout and the
/// time left to `end_at_ms`. It never lengthens a timeout. A tool timeout of
/// `0` means "none", so a bound is then the only limit. An end that has passed
/// or is under a second away is refused, never turned into `0`, because a
/// timeout of `0` would mean no timeout at all. All arithmetic saturates.
pub fn clamp_tool_timeout(
    own_timeout_secs: u64,
    end_at_ms: Option<i64>,
    now_ms: i64,
) -> Result<ToolTimeout, ToolDeadlineElapsed> {
    let Some(end_at_ms) = end_at_ms else {
        return Ok(ToolTimeout::Own(own_timeout_secs));
    };
    let remaining_ms = end_at_ms.saturating_sub(now_ms);
    let remaining_secs = u64::try_from(remaining_ms).unwrap_or(0) / 1_000;
    if remaining_secs == 0 {
        return Err(ToolDeadlineElapsed { end_at_ms, now_ms });
    }
    if own_timeout_secs != 0 && own_timeout_secs <= remaining_secs {
        Ok(ToolTimeout::Own(own_timeout_secs))
    } else {
        Ok(ToolTimeout::CutAtDeadline(remaining_secs))
    }
}

/// The caller's hard stop is reached (or under a second away): the tool does
/// not launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolDeadlineElapsed {
    pub end_at_ms: i64,
    pub now_ms: i64,
}

impl std::fmt::Display for ToolDeadlineElapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{TOOL_DEADLINE_ELAPSED_CODE}: the run's hard stop ({} ms) is reached at {} ms; \
             the tool did not start",
            self.end_at_ms, self.now_ms
        )
    }
}

impl std::error::Error for ToolDeadlineElapsed {}

/// The terminal record for a thread whose effective end passed before it ran:
/// status `timed_out`, outcome `deadline`, and a result that names the phase
/// and the reason. Nothing was spawned, so there is no cost to carry.
pub fn elapsed_finalize_params(
    thread_id: &str,
    reason: &str,
    end_at_ms: i64,
    settled_at_ms: i64,
) -> ThreadFinalizeParams {
    ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: DEADLINE_TERMINAL_STATUS.to_string(),
        outcome_code: Some(DEADLINE_OUTCOME.to_string()),
        result: Some(elapsed_result(reason, end_at_ms, settled_at_ms)),
        error: None,
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    }
}

fn elapsed_result(reason: &str, end_at_ms: i64, settled_at_ms: i64) -> Value {
    json!({
        "outcome": DEADLINE_OUTCOME,
        "phase": PHASE_ELAPSED_BEFORE_LAUNCH,
        "reason": reason,
        "deadline_at_ms": end_at_ms,
        "stopped_at_ms": settled_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_end_subtracts_the_hold_back_and_needs_a_tree_deadline() {
        assert_eq!(effective_end_at_ms(Some(100_000), 30_000), Some(70_000));
        assert_eq!(effective_end_at_ms(None, 30_000), None);
    }

    #[test]
    fn elapsed_is_inclusive_of_the_end_and_false_without_one() {
        assert!(has_elapsed(Some(1_000), 1_000));
        assert!(!has_elapsed(Some(1_000), 999));
        assert!(!has_elapsed(None, i64::MAX));
    }

    fn parent(hold_back_ms: u64, reserve_seconds: u64) -> DeadlineLimits {
        DeadlineLimits {
            reserve_seconds,
            hold_back_ms,
            at_ms: None,
        }
    }

    #[test]
    fn a_tool_ends_at_the_callers_hard_stop() {
        // Tree deadline 100 s, parent holds back nothing and reserves 5 s.
        let reserving = parent(0, 5);
        assert_eq!(
            tool_end_at_ms(Some(100_000), &reserving, 10_000),
            Some(95_000)
        );
        // Inside the reserve window the callee may use what remains.
        assert_eq!(
            tool_end_at_ms(Some(100_000), &reserving, 96_000),
            Some(100_000)
        );
        assert_eq!(tool_end_at_ms(None, &reserving, 10_000), None);
        // The hold-back of an ancestor is subtracted as well.
        assert_eq!(
            tool_end_at_ms(Some(100_000), &parent(20_000, 5), 0),
            Some(75_000)
        );
    }

    #[test]
    fn the_clamp_only_shortens_and_never_yields_no_timeout() {
        // Own timeout longer than the time left: cut at what is left.
        assert_eq!(
            clamp_tool_timeout(300, Some(60_000), 5_000),
            Ok(ToolTimeout::CutAtDeadline(55))
        );
        // Own timeout shorter: kept.
        assert_eq!(
            clamp_tool_timeout(10, Some(60_000), 5_000),
            Ok(ToolTimeout::Own(10))
        );
        // A tool with no timeout of its own is bounded by the end.
        assert_eq!(
            clamp_tool_timeout(0, Some(60_000), 5_000),
            Ok(ToolTimeout::CutAtDeadline(55))
        );
        // No end: unchanged, including a tool's own `0`.
        assert_eq!(
            clamp_tool_timeout(300, None, 5_000),
            Ok(ToolTimeout::Own(300))
        );
        assert_eq!(clamp_tool_timeout(0, None, 5_000), Ok(ToolTimeout::Own(0)));
        // Whole seconds round down, so the cut is never after the end.
        assert_eq!(
            clamp_tool_timeout(300, Some(10_999), 0),
            Ok(ToolTimeout::CutAtDeadline(10))
        );
    }

    #[test]
    fn a_reached_or_sub_second_end_is_refused_not_unbounded() {
        for (end, now) in [(5_000, 5_000), (5_000, 9_000), (5_999, 5_000), (0, 0)] {
            assert!(
                clamp_tool_timeout(300, Some(end), now).is_err(),
                "{end} {now}"
            );
            assert!(
                clamp_tool_timeout(0, Some(end), now).is_err(),
                "{end} {now}"
            );
        }
        // Extreme values saturate instead of panicking or wrapping.
        assert!(clamp_tool_timeout(300, Some(i64::MIN), i64::MAX).is_err());
        assert!(clamp_tool_timeout(300, Some(i64::MAX), i64::MIN).is_ok());
        assert_eq!(
            clamp_tool_timeout(u64::MAX, Some(i64::MAX), 0),
            Ok(ToolTimeout::CutAtDeadline(i64::MAX as u64 / 1_000))
        );
        assert!(clamp_tool_timeout(300, Some(i64::MIN), i64::MIN).is_err());
    }

    #[test]
    fn a_callers_larger_claim_cannot_outlast_the_sealed_bound() {
        // The sealed end is 60 s from now. Whatever a caller says about its
        // remaining time never enters the computation: the same end always
        // yields the same cut.
        let sealed = tool_end_at_ms(Some(60_000), &parent(0, 0), 0);
        assert_eq!(
            clamp_tool_timeout(300, sealed, 0),
            Ok(ToolTimeout::CutAtDeadline(60))
        );
    }

    #[test]
    fn elapsed_record_is_a_typed_timed_out_settle() {
        let params = elapsed_finalize_params("T-1", REASON_ELAPSED_WHILE_DOWN, 5_000, 6_000);
        assert_eq!(params.status, "timed_out");
        assert_eq!(params.outcome_code.as_deref(), Some("deadline"));
        let result = params.result.unwrap();
        assert_eq!(result["outcome"], "deadline");
        assert_eq!(result["phase"], "elapsed_before_launch");
        assert_eq!(result["reason"], "deadline_elapsed_while_down");
        assert_eq!(result["deadline_at_ms"], 5_000);
        assert_eq!(result["stopped_at_ms"], 6_000);
        assert!(params.error.is_none());
    }
}
