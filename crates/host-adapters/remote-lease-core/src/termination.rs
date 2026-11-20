//! When elapsed lease time may stand in for a provider's terminal report.
//!
//! A provider that has forgotten a lease answers not-found for it. That
//! answer alone proves nothing (the id may be wrong, the owner may differ),
//! but once the lease could no longer be running by its own bounded lifetime
//! the same definitive answer is the provider's reaped lease. The controller
//! states the latest moment the lease could last; this module decides whether
//! that moment, plus a grace for provider reaping lag and clock skew, has
//! passed on the adapter's wall clock.

use ryeos_external_execution_contract::LatestLeaseEnd;

/// How long after a lease's latest end an adapter waits before a definitive
/// not-found counts as the lease having ended. Ten minutes covers a provider
/// reaping a lease after its timeout and the clock skew between the
/// controller that derived the end and this adapter's clock.
pub const LEASE_END_GRACE_MS: i64 = 10 * 60 * 1000;

/// Whether `end` plus [`LEASE_END_GRACE_MS`] has passed on the wall clock now.
/// False for an unbounded lease and when the clock reads no time after the
/// epoch, so an unreadable clock leaves termination pending.
pub fn lease_end_passed(end: &LatestLeaseEnd) -> bool {
    lease_end_passed_at(end, lillux::time::timestamp_millis())
}

/// [`lease_end_passed`] against an explicit clock reading.
pub fn lease_end_passed_at(end: &LatestLeaseEnd, now_unix_ms: i64) -> bool {
    end.is_past_with_grace(now_unix_ms, LEASE_END_GRACE_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_only_strictly_after_end_plus_grace() {
        let end = LatestLeaseEnd::Bounded { unix_ms: 1_000_000 };
        let limit = 1_000_000 + LEASE_END_GRACE_MS;
        assert!(!lease_end_passed_at(&end, limit));
        assert!(lease_end_passed_at(&end, limit + 1));
        assert!(!lease_end_passed_at(&end, 1_000_000));
        assert!(!lease_end_passed_at(&end, 0));
        assert!(!lease_end_passed_at(
            &LatestLeaseEnd::Unbounded {},
            i64::MAX
        ));
    }
}
