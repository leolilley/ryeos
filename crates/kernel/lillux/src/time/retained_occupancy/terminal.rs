//! Consuming terminal operation for the original clock. No ordinary observation
//! or fresh authority is available once either descriptor close was attempted.

use super::{Duration, OccupancyLimit, OccupancyWindowState, RetainedOccupancyClock};
use crate::time::MonotonicDeadline;
use std::mem::ManuallyDrop;

fn terminal_interval_is_within(
    monotonic_within: bool,
    window: Result<OccupancyWindowState, &'static str>,
) -> bool {
    monotonic_within && matches!(window,
        Ok(OccupancyWindowState::Service { .. } | OccupancyWindowState::Cleanup { .. }))
}

/// Actual close evidence, never a new clock or cleanup allowance. A late close
/// is still a physical fact; the caller must report its timing separately.
#[derive(Debug)]
pub struct RetiredOccupancyClock {
    process_id: u32,
    binding: [u8; 64],
    within_original_interval: bool,
}
impl RetiredOccupancyClock {
    pub fn require_original_process(&self) -> Result<(), &'static str> {
        if self.process_id != std::process::id() { return Err("clock close belongs to another process"); }
        Ok(())
    }
    pub fn binding_digest(&self) -> &str {
        std::str::from_utf8(&self.binding).expect("original canonical clock digest")
    }
    pub fn within_original_interval(&self) -> bool { self.within_original_interval }
}

enum Refusal {
    Before(&'static str),
    // Each errno is cached in the actual registered descriptor owner too. The
    // consumed clock remains terminal even if the OS released both FD numbers.
    Entered { first: Result<(), i32>, second: Result<(), i32> },
}

pub struct RetainedClockCloseFailure {
    original: ManuallyDrop<RetainedOccupancyClock>,
    refusal: Refusal,
}
impl RetainedClockCloseFailure {
    /// Only a no-effect refusal can restore observation/retry through the same
    /// actual clock. The caller retains its original paid counter and deadline.
    pub fn into_original(self) -> Result<RetainedOccupancyClock, Self> {
        if !matches!(self.refusal, Refusal::Before(_))
            || self.original.process_id != std::process::id() {
            return Err(self);
        }
        Ok(ManuallyDrop::into_inner(self.original))
    }
    pub fn effects_entered(&self) -> bool { matches!(self.refusal, Refusal::Entered { .. }) }
    pub fn close_results(&self) -> Option<[Result<(), i32>; 2]> {
        match self.refusal {
            Refusal::Before(_) => None,
            Refusal::Entered { first, second } => Some([first, second]),
        }
    }
}
impl std::fmt::Debug for RetainedClockCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedClockCloseFailure")
            .field("effects_entered", &self.effects_entered())
            .field("close_results", &self.close_results()).finish_non_exhaustive()
    }
}
impl std::fmt::Display for RetainedClockCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.refusal {
            Refusal::Before(reason) => write!(f, "original clock retained before close: {reason}"),
            Refusal::Entered { .. } => write!(f, "original clock terminal close ambiguous; no observation or FD retry"),
        }
    }
}
impl std::error::Error for RetainedClockCloseFailure {}

impl RetainedOccupancyClock {
    /// Bounded outside work charged before each terminal close attempt. A
    /// before-effect refusal may return the same clock for a later attempt, so
    /// the enclosing finalizer multiplies this by its enforced attempt cap.
    pub const fn terminal_close_work_per_attempt() -> u64 {
        12
    }

    /// Terminal-only timing check after both retained descriptors have been
    /// consumed. The cached namespace identity and original start are enough
    /// to report the original interval; this observes no closed descriptor and
    /// creates no replacement clock authority.
    #[cfg(target_os = "linux")]
    fn terminal_interval_is_current(
        &self,
        limit: &OccupancyLimit,
        cleanup: Duration,
        monotonic_within: bool,
    ) -> bool {
        if !monotonic_within || self.process_id != std::process::id() || limit.start != self.start {
            return false;
        }
        for path in [c"/proc/thread-self/ns/time", c"/proc/thread-self/ns/time_for_children"] {
            let mut current: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::stat(path.as_ptr(), &mut current) } != 0
                || (current.st_dev, current.st_ino) != (self.namespace_device, self.namespace_inode)
            {
                return false;
            }
        }
        let Ok(tick) = crate::time::occupancy_tick_ns_fixed() else { return false };
        terminal_interval_is_within(monotonic_within, self.window_at_tick(limit, cleanup, tick))
    }

    /// Close the actual registered namespace/boot pair once, as the last clock
    /// operation of its outside owner. Before effects, unknown/expired custody,
    /// aliases and a busy fork barrier return the entire original clock. Once
    /// effects enter, it can never return as usable clock authority.
    pub fn into_terminal(self, limit: &OccupancyLimit, cleanup: Duration,
        original: MonotonicDeadline) -> Result<RetiredOccupancyClock, RetainedClockCloseFailure> {
        let mut retained = ManuallyDrop::new(self);
        let refuse = |original, reason| RetainedClockCloseFailure { original, refusal: Refusal::Before(reason) };
        if retained.process_id != std::process::id() {
            return Err(refuse(retained, "clock belongs to another process"));
        }
        if original.has_elapsed() { return Err(refuse(retained, "original clock close deadline exhausted")); }
        match retained.window_fixed(limit, cleanup) {
            Ok(OccupancyWindowState::Service { .. } | OccupancyWindowState::Cleanup { .. }) => {}
            Ok(_) => return Err(refuse(retained, "original occupancy interval exhausted")),
            Err(reason) => return Err(refuse(retained, reason)),
        }
        // Narrow the supplied endpoint to this same original full window.
        // This conversion cannot renew the original start or maximum duration.
        let original = match retained.original_local_deadlines(limit, Duration::ZERO) {
            Ok((deadline, _)) => original.min(deadline),
            Err(_) => return Err(refuse(retained, "original clock endpoint no longer available")),
        };
        #[cfg(not(target_os = "linux"))]
        {
            let _ = original;
            Err(refuse(retained, "terminal clock close requires Linux"))
        }
        #[cfg(target_os = "linux")]
        {
            let clock = &mut *retained;
            let observed = match crate::exec::registered_pair::close_pair(&mut clock.namespace, &mut clock.boot, original) {
                Ok(observed) => observed,
                Err(reason) => return Err(refuse(retained, reason)),
            };
            if observed.first.is_err() || observed.second.is_err() {
                return Err(RetainedClockCloseFailure { original: retained,
                    refusal: Refusal::Entered { first: observed.first, second: observed.second } });
            }
            let within_original_interval = clock.terminal_interval_is_current(
                limit, cleanup, observed.within_interval);
            let receipt = RetiredOccupancyClock {
                process_id: retained.process_id, binding: retained.binding,
                // Instant bounds the pair operation; BOOTTIME includes suspend
                // and is sampled only after the consuming close. Either late or
                // unknown observation yields a false timing claim while keeping
                // the successful physical close receipt.
                within_original_interval,
            };
            // Both cached successful explicit closes consumed the actual Files;
            // consuming their wrappers now performs no descriptor close.
            drop(ManuallyDrop::into_inner(retained));
            Ok(receipt)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod controls {
    use super::*;

    #[test]
    fn shared_clock_descriptor_keeps_whole_original_clock_retryable() {
        let clock = RetainedOccupancyClock::capture().unwrap();
        let limit = OccupancyLimit::new(clock.start().clone(), 30_000_000_000).unwrap();
        let (deadline, _) = clock.original_local_deadlines(&limit, Duration::ZERO).unwrap();
        let alias = clock.boot.clone();
        let failure = clock.into_terminal(&limit, Duration::ZERO, deadline).unwrap_err();
        assert!(!failure.effects_entered());
        let clock = failure.into_original().unwrap();
        assert!(clock.window_fixed(&limit, Duration::ZERO).is_ok());
        let binding = clock.binding;
        drop(alias);
        let receipt = clock.into_terminal(&limit, Duration::ZERO, deadline).unwrap();
        receipt.require_original_process().unwrap();
        assert_eq!(receipt.binding_digest().as_bytes(), binding);
        assert!(receipt.within_original_interval());
    }

    #[test]
    fn expired_or_foreign_limit_never_consumes_clock_descriptors() {
        let clock = RetainedOccupancyClock::capture().unwrap();
        let limit = OccupancyLimit::new(clock.start().clone(), 30_000_000_000).unwrap();
        let (deadline, _) = clock.original_local_deadlines(&limit, Duration::ZERO).unwrap();
        let failure = clock.into_terminal(&limit, Duration::ZERO,
            MonotonicDeadline::after(Duration::ZERO)).unwrap_err();
        assert_eq!(failure.close_results(), None);
        let clock = failure.into_original().unwrap();
        let mut foreign = limit.clone();
        foreign.start.tick_ns += 1;
        let failure = clock.into_terminal(&foreign, Duration::ZERO, deadline).unwrap_err();
        assert!(!failure.effects_entered());
        let clock = failure.into_original().unwrap();
        clock.into_terminal(&limit, Duration::ZERO, deadline).unwrap();
    }

    #[test]
    fn suspend_expiry_fails_timing_even_while_original_monotonic_deadline_is_live() {
        let clock = RetainedOccupancyClock::capture().unwrap();
        let limit = OccupancyLimit::new(clock.start().clone(), 30_000_000_000).unwrap();
        let (deadline, _) = clock.original_local_deadlines(&limit, Duration::ZERO).unwrap();
        assert!(!deadline.has_elapsed());
        let expired_tick = clock.start.tick_ns + limit.maximum_occupancy_ns;
        let state = clock.window_at_tick(&limit, Duration::ZERO, expired_tick);
        assert!(!terminal_interval_is_within(true, state));
        assert!(!deadline.has_elapsed());
    }
}
