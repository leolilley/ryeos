//! Settlement of Q local custody a previous daemon generation left open.
//!
//! A Q private workspace and a Q guest-input scratch directory are held by
//! the daemon generation that created them, through pinned descriptors in
//! its process-local custody registry. When that generation dies with a
//! placement in flight, the rows stay open: no later generation holds the
//! descriptors, so the ordinary cleanup, which proves removal through them,
//! can never run, and until now only the execution-history reset could
//! retire such rows. This pass gives a running daemon the same settlement
//! under the same exclusion: it holds the exclusive state lock, so it is the
//! only writer of the node-private parents; the creating generation is dead
//! (its id names a process that this daemon's start reconciled, and any
//! Worker it attached was proved dead and settled before this pass runs);
//! and each directory is reached by its recorded coordinate under the
//! node-private parent, with the recorded project identity where the row
//! has one. A scratch is settled only once its allocation is settled or
//! absent, as its input bytes are evidence while a provider contact is open.
//!
//! Rows of this generation are never touched here: their original custody
//! drives their cleanup, and the runtime store refuses a prior-generation
//! transition on them.

use anyhow::{Context, Result};

use crate::runtime_db::external_execution::{
    QualificationGuestScratchPhase, QualificationGuestScratchRecord, QualificationLocalStartPhase,
    QualificationLocalStartRecord,
};
use crate::runtime_db::qualification_cold_custody::QualificationCustodyParents;
use crate::state::AppState;

/// What one pass settled, and what it had to leave for a later pass.
#[derive(Debug, Default)]
pub struct PriorGenerationSettlementReport {
    pub local_starts_closed: usize,
    pub scratch_closed: usize,
    /// Rows left open with the reason, each retried by the next pass.
    pub failures: Vec<(String, String)>,
}

/// Close every open Q local start and guest scratch row a previous daemon
/// generation created, removing their directories by recorded coordinate.
pub fn settle_prior_generation_qualification_custody(
    state: &AppState,
) -> Result<PriorGenerationSettlementReport> {
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let mut report = PriorGenerationSettlementReport::default();
    let local_starts = state
        .state_store
        .prior_generation_qualification_local_starts()?;
    let scratches = state
        .state_store
        .prior_generation_open_qualification_guest_scratch()?;
    if local_starts
        .iter()
        .all(|r| r.phase == QualificationLocalStartPhase::Closed)
        && scratches.is_empty()
    {
        return Ok(report);
    }
    let parents = state.state_store.qualification_custody_parents()?;
    for record in local_starts {
        if record.phase == QualificationLocalStartPhase::Closed {
            continue;
        }
        let placement = record.placement_thread_id.clone();
        match settle_local_start(state, &parents, record) {
            Ok(()) => report.local_starts_closed += 1,
            Err(error) => report.failures.push((placement, format!("{error:#}"))),
        }
    }
    for record in scratches {
        let placement = record.placement_thread_id.clone();
        match settle_scratch(state, &parents, record) {
            Ok(()) => report.scratch_closed += 1,
            Err(error) => report.failures.push((placement, format!("{error:#}"))),
        }
    }
    if report.local_starts_closed + report.scratch_closed > 0 {
        tracing::info!(
            local_starts_closed = report.local_starts_closed,
            scratch_closed = report.scratch_closed,
            "settled Q local custody a previous daemon generation left open"
        );
    }
    Ok(report)
}

/// Drive one prior-generation local start from wherever it stopped to Closed.
/// Credential custody must already be released (the start's reconcile does
/// that once the Worker is proved dead); a live custody leaves the row for a
/// later pass rather than racing that release.
fn settle_local_start(
    state: &AppState,
    parents: &QualificationCustodyParents,
    mut current: QualificationLocalStartRecord,
) -> Result<()> {
    let placement = current.placement_thread_id.clone();
    anyhow::ensure!(
        current
            .credential_custody
            .as_ref()
            .is_none_or(|custody| custody.released_at_ms.is_some()),
        "its credential custody is still held; the start's reconcile releases it first"
    );
    let worker = state.state_store.qualification_worker_process(&placement)?;
    crate::external_qualification_local::settle_unconsumed_directory_borrow_if_unheld(
        state,
        &placement,
        &current,
        worker.as_ref(),
    )?;
    if current
        .directory_borrow_intent
        .as_ref()
        .is_some_and(|borrow| borrow.released_at_ms.is_none())
        && current.worker_incarnation_digest.is_none()
    {
        // The settlement above wrote the row; continue from what it wrote.
        current = state
            .state_store
            .qualification_local_start(&placement)?
            .context("Q local start vanished during its borrow settlement")?;
    }
    loop {
        current = match current.phase {
            QualificationLocalStartPhase::Reserved
            | QualificationLocalStartPhase::WorkspaceReady
            | QualificationLocalStartPhase::WorkerAttached
            | QualificationLocalStartPhase::CleanupUnproved => state
                .state_store
                .begin_qualification_local_cleanup_prior_generation(&current)?,
            QualificationLocalStartPhase::CleanupIntent => {
                state
                    .state_store
                    .authorize_qualification_workspace_removal_prior_generation(&current)?;
                parents.settle_local_start_workspace(&current)?;
                state
                    .state_store
                    .record_qualification_workspace_removed_prior_generation(&current)?
            }
            QualificationLocalStartPhase::WorkspaceRemoved => {
                parents.settle_local_start_state_root(&current)?;
                state
                    .state_store
                    .record_qualification_state_root_removed_prior_generation(&current)?
            }
            QualificationLocalStartPhase::StateRemoved => state
                .state_store
                .close_qualification_local_start_prior_generation(&current)?,
            QualificationLocalStartPhase::Closed => return Ok(()),
        };
    }
}

/// Drive one prior-generation scratch from wherever it stopped to Closed. The
/// cleanup intent freezes the settled-allocation basis and refuses while the
/// allocation is unsettled; that refusal leaves the row for a later pass.
fn settle_scratch(
    state: &AppState,
    parents: &QualificationCustodyParents,
    mut current: QualificationGuestScratchRecord,
) -> Result<()> {
    loop {
        current = match current.phase {
            QualificationGuestScratchPhase::Reserved | QualificationGuestScratchPhase::Created => {
                state
                    .state_store
                    .begin_qualification_guest_scratch_cleanup_prior_generation(&current)?
            }
            QualificationGuestScratchPhase::CleanupIntent => {
                parents.settle_scratch(&current, true)?;
                state
                    .state_store
                    .record_qualification_guest_scratch_removed_prior_generation(&current)?
            }
            QualificationGuestScratchPhase::Removed => state
                .state_store
                .close_qualification_guest_scratch_prior_generation(&current)?,
            QualificationGuestScratchPhase::Closed => return Ok(()),
        };
    }
}
