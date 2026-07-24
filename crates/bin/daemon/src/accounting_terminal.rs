//! Shared terminal-admission accounting closure.
//!
//! Every terminal path — runtime-requested finalize, supervisor
//! death/timeout/kill/cancel recovery, and startup reconciliation — must
//! close a thread's financial attempts through this one helper before (or
//! atomically with) committing terminal thread state. No path reimplements
//! the accounting closure.

use anyhow::Result;
use ryeos_accounting::ReconciliationReason;
use ryeos_app::state::AppState;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Fence the accounting gate(s) for a thread that is going terminal and
/// conservatively close every nonterminal attempt: `Reserved` releases,
/// `Issued` charges its reserved maximum. Idempotent; a thread with no
/// accounting scope (or a node without a ledger) is a no-op — such threads
/// were never admitted to reserve.
///
/// When the exact launch owner is known (runtime-requested finalize), only
/// that generation is fenced. Supervisor/startup recovery of a dead or
/// superseded owner fences every open gate for the thread.
pub fn fence_accounting_for_terminal(
    state: &AppState,
    thread_id: &str,
    launch_owner: Option<&str>,
    reason: ReconciliationReason,
) -> Result<()> {
    let Some(ledger) = state.accounting.as_ref() else {
        return Ok(());
    };
    match launch_owner {
        Some(owner) => {
            let outcome =
                ledger.fence_launch_gate_and_close_attempts(thread_id, owner, reason, now_ms())?;
            log_fence(thread_id, owner, &outcome);
        }
        None => {
            // Owner unknown (dead process, orphan recovery): fence every
            // OPEN gate for the thread. A gate with no reservation yet is
            // still an open admission surface — sweeping only nonterminal
            // reservations would let a racing reserve/issue callback slip
            // in between the scan and terminal publication.
            for generation in ledger.open_gates_for_thread(thread_id)? {
                let outcome = ledger.fence_launch_gate_and_close_attempts(
                    thread_id,
                    &generation,
                    reason,
                    now_ms(),
                )?;
                log_fence(thread_id, &generation, &outcome);
            }
            // Belt-and-braces: nonterminal reservations whose gate row is
            // already fenced (or predates gate bookkeeping) are closed
            // through the same idempotent fence.
            for (attempt_id, attempt_thread, generation, state_before) in
                ledger.nonterminal_reservations()?
            {
                if attempt_thread != thread_id {
                    continue;
                }
                tracing::warn!(
                    thread_id,
                    attempt_id,
                    state = state_before.as_str(),
                    "closing orphaned nonterminal provider attempt at terminal admission"
                );
                let outcome = ledger.fence_launch_gate_and_close_attempts(
                    thread_id,
                    &generation,
                    reason,
                    now_ms(),
                )?;
                log_fence(thread_id, &generation, &outcome);
            }
        }
    }
    Ok(())
}

/// After the terminal thread state (and its CAS publication) has committed,
/// clear the durable terminal-publication marker on the fenced gate.
pub fn confirm_terminal_accounting_publication(
    state: &AppState,
    thread_id: &str,
    launch_owner: &str,
) -> Result<()> {
    let Some(ledger) = state.accounting.as_ref() else {
        return Ok(());
    };
    ledger.confirm_terminal_publication(thread_id, launch_owner)
}

fn log_fence(thread_id: &str, generation: &str, outcome: &ryeos_app::accounting_db::FenceOutcome) {
    tracing::info!(
        thread_id,
        generation,
        ?outcome,
        "accounting gate fenced at terminal admission"
    );
}
