//! Stop settlement for one daemon-owned qualification child.
//!
//! The caller must first commit the root's durable stop tombstone. This
//! service never invents a natural-exit receipt: it transfers the exact live
//! process owner to checked abort, or signals the observer's exact retained
//! scope and waits for that observer to reap. SQLite and registry locks are
//! held only for short state cuts, never for a Lillux wait.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::runtime_db::scoped_child_attempt::{ScopedChildAttemptRecord, ScopedChildPhase};
use crate::scoped_producer_process::{ScopedProducerProcessKey, ScopedProducerStopClaim};
use crate::state::AppState;

/// Settle every retained attempt owned by the durably stopped root. An empty
/// set means no producer reached durable reservation; a retained non-retired
/// row with no local process owner is uncertainty, not permission to
/// terminalize. The generic thread terminal CAS independently refuses any
/// unsettled attempt, including one from a predecessor launch owner.
pub fn stop_scoped_producer_for_root(
    state: &AppState,
    root_thread_id: &str,
    maximum_wait: Duration,
) -> Result<()> {
    ensure!(
        maximum_wait > Duration::ZERO,
        "scoped stop requires a bounded wait"
    );
    let runtime = state
        .state_store
        .get_thread(root_thread_id)?
        .context("scoped stop root thread is absent")?;
    ensure!(
        runtime.runtime.stop_intent.is_some(),
        "scoped child stop requires the durable root stop tombstone"
    );
    let deadline = lillux::time::MonotonicDeadline::after(maximum_wait);
    let mut attempts = Vec::new();
    for attempt_id in state.state_store.unsettled_scoped_child_attempt_ids()? {
        let retained = state
            .state_store
            .scoped_child_attempt(&attempt_id)?
            .context("unsettled scoped child disappeared during stop")?;
        if retained.initial.owner.thread_id == root_thread_id {
            attempts.push(retained);
        }
    }
    // Cleanup-only authority comes from the retained exact row. A launch
    // claim may have been released or rotated when the root process exited;
    // requiring that *live* claim here would strand a still-owned child.
    // Neither this enumeration nor the row permits a new launch or a natural
    // receipt. One total deadline covers every predecessor/current attempt.
    for record in attempts {
        stop_exact_attempt(state, &record, &deadline)?;
    }
    Ok(())
}

fn stop_exact_attempt(
    state: &AppState,
    record: &ScopedChildAttemptRecord,
    deadline: &lillux::time::MonotonicDeadline,
) -> Result<()> {
    let key = ScopedProducerProcessKey::new(
        record.initial.attempt_id.clone(),
        record.initial.owner.clone(),
    )?;
    let mut observer_signalled = false;
    let mut observer_termination_error = None;

    loop {
        if deadline.has_elapsed() {
            bail!(
                "scoped child stop expired before exact process and scope settlement{}",
                observer_termination_error
                    .as_deref()
                    .map(|error| format!("; observer termination: {error}"))
                    .unwrap_or_default()
            );
        }
        let current = state
            .state_store
            .scoped_child_attempt(&key.attempt_id)?
            .context("scoped child attempt disappeared during stop")?;
        ensure!(
            current.initial.owner == key.launch_owner,
            "scoped child owner changed during stop"
        );
        if current.phase == ScopedChildPhase::Retired {
            require_exact_retirement(&current)?;
            return Ok(());
        }
        match state.scoped_producer_processes.request_stop(&key)? {
            ScopedProducerStopClaim::Live(child) => {
                // The registry mutex was released before checked reaping. An
                // abort error consumes the process handle but does not prove
                // wrapper reaping; leave the durable row unsettled for restart.
                // Wake any callback currently blocked on the parent channel;
                // this is not death proof and cannot replace checked abort.
                if let Some(io) = &child.interactive_io {
                    let _ = io.interrupt();
                }
                let abort = child.process.abort_and_reap_checked();
                if let Err(error) = abort {
                    let _ = state.scoped_producer_processes.finish_stop(&key, false);
                    bail!("scoped child checked abort did not prove process reaping: {error}");
                }
                let settlement =
                    crate::scoped_producer_observe::settle_cleanup_only(state, &current);
                let settled = settlement.is_ok();
                state.scoped_producer_processes.finish_stop(&key, settled)?;
                settlement?;
            }
            ScopedProducerStopClaim::Observing(recovery) => {
                if !observer_signalled {
                    // The observer owns the wrapper. Exact-scope termination
                    // unblocks its natural wait; the root stop CAS prevents
                    // any later natural observation publication. Only the
                    // observer may reap and retire this attempt.
                    let termination = recovery
                        .terminate_and_wait(deadline.remaining().min(recovery.control_timeout()))
                        .map_err(anyhow::Error::msg);
                    if let Err(error) = termination {
                        let after = state
                            .state_store
                            .scoped_child_attempt(&key.attempt_id)?
                            .context("scoped child disappeared after scope termination race")?;
                        if after.phase != ScopedChildPhase::Retired {
                            observer_termination_error = Some(error.to_string());
                        } else {
                            require_exact_retirement(&after)?;
                            return Ok(());
                        }
                    }
                    observer_signalled = true;
                }
            }
            ScopedProducerStopClaim::Starting | ScopedProducerStopClaim::Settling => {}
            ScopedProducerStopClaim::Settled => {
                let settled = state
                    .state_store
                    .scoped_child_attempt(&key.attempt_id)?
                    .context("settled scoped child attempt disappeared")?;
                require_exact_retirement(&settled)?;
                return Ok(());
            }
            ScopedProducerStopClaim::Uncertain => {
                bail!("scoped child process owner left uncertain settlement")
            }
            ScopedProducerStopClaim::Missing => {
                bail!("retained scoped child has no current-daemon process owner")
            }
        }
        let pause = deadline.remaining().min(Duration::from_millis(50));
        if pause == Duration::ZERO {
            bail!("scoped child stop expired before exact scope settlement");
        }
        state.scoped_producer_processes.wait_for_change(pause)?;
    }
}

fn require_exact_retirement(record: &ScopedChildAttemptRecord) -> Result<()> {
    ensure!(
        record.phase == ScopedChildPhase::Retired && record.retirement_evidence_digest.is_some(),
        "scoped child lacks durable exact retirement"
    );
    if let Some(recovery) = &record.scope_recovery {
        ensure!(
            recovery.matches_allocation(&record.initial.scope_allocation)
                && record.recovery_death_evidence_digest.is_some(),
            "bound scoped child lacks exact scope-death proof"
        );
    } else {
        ensure!(
            record.process_identity.is_none() && record.recovery_death_evidence_digest.is_none(),
            "unbound scoped child retirement contradicts process contact"
        );
    }
    Ok(())
}
