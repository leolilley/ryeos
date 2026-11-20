//! A placed session's remote lease across controller restarts and turns.
//!
//! Workload: a session whose admitted profile needs a remote lease runs its
//! turns' effects in that lease while its worker runs beside the controller.
//! The worker is a local child of the daemon, so when the daemon restarts the
//! worker is gone, and the lease it was bound to is never reattached: every
//! lease held for a worker of another daemon incarnation is terminated by its
//! occurrence id through the ordinary cleanup path, and its node lease budget
//! is released by the same settlement that records the observed terminal. A
//! session bound to such a lease is interrupted: a later attempt to run it on
//! a new worker is refused plainly, and nothing from the interrupted turn is
//! imported.
//!
//! A lease also ends at a fixed time. A session is placed on a lease only
//! when the lease's earliest end outlives the session's deadline by the
//! node's turn-end margin, the time to freeze, export and import a turn's
//! result; a session that does not fit is refused before any lease is
//! reserved. The same measure is taken again on each route against the lease
//! actually held, as a backstop: a lease that no longer covers the session's
//! deadline is terminated rather than kept once the session is idle, and the
//! route is refused. A session cannot move to a fresh lease, so it continues
//! in a new session.

use super::*;

/// The outcome of terminating the leases of predecessor daemon incarnations.
/// A placement still pending keeps its lease charged and quarantined; the
/// periodic cleanup pass keeps observing it by occurrence id.
#[derive(Debug, Default)]
pub struct PredecessorLeaseTermination {
    pub discovered: usize,
    pub terminated: Vec<String>,
    pub pending: Vec<String>,
    pub failures: Vec<(String, String)>,
}

/// The interruption every session bound to a terminated predecessor lease
/// carries.
pub const PREDECESSOR_LEASE_INTERRUPTION: &str = "session interrupted: the controller restarted, and the remote lease its worker was bound \
     to is terminated, never reattached; nothing from the interrupted turn was imported";

/// The reason a lease of an earlier daemon incarnation is refused with.
const PREDECESSOR_LEASE_REFUSAL: &str = "controller restarted";

/// The reason a lease whose guest never reported ready in time is refused
/// with.
const OVERDUE_LEASE_REFUSAL: &str =
    "the guest did not report ready before the lease's ready deadline";

/// Refuse every activated lease whose ready deadline has passed, durably, and
/// start the cleanup that terminates it. The guest's frames alone cannot do
/// this: a guest that never speaks sends nothing to judge. Returns the
/// placements that could not be refused, with the reason; the periodic pass
/// that calls this tries them again.
pub(super) fn refuse_overdue_external_leases(state: &AppState) -> Result<Vec<(String, String)>> {
    let now_ms = lillux::time::timestamp_millis();
    let mut failures = Vec::new();
    for record in state.state_store.overdue_external_lease_records(now_ms)? {
        if let Err(error) = super::lease_ready_gate::refuse_activated_lease(
            state,
            &record.placement_thread_id,
            OVERDUE_LEASE_REFUSAL,
        ) {
            failures.push((record.placement_thread_id, format!("{error:#}")));
        }
    }
    Ok(failures)
}

/// Drive every lease held for a worker of another daemon incarnation to
/// termination by its occurrence id. Call before the node is ready, when no
/// worker of this incarnation can yet hold a lease. One provider operation is
/// advanced per lease; a lease whose terminal is not yet observed stays
/// quarantined and charged for the periodic cleanup pass.
pub fn terminate_predecessor_external_leases(
    state: &AppState,
) -> Result<PredecessorLeaseTermination> {
    let generation = crate::runtime_db::daemon_generation_id();
    let placements = state
        .state_store
        .predecessor_external_lease_placements(generation)?;
    let records = state
        .state_store
        .predecessor_external_lease_records(generation)?;
    let mut report = PredecessorLeaseTermination {
        discovered: placements.len(),
        ..PredecessorLeaseTermination::default()
    };
    // The guest of an earlier incarnation dialed a controller that is gone.
    // Its lease is condemned before termination begins, so its stream and any
    // retry of its frames are refused for good whatever termination finds.
    for record in records {
        if let Err(error) = state.state_store.refuse_external_lease_and_cancel(
            &record.placement_thread_id,
            PREDECESSOR_LEASE_REFUSAL,
        ) {
            report
                .failures
                .push((record.placement_thread_id, format!("{error:#}")));
        }
    }
    for placement in placements {
        match advance_external_candidate_cleanup(state, &placement) {
            Ok(ExternalCandidateCleanupProgress::Proved) => report.terminated.push(placement),
            Ok(ExternalCandidateCleanupProgress::Pending) => report.pending.push(placement),
            Err(error) => report.failures.push((placement, format!("{error:#}"))),
        }
    }
    Ok(report)
}

/// Refuse to run a session on a new worker over a lease that ended or that
/// its placement holds for anything but an earlier worker of this session.
/// Only a lease of the same session's workspace bound to an earlier worker
/// epoch is fenced for termination first: that worker is gone, so the lease
/// can carry nothing more. Every other mismatch is refused and left
/// untouched, because this session does not own what the placement holds.
pub(super) fn refuse_ended_session_lease(
    state: &AppState,
    existing: &ExternalAllocationRecord,
    workspace_id: &str,
    worker_instance_id: &str,
    worker_boot_epoch: u64,
) -> Result<()> {
    match ended_session_lease(
        existing,
        workspace_id,
        worker_instance_id,
        worker_boot_epoch,
    ) {
        SessionLeaseStanding::Current => Ok(()),
        SessionLeaseStanding::Refused(reason) => bail!("{reason}"),
        SessionLeaseStanding::FenceThenRefuse(reason) => {
            request_external_candidate_cleanup(state, &existing.reservation.placement_thread_id)
                .context("fence the remote lease of an earlier worker for termination")?;
            bail!("{reason}")
        }
    }
}

/// How a placement's retained allocation stands for a worker of the session
/// now asking to run on it.
#[derive(Debug, PartialEq, Eq)]
enum SessionLeaseStanding {
    /// The allocation is this worker's own.
    Current,
    /// Refused; the allocation is left as it is.
    Refused(String),
    /// Refused after the allocation, an earlier worker's of this session, is
    /// fenced for termination.
    FenceThenRefuse(String),
}

fn ended_session_lease(
    existing: &ExternalAllocationRecord,
    workspace_id: &str,
    worker_instance_id: &str,
    worker_boot_epoch: u64,
) -> SessionLeaseStanding {
    if existing.phase.is_settled() {
        return SessionLeaseStanding::Refused(
            "session interrupted: its remote execution has ended and is never reattached; \
             nothing from an interrupted turn was imported; continue in a new session"
                .to_owned(),
        );
    }
    let ExternalAllocationOwner::DedicatedSession(owner) = &existing.reservation.owner else {
        return SessionLeaseStanding::Refused(
            "session refused: its placement holds another owner's remote execution, which is \
             left untouched"
                .to_owned(),
        );
    };
    if owner.workspace_id != workspace_id {
        return SessionLeaseStanding::Refused(format!(
            "session refused: its placement holds the remote execution of workspace `{}`, not \
             this session's workspace `{workspace_id}`; it is left untouched",
            owner.workspace_id
        ));
    }
    if owner.worker_instance_id == worker_instance_id
        && owner.worker_boot_epoch == worker_boot_epoch
    {
        return SessionLeaseStanding::Current;
    }
    if owner.worker_boot_epoch < worker_boot_epoch {
        return SessionLeaseStanding::FenceThenRefuse(format!(
            "session interrupted: its remote execution was bound to worker epoch {}, which has \
             ended; it is terminated, never reattached to a later worker, and nothing from an \
             interrupted turn was imported; continue in a new session",
            owner.worker_boot_epoch
        ));
    }
    SessionLeaseStanding::Refused(format!(
        "session refused: its remote execution is bound to worker `{}` at epoch {}, which is not \
         earlier than this worker's epoch {worker_boot_epoch}; it is left untouched",
        owner.worker_instance_id, owner.worker_boot_epoch
    ))
}

/// Refuse, before a fresh lease is admitted, a placed session whose deadline
/// the lease could not cover. What is measured: the session's deadline, its
/// own bounded lifetime or its aggregate budget's deadline, whichever comes
/// first, plus the node's turn-end margin, against the earliest end the lease
/// can have, now plus its lifetime. The lease is reserved after this check
/// and created after its reservation, so it ends no earlier than that. Every
/// input is fixed for the session, so a session refused here could never
/// start a turn on the lease; refusing now costs no vault read, reservation
/// or provider contact.
pub(super) fn refuse_session_unfit_for_fresh_lease(
    session_deadline_at_ms: Option<i64>,
    lifetime_seconds: u32,
    margin_seconds: u32,
    now_ms: i64,
) -> Result<()> {
    let earliest_lease_end_ms = now_ms
        .checked_add(i64::from(lifetime_seconds) * 1_000)
        .context("lease end overflows the timestamp range")?;
    deadline_fits_lease(
        "lease refused",
        earliest_lease_end_ms,
        session_deadline_at_ms,
        margin_seconds,
        now_ms,
    )
}

/// Admit a route on the placement's lease, if it holds one: the same measure
/// as at placement, the session's deadline plus the node's turn-end margin
/// against the end of the lease actually held. It is a backstop for a lease
/// whose standing changed after placement: its assumed end moved, or node
/// policy no longer declares a lease budget or now declares a larger margin.
/// A refused route on an idle session fences the lease for termination,
/// because no later route of this session can pass either and the session
/// cannot move to a fresh lease. A refused route on a session with a turn in
/// progress leaves the lease to that turn.
pub(crate) fn admit_turn_on_external_lease(
    state: &AppState,
    placement: &str,
    session_deadline_at_ms: Option<i64>,
    session_idle: bool,
) -> Result<()> {
    let Some(record) = state.state_store.external_allocation(placement)? else {
        return Ok(());
    };
    if record.reservation.admitted_adapter_request_base64.is_none() {
        return Ok(());
    }
    let Some(lease) = state.state_store.held_external_lease(placement)? else {
        bail!(
            "turn refused: this session's remote lease has ended; a session cannot move to a \
             fresh lease, so continue in a new session"
        );
    };
    let margin_seconds = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy>()?
        .external_lease_budget
        .as_ref()
        .map(|budget| budget.turn_end_margin_seconds);
    let now_ms = lillux::time::timestamp_millis();
    let refusal = match margin_seconds {
        Some(margin_seconds) => deadline_fits_lease(
            "turn refused",
            lease.assumed_end_ms()?,
            session_deadline_at_ms,
            margin_seconds,
            now_ms,
        )
        .err(),
        None => Some(anyhow::anyhow!(
            "turn refused: this node's execution policy no longer declares an external lease \
             budget, so it holds no turn-end margin to admit a turn on a remote lease"
        )),
    };
    let Some(refusal) = refusal else {
        return Ok(());
    };
    if !session_idle {
        return Err(refusal.context(
            "the lease is left to the turn in progress; no further turn can start on it",
        ));
    }
    request_external_candidate_cleanup(state, placement)
        .context("fence the remote lease that cannot carry a turn for termination")?;
    Err(refusal.context(
        "the lease is being terminated; a session cannot move to a fresh lease, so continue in \
         a new session",
    ))
}

/// The session's deadline fits a lease ending at `lease_end_ms` when the
/// deadline plus the turn-end margin, the time to freeze, export and import a
/// turn's result, is no later than that end. A session with no deadline
/// never fits: the lease ends at a fixed time and an unbounded turn could
/// outlive it. `refused` names what is refused.
fn deadline_fits_lease(
    refused: &str,
    lease_end_ms: i64,
    session_deadline_at_ms: Option<i64>,
    margin_seconds: u32,
    now_ms: i64,
) -> Result<()> {
    let Some(session_deadline_at_ms) = session_deadline_at_ms else {
        bail!(
            "{refused}: a session on a remote lease must have a deadline and this one has none, \
             so a turn could outlive the lease"
        );
    };
    let needed_end_ms = session_deadline_at_ms
        .checked_add(i64::from(margin_seconds) * 1_000)
        .context("session deadline plus margin overflows the timestamp range")?;
    let seconds_until = |instant_ms: i64| (instant_ms.saturating_sub(now_ms).max(0) + 999) / 1_000;
    ensure!(
        needed_end_ms <= lease_end_ms,
        "{refused}: the remote lease ends in {}s, and this session's deadline is {}s away; the \
         lease must outlive that deadline by the node's {}s turn-end margin to freeze, export \
         and import a turn's result",
        seconds_until(lease_end_ms),
        seconds_until(session_deadline_at_ms),
        margin_seconds
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASE_START_MS: i64 = 1_000_000;
    const LEASE_END_MS: i64 = LEASE_START_MS + 4 * 60 * 60 * 1_000;

    #[test]
    fn a_session_fits_a_lease_only_when_the_lease_outlives_its_deadline_by_the_margin() {
        let end = LEASE_END_MS;
        let now = end - 60 * 60 * 1_000;
        let margin = 600;
        let fits = |deadline| deadline_fits_lease("turn refused", end, deadline, margin, now);
        // A deadline 45 minutes away, with ten minutes to spare before the
        // lease's last hour ends.
        fits(Some(now + 45 * 60 * 1_000)).unwrap();
        // Exactly at the margin still fits.
        fits(Some(end - 600_000)).unwrap();
        let refusal = fits(Some(end - 599_000)).unwrap_err().to_string();
        assert!(
            refusal.starts_with("turn refused: the remote lease ends in 3600s")
                && refusal.contains("deadline is 3001s away")
                && refusal.contains("600s turn-end margin"),
            "{refusal}"
        );
        let refusal = fits(None).unwrap_err().to_string();
        assert!(refusal.contains("this one has none"), "{refusal}");
    }

    #[test]
    fn a_lease_past_its_end_admits_no_route() {
        let refusal = deadline_fits_lease(
            "turn refused",
            LEASE_END_MS,
            Some(LEASE_END_MS + 60_000),
            1,
            LEASE_END_MS + 1,
        )
        .unwrap_err()
        .to_string();
        assert!(refusal.contains("ends in 0s"), "{refusal}");
    }

    #[test]
    fn placement_measures_the_session_deadline_against_now_plus_the_lease_lifetime() {
        let now = LEASE_START_MS;
        // A one-hour lease covers a deadline 50 minutes away with a
        // ten-minute margin, and no later one.
        refuse_session_unfit_for_fresh_lease(Some(now + 50 * 60 * 1_000), 3_600, 600, now).unwrap();
        let refusal =
            refuse_session_unfit_for_fresh_lease(Some(now + 50 * 60 * 1_000 + 1), 3_600, 600, now)
                .unwrap_err()
                .to_string();
        assert!(
            refusal.starts_with("lease refused: the remote lease ends in 3600s"),
            "{refusal}"
        );
        let refusal = refuse_session_unfit_for_fresh_lease(None, 3_600, 600, now)
            .unwrap_err()
            .to_string();
        assert!(
            refusal.starts_with("lease refused") && refusal.contains("this one has none"),
            "{refusal}"
        );
    }

    fn record(
        phase: ExternalAllocationPhase,
        workspace: &str,
        worker: &str,
        epoch: u64,
    ) -> ExternalAllocationRecord {
        ExternalAllocationRecord {
            reservation: ExternalAllocationReservation {
                schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
                placement_thread_id: "T-one".into(),
                admitted_capsule_hash: "a".repeat(64),
                owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                    workspace_id: workspace.into(),
                    worker_instance_id: worker.into(),
                    worker_boot_epoch: epoch,
                }),
                base_snapshot_hash: "b".repeat(64),
                binding_hash: "c".repeat(64),
                capacity_owner: "d".repeat(64),
                channel_authority_generation: "3".repeat(64),
                channel_owner_public_key: "key".into(),
                channel_bootstrap_capability_hash: "4".repeat(64),
                admitted_adapter_request_base64: None,
                request_digest: "e".repeat(64),
                max_active: 1,
                timeout_seconds: 60,
                contact_deadline_ms: 2,
                startup_started_at_ms: 1,
                startup_deadline_ms: 2,
            },
            phase,
            occurrence: None,
        }
    }

    fn reason(standing: SessionLeaseStanding) -> (bool, String) {
        match standing {
            SessionLeaseStanding::Current => panic!("the allocation was judged current"),
            SessionLeaseStanding::Refused(reason) => (false, reason),
            SessionLeaseStanding::FenceThenRefuse(reason) => (true, reason),
        }
    }

    #[test]
    fn only_an_earlier_worker_of_the_same_workspace_is_fenced() {
        use ExternalAllocationPhase::{Bound, Terminated};
        assert_eq!(
            ended_session_lease(&record(Bound, "W-one", "W1", 1), "W-one", "W1", 1),
            SessionLeaseStanding::Current
        );
        let (fenced, earlier) = reason(ended_session_lease(
            &record(Bound, "W-one", "W1", 1),
            "W-one",
            "W2",
            2,
        ));
        assert!(fenced);
        assert!(
            earlier.contains("bound to worker epoch 1, which has ended")
                && earlier.contains("never reattached")
                && earlier.contains("nothing from an interrupted turn was imported"),
            "{earlier}"
        );
        let (fenced, other_workspace) = reason(ended_session_lease(
            &record(Bound, "W-two", "W1", 1),
            "W-one",
            "W2",
            2,
        ));
        assert!(!fenced);
        assert!(
            other_workspace.contains("workspace `W-two`, not this session's workspace `W-one`")
                && other_workspace.contains("left untouched"),
            "{other_workspace}"
        );
        // The same epoch under another worker, or a later epoch, is not an
        // earlier worker of this session.
        for (worker, epoch) in [("W9", 2), ("W9", 3)] {
            let (fenced, not_earlier) = reason(ended_session_lease(
                &record(Bound, "W-one", worker, epoch),
                "W-one",
                "W2",
                2,
            ));
            assert!(!fenced);
            assert!(
                not_earlier.contains("not earlier than this worker's epoch 2"),
                "{not_earlier}"
            );
        }
        let (fenced, ended) = reason(ended_session_lease(
            &record(Terminated, "W-one", "W1", 1),
            "W-one",
            "W1",
            1,
        ));
        assert!(!fenced);
        assert!(
            ended.contains("has ended and is never reattached"),
            "{ended}"
        );
    }

    #[test]
    fn another_owners_allocation_is_refused_without_a_fence() {
        let mut direct = record(ExternalAllocationPhase::Bound, "W-one", "W1", 1);
        direct.reservation.owner = ExternalAllocationOwner::DirectThread {
            chain_root_id: "T-one".into(),
            launch_owner: crate::runtime_db::LaunchOwner {
                thread_id: "T-one".into(),
                monotonic_launch_epoch: 1,
                unpredictable_nonce: "nonce".into(),
                daemon_generation_id: "daemon".into(),
            },
            program: crate::thread_lifecycle::external_direct_program_test_fixture(),
        };
        let (fenced, foreign) = reason(ended_session_lease(&direct, "W-one", "W2", 2));
        assert!(!fenced);
        assert!(
            foreign.contains("another owner's remote execution"),
            "{foreign}"
        );
    }
}
