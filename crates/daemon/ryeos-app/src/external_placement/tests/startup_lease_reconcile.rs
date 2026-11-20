//! A restarted controller whose predecessor session holds an unsettled
//! external allocation: the remote lease was terminated out of band, the dead
//! generation's launch claim is cleared, and the session's local settlement is
//! refused by the runtime database until the provider terminal is observed.

use super::*;

const PLACEMENT: &str = "T-one";

struct Restarted {
    _owner: crate::state_store::test_native::TestStateStore,
    store: Arc<crate::state_store::StateStore>,
    lifetime: Arc<StateLockLease>,
    reservation: ExternalAllocationReservation,
    binding: RetainedExternalExecutionBinding,
}

impl Restarted {
    fn prepared(&self, backend: &Arc<FaultBackend>) -> PreparedExternalPlacement {
        prepared_fixture(
            self.store.clone(),
            backend.clone(),
            &self.reservation,
            &self.binding,
            Arc::new(AtomicBool::new(false)),
            self.lifetime.clone(),
        )
    }

    fn phase(&self) -> ExternalAllocationPhase {
        self.store
            .external_allocation(PLACEMENT)
            .unwrap()
            .unwrap()
            .phase
    }

    fn settle_session_locally(&self) -> Result<()> {
        self.store.fail_dedicated_session_start(
            PLACEMENT,
            "worker-one",
            1,
            "ended host lifetime",
            true,
        )
    }

    fn lock_owner(&self) -> Option<String> {
        self.store
            .credential_profile(&format!("P-{PLACEMENT}"))
            .unwrap()
            .unwrap()
            .lock_owner
    }

    /// Own the original provider termination once, then observe it.
    fn reconcile_occurrence(&self, backend: &Arc<FaultBackend>) {
        let ExternalPlacementContactDecision::Reconcile(recovery) =
            self.prepared(backend).claim().unwrap()
        else {
            panic!("the retained allocation lost its reconciliation authority");
        };
        recovery.reconcile().unwrap();
        assert_eq!(self.phase(), ExternalAllocationPhase::Quarantined);
    }
}

/// Reproduces both starts of the wedged controller: the predecessor contacted
/// its provider, start 1 fenced the allocation and cleared the dead
/// generation's launch claim in its own committed step, then died. The store
/// is reopened (start 2's opening validation) with the claim still absent.
fn restarted_with_cleared_claim() -> Restarted {
    let (predecessor_owner, predecessor, reservation, binding, predecessor_lifetime, lock_path) =
        placement_store_fixture();
    let backend = Arc::new(FaultBackend::new());
    let ExternalPlacementContactDecision::Contact(contact) = prepared_fixture(
        predecessor.clone(),
        backend.clone(),
        &reservation,
        &binding,
        Arc::new(AtomicBool::new(false)),
        predecessor_lifetime.clone(),
    )
    .claim()
    .unwrap() else {
        panic!("predecessor did not own the first allocation contact");
    };
    assert!(contact.contact(test_startup_deadline()).is_err());
    drop(predecessor);
    drop(predecessor_lifetime);
    drop(predecessor_owner);

    // Start 1.
    let (first_owner, first, first_lifetime) = reopen_placement_store(&lock_path);
    let app_root = tempfile::tempdir().unwrap();
    let first_state = crate::state::test_support::build_with_store(
        app_root.path(),
        first.clone(),
        first_lifetime.clone(),
    )
    .unwrap();
    assert_eq!(
        fence_external_candidates_after_controller_restart(&first_state).unwrap(),
        1
    );
    // `clear_stale_launch_claims` commits separately from everything after it.
    first
        .with_runtime_sql_for_test(|connection| {
            Ok(connection.execute(
                "DELETE FROM thread_launch_claim WHERE thread_id=?1",
                [PLACEMENT],
            )?)
        })
        .unwrap();
    drop(first_state);
    drop(first);
    drop(first_lifetime);
    drop(first_owner);

    // Start 2 reopens over the cleared claim; opening validates every
    // retained allocation.
    let (owner, store, lifetime) = reopen_placement_store(&lock_path);
    Restarted {
        _owner: owner,
        store,
        lifetime,
        reservation,
        binding,
    }
}

#[test]
fn the_trigger_still_refuses_local_settlement_of_a_session_with_an_unsettled_allocation() {
    let restarted = restarted_with_cleared_claim();
    assert_eq!(restarted.phase(), ExternalAllocationPhase::Quarantined);
    let refusal = format!("{:#}", restarted.settle_session_locally().unwrap_err());
    assert!(
        refusal.contains("external execution blocks local completion and owner replacement"),
        "{refusal}"
    );
    let session = restarted
        .store
        .dedicated_session(PLACEMENT)
        .unwrap()
        .unwrap();
    assert_eq!(session.state, "admitted");
    assert_eq!(session.worker_instance_id.as_deref(), Some("worker-one"));
    assert_eq!(restarted.lock_owner().as_deref(), Some("worker-one"));
    assert!(!restarted.phase().is_settled());
}

#[test]
fn a_start_that_died_after_the_claim_clear_does_not_wedge_the_next() {
    let restarted = restarted_with_cleared_claim();
    // The reopen above passed validation with no claim. Retained validation
    // is still exact: a claim that exists and differs from the workspace's
    // launch owner is a replaced owner, refused.
    restarted
        .store
        .with_runtime_sql_for_test(|connection| {
            Ok(connection.execute(
                "INSERT INTO thread_launch_claim(thread_id,claim_id,claimed_at_ms,
                    lease_expires_at_ms,claimed_by,rearm_resume_budget_if_stale)
                 VALUES(?1,'replacement',0,0,'{\"replacement\":true}',0)",
                [PLACEMENT],
            )?)
        })
        .unwrap();
    let refusal = format!(
        "{:#}",
        restarted
            .store
            .predecessor_external_lease_placements("daemon-next")
            .unwrap_err()
    );
    assert!(
        refusal.contains("no exact locked dedicated-session owner"),
        "{refusal}"
    );
    restarted
        .store
        .with_runtime_sql_for_test(|connection| {
            Ok(connection.execute(
                "DELETE FROM thread_launch_claim WHERE thread_id=?1",
                [PLACEMENT],
            )?)
        })
        .unwrap();
    restarted
        .store
        .predecessor_external_lease_placements("daemon-next")
        .unwrap();
    // Cleanup authority for the retained allocation does not need the claim.
    let backend = Arc::new(FaultBackend::new());
    restarted.reconcile_occurrence(&backend);
}

#[test]
fn a_lease_gone_at_the_provider_settles_the_allocation_then_the_session_without_double_termination()
{
    let restarted = restarted_with_cleared_claim();
    let backend = Arc::new(FaultBackend::new());
    restarted.reconcile_occurrence(&backend);
    // Start-up order: the allocation first. The termination is issued once
    // and its response lost; the intent is durable, so the retry reconciles.
    let ExternalPlacementContactDecision::Reconcile(cleanup) =
        restarted.prepared(&backend).claim().unwrap()
    else {
        panic!("quarantined occurrence lost cleanup authority");
    };
    assert!(cleanup.terminate_or_reconcile().is_err());
    assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
    assert!(restarted.settle_session_locally().is_err());
    let ExternalPlacementContactDecision::Reconcile(cleanup) =
        restarted.prepared(&backend).claim().unwrap()
    else {
        panic!("quarantined occurrence lost exact reconciliation authority");
    };
    assert_eq!(
        cleanup.terminate_or_reconcile().unwrap().phase,
        ExternalAllocationPhase::Terminated
    );
    assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(backend.termination_observations.load(Ordering::SeqCst), 1);

    // Only now does the session settle, and its credential lock goes with it.
    restarted.settle_session_locally().unwrap();
    let session = restarted
        .store
        .dedicated_session(PLACEMENT)
        .unwrap()
        .unwrap();
    assert_eq!(session.state, "terminal");
    assert!(session.worker_instance_id.is_none());
    assert!(restarted.lock_owner().is_none());
    assert!(
        restarted
            .store
            .held_external_lease(PLACEMENT)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_termination_the_provider_has_not_resolved_stays_quarantined_and_is_issued_once() {
    let restarted = restarted_with_cleared_claim();
    let backend = Arc::new(FaultBackend {
        reconciled_termination: "pending",
        ..FaultBackend::new()
    });
    restarted.reconcile_occurrence(&backend);
    for pass in 0..3 {
        let ExternalPlacementContactDecision::Reconcile(cleanup) =
            restarted.prepared(&backend).claim().unwrap()
        else {
            panic!("quarantined occurrence lost cleanup authority");
        };
        let result = cleanup.terminate_or_reconcile();
        if pass == 0 {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap().phase, ExternalAllocationPhase::Quarantined);
        }
        assert_eq!(restarted.phase(), ExternalAllocationPhase::Quarantined);
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
        assert!(restarted.settle_session_locally().is_err());
        assert_eq!(restarted.lock_owner().as_deref(), Some("worker-one"));
    }
    // The provider later resolves it: the same retained intent is observed.
    let resolving = Arc::new(FaultBackend::new());
    let ExternalPlacementContactDecision::Reconcile(cleanup) =
        restarted.prepared(&resolving).claim().unwrap()
    else {
        panic!("quarantined occurrence lost cleanup authority");
    };
    assert_eq!(
        cleanup.terminate_or_reconcile().unwrap().phase,
        ExternalAllocationPhase::Terminated
    );
    assert_eq!(resolving.terminate_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn an_already_recorded_terminal_is_never_terminated_again() {
    let restarted = restarted_with_cleared_claim();
    let backend = Arc::new(FaultBackend::new());
    restarted.reconcile_occurrence(&backend);
    let ExternalPlacementContactDecision::Reconcile(cleanup) =
        restarted.prepared(&backend).claim().unwrap()
    else {
        panic!("quarantined occurrence lost cleanup authority");
    };
    assert!(cleanup.terminate_or_reconcile().is_err());
    let ExternalPlacementContactDecision::Reconcile(cleanup) =
        restarted.prepared(&backend).claim().unwrap()
    else {
        panic!("quarantined occurrence lost reconciliation authority");
    };
    cleanup.terminate_or_reconcile().unwrap();
    let calls = backend.terminate_calls.load(Ordering::SeqCst);
    let observations = backend.termination_observations.load(Ordering::SeqCst);
    // A later pass finds the settled allocation and contacts no one.
    let later = Arc::new(FaultBackend::new());
    assert!(matches!(
        restarted.prepared(&later).claim().unwrap(),
        ExternalPlacementContactDecision::Settled(_)
    ));
    assert_eq!(later.terminate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(later.termination_observations.load(Ordering::SeqCst), 0);
    assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        backend.termination_observations.load(Ordering::SeqCst),
        observations
    );
}
