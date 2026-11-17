//! The run deadline as this runtime sees it.
//!
//! The daemon stamps the execution's absolute end `E` on the launch envelope.
//! The run has three phases:
//!
//! - `Running` until `W = E - R - wrap_up`, where `R` is the execution's
//!   reserve (the time it keeps to settle) and `wrap_up` is the runtime
//!   limit `wrap_up_seconds`;
//! - `WrapUp` from `W` until `H = E - R`, when the model is told time is short;
//! - `HardStop` from `H`: no new turn or tool call starts, and the run settles
//!   as a deadline stop. The daemon's kill at `E` is only the backstop.
//!
//! `wrap_up_seconds` is a dimension this runtime declares in its signed
//! limits contract; the substrate never names it. `0` means no wrap-up phase.

use ryeos_runtime::envelope::HardLimits;

/// Name of the runtime-declared limit that sizes the wrap-up phase, in seconds.
pub(crate) const WRAP_UP_SECONDS_LIMIT: &str = "wrap_up_seconds";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunPhase {
    Running,
    WrapUp,
    HardStop,
}

impl RunPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WrapUp => "wrap_up",
            Self::HardStop => "hard_stop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunClock {
    end_at_ms: i64,
    reserve_ms: u64,
    wrap_up_ms: u64,
}

impl RunClock {
    /// The clock of an execution that has a run deadline; `None` when the
    /// daemon stamped no end, which leaves every phase inert.
    pub(crate) fn from_limits(limits: &HardLimits) -> Option<Self> {
        let end_at_ms = limits.deadline.at_ms?;
        Some(Self {
            end_at_ms,
            reserve_ms: limits.deadline.reserve_ms(),
            wrap_up_ms: limits
                .runtime_limit(WRAP_UP_SECONDS_LIMIT)
                .saturating_mul(1_000),
        })
    }

    /// `E`: when the daemon kills this execution.
    pub(crate) fn end_at_ms(&self) -> i64 {
        self.end_at_ms
    }

    /// `H`: when new work stops and the run settles.
    pub(crate) fn hard_stop_at_ms(&self) -> i64 {
        self.end_at_ms
            .saturating_sub(i64::try_from(self.reserve_ms).unwrap_or(i64::MAX))
    }

    /// `W`: when the model is told time is short. `None` without a wrap-up
    /// phase.
    pub(crate) fn wrap_up_at_ms(&self) -> Option<i64> {
        (self.wrap_up_ms > 0).then(|| {
            self.hard_stop_at_ms()
                .saturating_sub(i64::try_from(self.wrap_up_ms).unwrap_or(i64::MAX))
        })
    }

    pub(crate) fn phase_at(&self, now_ms: i64) -> RunPhase {
        if now_ms >= self.hard_stop_at_ms() {
            RunPhase::HardStop
        } else if self
            .wrap_up_at_ms()
            .is_some_and(|wrap_up| now_ms >= wrap_up)
        {
            RunPhase::WrapUp
        } else {
            RunPhase::Running
        }
    }

    /// Time left before the hard stop, never negative.
    pub(crate) fn remaining_ms(&self, now_ms: i64) -> u64 {
        u64::try_from(self.hard_stop_at_ms().saturating_sub(now_ms)).unwrap_or(0)
    }
}

/// The `time_left` tool's answer: where the run stands against its deadline.
/// Every field but `now_ms` is `null` when the run has no deadline.
/// `remaining_ms` counts down to the hard stop, the last moment new work can
/// start; `deadline_at_ms` is when the daemon would kill the execution.
pub(crate) fn time_left_report(clock: Option<&RunClock>, now_ms: i64) -> serde_json::Value {
    match clock {
        Some(clock) => serde_json::json!({
            "now_ms": now_ms,
            "deadline_at_ms": clock.end_at_ms(),
            "remaining_ms": clock.remaining_ms(now_ms),
            "phase": clock.phase_at(now_ms).as_str(),
            "wrap_up_at_ms": clock.wrap_up_at_ms(),
            "hard_stop_at_ms": clock.hard_stop_at_ms(),
        }),
        None => serde_json::json!({
            "now_ms": now_ms,
            "deadline_at_ms": null,
            "remaining_ms": null,
            "phase": null,
            "wrap_up_at_ms": null,
            "hard_stop_at_ms": null,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::envelope::DeadlineLimits;

    fn limits(at_ms: Option<i64>, reserve_seconds: u64, wrap_up_seconds: u64) -> HardLimits {
        let mut limits = HardLimits {
            deadline: DeadlineLimits {
                reserve_seconds,
                hold_back_ms: 0,
                at_ms,
            },
            ..HardLimits::default()
        };
        limits
            .runtime
            .insert(WRAP_UP_SECONDS_LIMIT.to_string(), wrap_up_seconds);
        limits
    }

    #[test]
    fn no_deadline_means_no_clock() {
        assert_eq!(RunClock::from_limits(&limits(None, 30, 60)), None);
    }

    #[test]
    fn phases_change_at_exactly_the_three_boundaries() {
        // E = 1_000_000, R = 30 s, wrap-up 60 s: H = 970_000, W = 910_000.
        let clock = RunClock::from_limits(&limits(Some(1_000_000), 30, 60)).unwrap();
        assert_eq!(clock.end_at_ms(), 1_000_000);
        assert_eq!(clock.hard_stop_at_ms(), 970_000);
        assert_eq!(clock.wrap_up_at_ms(), Some(910_000));
        assert_eq!(clock.phase_at(0), RunPhase::Running);
        assert_eq!(clock.phase_at(909_999), RunPhase::Running);
        assert_eq!(clock.phase_at(910_000), RunPhase::WrapUp);
        assert_eq!(clock.phase_at(969_999), RunPhase::WrapUp);
        assert_eq!(clock.phase_at(970_000), RunPhase::HardStop);
        assert_eq!(clock.phase_at(2_000_000), RunPhase::HardStop);
    }

    #[test]
    fn zero_wrap_up_skips_the_phase() {
        let clock = RunClock::from_limits(&limits(Some(1_000_000), 30, 0)).unwrap();
        assert_eq!(clock.wrap_up_at_ms(), None);
        assert_eq!(clock.phase_at(969_999), RunPhase::Running);
        assert_eq!(clock.phase_at(970_000), RunPhase::HardStop);
    }

    #[test]
    fn zero_reserve_stops_at_the_daemon_kill() {
        let clock = RunClock::from_limits(&limits(Some(1_000_000), 0, 10)).unwrap();
        assert_eq!(clock.hard_stop_at_ms(), clock.end_at_ms());
        assert_eq!(clock.phase_at(999_999), RunPhase::WrapUp);
        assert_eq!(clock.phase_at(1_000_000), RunPhase::HardStop);
    }

    #[test]
    fn remaining_counts_down_to_the_hard_stop_and_stays_at_zero() {
        let clock = RunClock::from_limits(&limits(Some(1_000_000), 30, 0)).unwrap();
        assert_eq!(clock.remaining_ms(960_000), 10_000);
        assert_eq!(clock.remaining_ms(970_000), 0);
        assert_eq!(clock.remaining_ms(5_000_000), 0);
    }

    #[test]
    fn time_left_reports_each_phase_and_nulls_without_a_deadline() {
        let clock = RunClock::from_limits(&limits(Some(1_000_000), 30, 60)).unwrap();
        let running = time_left_report(Some(&clock), 100_000);
        assert_eq!(running["phase"], "running");
        assert_eq!(running["remaining_ms"], 870_000);
        assert_eq!(running["deadline_at_ms"], 1_000_000);
        assert_eq!(running["hard_stop_at_ms"], 970_000);
        assert_eq!(running["wrap_up_at_ms"], 910_000);
        assert_eq!(running["now_ms"], 100_000);
        assert_eq!(time_left_report(Some(&clock), 930_000)["phase"], "wrap_up");
        let stopped = time_left_report(Some(&clock), 990_000);
        assert_eq!(stopped["phase"], "hard_stop");
        assert_eq!(stopped["remaining_ms"], 0);

        let none = time_left_report(None, 5);
        assert_eq!(none["now_ms"], 5);
        for field in [
            "deadline_at_ms",
            "remaining_ms",
            "phase",
            "wrap_up_at_ms",
            "hard_stop_at_ms",
        ] {
            assert!(none[field].is_null(), "{field}");
        }
    }
}
