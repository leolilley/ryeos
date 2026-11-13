//! The periodic cleanup pass against a real retained placement: an
//! identically failing cleanup is deferred by back-off and stays quarantined.

use super::*;

#[test]
fn periodic_cleanup_defers_a_repeated_identical_failure_and_keeps_it_quarantined() {
    let (predecessor_owner, predecessor, reservation, _, predecessor_lifetime, lock_path) =
        placement_store_fixture();
    let placement = reservation.placement_thread_id.clone();
    assert!(matches!(
        predecessor
            .claim_external_allocation_contact(&placement, &reservation.request_digest)
            .unwrap(),
        ExternalAllocationContactClaim::Contact(_)
    ));
    drop(predecessor);
    drop(predecessor_lifetime);
    drop(predecessor_owner);
    let (_reopened_owner, reopened, replacement_lifetime) = reopen_placement_store(&lock_path);
    let app_root = tempfile::tempdir().unwrap();
    let replacement = crate::state::test_support::build_with_store(
        app_root.path(),
        reopened.clone(),
        replacement_lifetime.clone(),
    )
    .unwrap();
    assert_eq!(
        fence_external_candidates_after_controller_restart(&replacement).unwrap(),
        1
    );

    let clock = cleanup_backoff::ManualClock::new();
    let mut backoff = cleanup_backoff::RetainedCleanupBackoff::with_clock(clock.clock());
    let attempted = |report: &ExternalCandidateCleanupRecovery| {
        report
            .failures
            .iter()
            .any(|(failed, _)| failed == &placement)
    };
    // The fixture has no admitted program to clean up with, so every
    // attempt fails the same way.
    let first = recover_external_candidate_cleanups(&replacement, &mut backoff).unwrap();
    assert!(attempted(&first) && first.deferred.is_empty(), "{first:?}");
    clock.advance(cleanup_backoff::RETAINED_CLEANUP_BACKOFF_BASE);
    let second = recover_external_candidate_cleanups(&replacement, &mut backoff).unwrap();
    assert!(
        attempted(&second) && second.deferred.is_empty(),
        "{second:?}"
    );
    clock.advance(std::time::Duration::from_secs(10));
    let third = recover_external_candidate_cleanups(&replacement, &mut backoff).unwrap();
    assert!(!attempted(&third), "{third:?}");
    assert_eq!(third.discovered, 1);
    assert_eq!(
        third.deferred,
        vec![cleanup_backoff::DeferredCleanup {
            lane: cleanup_backoff::CleanupLane::QuarantinedAllocation,
            placement: placement.clone(),
            consecutive_failures: 2,
            retry_in: std::time::Duration::from_secs(230),
        }]
    );
    // Deferral is not settlement: the obligation stays retained.
    assert_eq!(
        reopened
            .external_allocation(&placement)
            .unwrap()
            .unwrap()
            .phase,
        ExternalAllocationPhase::Quarantined
    );
    assert_eq!(
        reopened.recoverable_external_cleanup_placements().unwrap(),
        vec![placement.clone()]
    );
    // A restarted daemon starts with a fresh back-off and attempts it.
    let restarted = recover_external_candidate_cleanups(
        &replacement,
        &mut cleanup_backoff::RetainedCleanupBackoff::default(),
    )
    .unwrap();
    assert!(
        attempted(&restarted) && restarted.deferred.is_empty(),
        "{restarted:?}"
    );
}
