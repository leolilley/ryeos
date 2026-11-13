//! Back-off for a retained external cleanup that keeps failing the same way.
//!
//! The periodic recovery pass re-attempts every retained cleanup obligation.
//! Without memory, a cleanup that fails identically on every pass spends the
//! controller's prepaid work and store holds on every tick and learns
//! nothing. This unit remembers, per cleanup, the last failure and when the
//! cleanup may be attempted again. It changes only how often an identical
//! failure is re-attempted: the obligation itself stays durably retained and
//! charged, and a deferred cleanup is reported, never hidden.
//!
//! The state is process-local on purpose. A daemon restart starts empty, so
//! the first pass of a new generation attempts every cleanup; a restart is
//! exactly when a wedge may have cleared. Durable state is never touched here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Delay after a first failure, or after a failure with a new cause: one
/// periodic recovery tick, so a single transient failure retries as before.
pub const RETAINED_CLEANUP_BACKOFF_BASE: Duration = Duration::from_secs(120);

/// Longest delay between attempts of an identically failing cleanup.
pub const RETAINED_CLEANUP_BACKOFF_CAP: Duration = Duration::from_secs(32 * 60);

/// A recovery tick that fires a moment before the recorded attempt time must
/// not skip a whole period.
const SCHEDULING_SLACK: Duration = Duration::from_secs(10);

/// Monotonic time source. Production reads [`Instant::now`]; tests inject a
/// clock they advance by hand.
pub type CleanupClock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The independent cleanup loops of one recovery pass. A failure in one lane
/// never defers the other lane's cleanup of the same placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CleanupLane {
    QuarantinedAllocation,
    GuestScratch,
}

impl CleanupLane {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QuarantinedAllocation => "quarantined_allocation",
            Self::GuestScratch => "guest_scratch",
        }
    }
}

/// A cleanup the pass did not attempt because its back-off has not expired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredCleanup {
    pub lane: CleanupLane,
    pub placement: String,
    pub consecutive_failures: u32,
    pub retry_in: Duration,
}

struct Entry {
    consecutive_failures: u32,
    failure: String,
    next_attempt: Instant,
}

/// Per-cleanup back-off state, owned by the periodic recovery loop.
pub struct RetainedCleanupBackoff {
    clock: CleanupClock,
    entries: HashMap<CleanupLane, HashMap<String, Entry>>,
}

impl Default for RetainedCleanupBackoff {
    fn default() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }
}

impl RetainedCleanupBackoff {
    pub fn with_clock(clock: CleanupClock) -> Self {
        Self {
            clock,
            entries: HashMap::new(),
        }
    }

    /// Start one lane of a pass over the cleanups its query returned now.
    /// Entries for cleanups no longer returned are pruned, and the pass reads
    /// the clock once so every decision in it uses the same instant.
    pub fn lane(&mut self, lane: CleanupLane, present: &[String]) -> LanePass<'_> {
        let present: HashSet<&str> = present.iter().map(String::as_str).collect();
        if let Some(entries) = self.entries.get_mut(&lane) {
            entries.retain(|placement, _| present.contains(placement.as_str()));
        }
        LanePass {
            now: (self.clock)(),
            lane,
            backoff: self,
        }
    }
}

/// Delay before the next attempt after `consecutive_failures` identical
/// failures: BASE, doubling per identical failure, never above CAP.
pub fn delay_after(consecutive_failures: u32) -> Duration {
    let doublings = consecutive_failures.saturating_sub(1).min(31);
    RETAINED_CLEANUP_BACKOFF_BASE
        .saturating_mul(1u32 << doublings)
        .min(RETAINED_CLEANUP_BACKOFF_CAP)
}

/// One lane of one recovery pass.
pub struct LanePass<'a> {
    backoff: &'a mut RetainedCleanupBackoff,
    lane: CleanupLane,
    now: Instant,
}

impl LanePass<'_> {
    /// `Some` when this cleanup is still backing off and must not be
    /// attempted in this pass.
    pub fn defer(&self, placement: &str) -> Option<DeferredCleanup> {
        let entry = self.backoff.entries.get(&self.lane)?.get(placement)?;
        (self.now + SCHEDULING_SLACK < entry.next_attempt).then(|| DeferredCleanup {
            lane: self.lane,
            placement: placement.to_owned(),
            consecutive_failures: entry.consecutive_failures,
            retry_in: entry.next_attempt - self.now,
        })
    }

    /// Record an attempt. Any success (proof or progress) clears the entry.
    /// A failure with the same text as the previous one doubles the delay; a
    /// failure with a new cause is new information and restarts at BASE.
    pub fn record<T>(&mut self, placement: &str, outcome: &anyhow::Result<T>) {
        let entries = self.backoff.entries.entry(self.lane).or_default();
        let Err(error) = outcome else {
            entries.remove(placement);
            return;
        };
        let failure = format!("{error:#}");
        let consecutive_failures = match entries.get(placement) {
            Some(previous) if previous.failure == failure => {
                previous.consecutive_failures.saturating_add(1)
            }
            _ => 1,
        };
        let retry_in = delay_after(consecutive_failures);
        if consecutive_failures > 1 {
            // Wall time for the operator only; `retry_in` is at most CAP.
            let next_attempt_at =
                chrono::Utc::now() + chrono::Duration::seconds(retry_in.as_secs() as i64);
            tracing::warn!(
                lane = self.lane.as_str(),
                placement,
                consecutive_failures,
                retry_in_secs = retry_in.as_secs(),
                next_attempt_at = %next_attempt_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "retained external cleanup failed identically again; deferring it by back-off"
            );
        }
        entries.insert(
            placement.to_owned(),
            Entry {
                consecutive_failures,
                failure,
                next_attempt: self.now + retry_in,
            },
        );
    }
}

/// A hand-advanced clock for tests.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ManualClock(Arc<std::sync::Mutex<Instant>>);

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(Instant::now())))
    }

    pub(crate) fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }

    pub(crate) fn clock(&self) -> CleanupClock {
        let now = Arc::clone(&self.0);
        Arc::new(move || *now.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANE: CleanupLane = CleanupLane::QuarantinedAllocation;

    fn fail(message: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("{message}"))
    }

    fn placements(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Run one pass of `lane` over `present`. `None` means `placement` was
    /// attempted and `outcome` recorded; `Some` is its deferral.
    fn pass(
        backoff: &mut RetainedCleanupBackoff,
        lane: CleanupLane,
        present: &[&str],
        placement: &str,
        outcome: anyhow::Result<()>,
    ) -> Option<DeferredCleanup> {
        let mut lane_pass = backoff.lane(lane, &placements(present));
        if let Some(deferred) = lane_pass.defer(placement) {
            return Some(deferred);
        }
        lane_pass.record(placement, &outcome);
        None
    }

    #[test]
    fn identical_failure_doubles_the_delay_up_to_the_cap() {
        let expected = [120, 240, 480, 960, 1920, 1920, 1920];
        for (index, secs) in expected.into_iter().enumerate() {
            assert_eq!(
                delay_after(index as u32 + 1),
                Duration::from_secs(secs),
                "failure {}",
                index + 1
            );
        }
        assert_eq!(delay_after(u32::MAX), RETAINED_CLEANUP_BACKOFF_CAP);

        let clock = ManualClock::new();
        let mut backoff = RetainedCleanupBackoff::with_clock(clock.clock());
        for (index, secs) in expected.into_iter().enumerate() {
            let attempted = pass(&mut backoff, LANE, &["P"], "P", fail("wedged"));
            assert_eq!(attempted, None, "failure {} was attempted", index + 1);
            // Just before the delay expires the cleanup is deferred, and the
            // deferral names how many identical failures it has seen.
            clock.advance(Duration::from_secs(secs) - SCHEDULING_SLACK - Duration::from_secs(1));
            let deferred = pass(&mut backoff, LANE, &["P"], "P", fail("wedged"))
                .expect("deferred before its delay expired");
            assert_eq!(deferred.consecutive_failures, index as u32 + 1);
            assert_eq!(deferred.placement, "P");
            assert_eq!(deferred.lane, LANE);
            assert_eq!(deferred.retry_in, SCHEDULING_SLACK + Duration::from_secs(1));
            clock.advance(Duration::from_secs(1));
        }
    }

    #[test]
    fn a_first_failure_retries_on_the_next_tick() {
        let clock = ManualClock::new();
        let mut backoff = RetainedCleanupBackoff::with_clock(clock.clock());
        assert_eq!(pass(&mut backoff, LANE, &["P"], "P", fail("once")), None);
        // A tick that fires slightly early still attempts it.
        clock.advance(RETAINED_CLEANUP_BACKOFF_BASE - Duration::from_secs(1));
        assert_eq!(pass(&mut backoff, LANE, &["P"], "P", Ok(())), None);
    }

    #[test]
    fn a_different_cause_restarts_the_schedule() {
        let clock = ManualClock::new();
        let mut backoff = RetainedCleanupBackoff::with_clock(clock.clock());
        for _ in 0..3 {
            assert_eq!(pass(&mut backoff, LANE, &["P"], "P", fail("cause A")), None);
            clock.advance(RETAINED_CLEANUP_BACKOFF_CAP);
        }
        assert_eq!(pass(&mut backoff, LANE, &["P"], "P", fail("cause B")), None);
        clock.advance(Duration::from_secs(60));
        let deferred = pass(&mut backoff, LANE, &["P"], "P", fail("cause B")).unwrap();
        assert_eq!(deferred.consecutive_failures, 1);
        assert_eq!(deferred.retry_in, Duration::from_secs(60));
    }

    #[test]
    fn success_clears_the_entry() {
        let clock = ManualClock::new();
        let mut backoff = RetainedCleanupBackoff::with_clock(clock.clock());
        for _ in 0..4 {
            assert_eq!(pass(&mut backoff, LANE, &["P"], "P", fail("wedged")), None);
            clock.advance(RETAINED_CLEANUP_BACKOFF_CAP);
        }
        // Proof or progress: either is an `Ok` attempt.
        assert_eq!(pass(&mut backoff, LANE, &["P"], "P", Ok(())), None);
        assert!(backoff.entries[&LANE].is_empty());
        // The same failure afterwards starts again at BASE.
        assert_eq!(pass(&mut backoff, LANE, &["P"], "P", fail("wedged")), None);
        clock.advance(Duration::from_secs(60));
        let deferred = pass(&mut backoff, LANE, &["P"], "P", fail("wedged")).unwrap();
        assert_eq!(deferred.consecutive_failures, 1);
    }

    #[test]
    fn a_placement_gone_from_the_query_is_pruned() {
        let clock = ManualClock::new();
        let mut backoff = RetainedCleanupBackoff::with_clock(clock.clock());
        assert_eq!(
            pass(&mut backoff, LANE, &["P", "Q"], "P", fail("wedged")),
            None
        );
        assert_eq!(
            pass(&mut backoff, LANE, &["P", "Q"], "Q", fail("wedged")),
            None
        );
        backoff.lane(LANE, &placements(&["Q"]));
        assert!(!backoff.entries[&LANE].contains_key("P"));
        assert!(backoff.entries[&LANE].contains_key("Q"));
    }

    #[test]
    fn lanes_are_independent() {
        let clock = ManualClock::new();
        let mut backoff = RetainedCleanupBackoff::with_clock(clock.clock());
        let scratch = CleanupLane::GuestScratch;
        assert_eq!(
            pass(&mut backoff, scratch, &["P"], "P", fail("scratch")),
            None
        );
        assert!(pass(&mut backoff, scratch, &["P"], "P", fail("scratch")).is_some());
        // The allocation cleanup of the same placement is not deferred, and a
        // pass of the allocation lane does not prune the scratch lane.
        assert_eq!(pass(&mut backoff, LANE, &[], "P", Ok(())), None);
        assert!(pass(&mut backoff, scratch, &["P"], "P", fail("scratch")).is_some());
    }

    #[test]
    fn a_fresh_backoff_attempts_everything() {
        let clock = ManualClock::new();
        let mut backoff = RetainedCleanupBackoff::with_clock(clock.clock());
        for _ in 0..3 {
            assert_eq!(pass(&mut backoff, LANE, &["P"], "P", fail("wedged")), None);
            clock.advance(RETAINED_CLEANUP_BACKOFF_CAP);
        }
        assert!(pass(&mut backoff, LANE, &["P"], "P", fail("wedged")).is_none());
        assert!(pass(&mut backoff, LANE, &["P"], "P", fail("wedged")).is_some());
        // A restarted daemon builds a new back-off and attempts it at once.
        let mut restarted = RetainedCleanupBackoff::with_clock(clock.clock());
        assert_eq!(
            pass(&mut restarted, LANE, &["P"], "P", fail("wedged")),
            None
        );
    }
}
