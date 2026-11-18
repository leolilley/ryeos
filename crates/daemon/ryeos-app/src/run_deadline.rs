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
