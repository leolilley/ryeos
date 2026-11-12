//! Host clock facts a remote process needs when it cannot trust that its wall
//! clock agrees with the clock that issued its authority: whether the kernel
//! considers the wall clock synchronized, and local suspend-inclusive anchors
//! from which a relative window becomes a deadline that no skew can move.

use super::{Duration, OccupancyCoordinate, occupancy_now};
use serde::{Deserialize, Serialize};
use std::fmt;

/// What the host kernel claims about its own wall clock. This is a claim about
/// synchronization, not a measurement of the offset from any other machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WallClockTrust {
    /// The kernel time discipline is synchronized; both bounds are the
    /// kernel's own error estimates in microseconds.
    Synced {
        maximum_error_us: u64,
        estimated_error_us: u64,
    },
    /// The kernel reports the clock as unsynchronized.
    Unsynchronized,
    /// The host refused or cannot answer the query (unsupported platform,
    /// filtered syscall, denied permission).
    Unknown,
}

impl WallClockTrust {
    /// Classify the raw kernel time-discipline answer. `state` is the
    /// syscall's return value, `status` the status word, and both errors are
    /// microseconds. A negative error estimate is not a measurement.
    #[cfg(target_os = "linux")]
    fn from_kernel(
        state: i32,
        status: i64,
        maximum_error_us: i64,
        estimated_error_us: i64,
    ) -> Self {
        // TIME_ERROR is the kernel's "clock is not synchronized" answer, and
        // the unsynchronized status bit says the same thing independently.
        if state == libc::TIME_ERROR || status & i64::from(libc::STA_UNSYNC) != 0 {
            return Self::Unsynchronized;
        }
        match (
            u64::try_from(maximum_error_us),
            u64::try_from(estimated_error_us),
        ) {
            (Ok(maximum_error_us), Ok(estimated_error_us)) => Self::Synced {
                maximum_error_us,
                estimated_error_us,
            },
            _ => Self::Unknown,
        }
    }

    /// Largest error the kernel admits, when it claims to be synchronized.
    pub fn maximum_error(&self) -> Option<Duration> {
        match self {
            Self::Synced {
                maximum_error_us, ..
            } => Some(Duration::from_micros(*maximum_error_us)),
            Self::Unsynchronized | Self::Unknown => None,
        }
    }
}

impl fmt::Display for WallClockTrust {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synced {
                maximum_error_us, ..
            } => write!(
                formatter,
                "synchronized (maximum error {maximum_error_us} us)"
            ),
            Self::Unsynchronized => formatter.write_str("unsynchronized"),
            Self::Unknown => formatter.write_str("unknown"),
        }
    }
}

/// Query the host kernel's wall-clock discipline without changing it.
pub fn wall_clock_trust() -> WallClockTrust {
    wall_clock_trust_platform()
}

#[cfg(target_os = "linux")]
fn wall_clock_trust_platform() -> WallClockTrust {
    // SAFETY: an all-zero `timex` with `modes == 0` is the documented
    // read-only query; the kernel only writes into the structure.
    let mut value: libc::timex = unsafe { std::mem::zeroed() };
    value.modes = 0;
    let state = unsafe { libc::adjtimex(&mut value) };
    if state < 0 {
        return WallClockTrust::Unknown;
    }
    WallClockTrust::from_kernel(
        state,
        i64::from(value.status),
        value.maxerror,
        value.esterror,
    )
}

#[cfg(not(target_os = "linux"))]
fn wall_clock_trust_platform() -> WallClockTrust {
    WallClockTrust::Unknown
}

/// One suspend-inclusive instant on this host, captured so a relative window
/// issued by a remote authority can be turned into a local deadline. Capture
/// it before the request that obtains the window: the authority then issues
/// the window after this instant, so `anchor + length` can only be earlier
/// than the authority's own end, whatever either wall clock says.
///
/// The anchor names its boot incarnation, so it can be retained in a journal
/// and re-read by a restarted process of the same boot. After a different
/// boot it proves nothing and every deadline derived from it reads as elapsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalClockAnchor {
    coordinate: OccupancyCoordinate,
}

impl LocalClockAnchor {
    pub fn capture() -> Result<Self, String> {
        Ok(Self {
            coordinate: occupancy_now()?,
        })
    }

    /// Rebuild an anchor from a retained record, refusing malformed identity.
    pub fn from_retained(coordinate: OccupancyCoordinate) -> Result<Self, String> {
        coordinate.validate()?;
        Ok(Self { coordinate })
    }

    pub fn coordinate(&self) -> &OccupancyCoordinate {
        &self.coordinate
    }

    /// Whether this host is still on the boot the anchor was captured on.
    /// Only then can the anchor bound a window: a later boot's clock restarts,
    /// so an anchor from before it proves nothing about elapsed time.
    pub fn is_current_boot(&self) -> bool {
        occupancy_now().is_ok_and(|now| {
            now.contract_digest == self.coordinate.contract_digest
                && now.incarnation_digest == self.coordinate.incarnation_digest
        })
    }

    /// Deadline `length` after this anchor, counting time the host spent
    /// suspended.
    pub fn deadline_after(&self, length: Duration) -> Result<LocalDeadline, String> {
        let length = u64::try_from(length.as_nanos())
            .map_err(|_| "local window length overflows the clock".to_owned())?;
        let tick_ns = self
            .coordinate
            .tick_ns
            .checked_add(length)
            .ok_or_else(|| "local window end overflows the clock".to_owned())?;
        Ok(LocalDeadline {
            coordinate: OccupancyCoordinate {
                tick_ns,
                ..self.coordinate.clone()
            },
        })
    }
}

/// A deadline on this host's suspend-inclusive clock. Unlike the process
/// monotonic deadline it keeps counting while the host is paused, and it can
/// be retained across a process restart within one boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalDeadline {
    coordinate: OccupancyCoordinate,
}

impl LocalDeadline {
    pub fn coordinate(&self) -> &OccupancyCoordinate {
        &self.coordinate
    }

    /// Time left, with every doubt (clock unreadable, other boot) reading as
    /// none.
    pub fn remaining(&self) -> Duration {
        match occupancy_now() {
            Ok(now) => self.remaining_at(&now),
            Err(_) => Duration::ZERO,
        }
    }

    pub fn has_elapsed(&self) -> bool {
        self.remaining().is_zero()
    }

    /// As [`Self::remaining`] against an explicit reading of the clock.
    pub fn remaining_at(&self, now: &OccupancyCoordinate) -> Duration {
        if now.contract_digest != self.coordinate.contract_digest
            || now.incarnation_digest != self.coordinate.incarnation_digest
        {
            return Duration::ZERO;
        }
        Duration::from_nanos(self.coordinate.tick_ns.saturating_sub(now.tick_ns))
    }

    /// The earlier of two deadlines on the same boot; deadlines of different
    /// boots have no order, so the result is already elapsed.
    pub fn min(&self, other: &Self) -> Self {
        if self.coordinate.incarnation_digest != other.coordinate.incarnation_digest
            || self.coordinate.contract_digest != other.coordinate.contract_digest
        {
            return Self {
                coordinate: OccupancyCoordinate {
                    tick_ns: 0,
                    ..self.coordinate.clone()
                },
            };
        }
        if self.coordinate.tick_ns <= other.coordinate.tick_ns {
            self.clone()
        } else {
            other.clone()
        }
    }

    /// The same deadline as a [`super::MonotonicDeadline`] for components
    /// that carry that type. It counts the suspend-inclusive clock, so a
    /// paused host cannot extend it, and it reads as elapsed when this
    /// deadline is from another boot. On a host with no such clock it falls
    /// back to the process clock from the time left now.
    pub fn to_monotonic(&self) -> super::MonotonicDeadline {
        if self.remaining().is_zero() {
            return super::MonotonicDeadline::after(Duration::ZERO);
        }
        super::MonotonicDeadline::at_boot_tick(self.coordinate.tick_ns)
            .unwrap_or_else(|| super::MonotonicDeadline::after(self.remaining()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_answers_classify_into_trust() {
        assert_eq!(
            WallClockTrust::from_kernel(libc::TIME_OK, 0x2001, 1500, 200),
            WallClockTrust::Synced {
                maximum_error_us: 1500,
                estimated_error_us: 200
            }
        );
        assert_eq!(
            WallClockTrust::from_kernel(libc::TIME_ERROR, 0, 16_000_000, 0),
            WallClockTrust::Unsynchronized
        );
        assert_eq!(
            WallClockTrust::from_kernel(libc::TIME_OK, i64::from(libc::STA_UNSYNC), 0, 0),
            WallClockTrust::Unsynchronized
        );
        assert_eq!(
            WallClockTrust::from_kernel(libc::TIME_OK, 0, -1, 0),
            WallClockTrust::Unknown
        );
    }

    #[test]
    fn trust_round_trips_and_names_itself() {
        for trust in [
            WallClockTrust::Synced {
                maximum_error_us: 7,
                estimated_error_us: 3,
            },
            WallClockTrust::Unsynchronized,
            WallClockTrust::Unknown,
        ] {
            let text = serde_json::to_string(&trust).unwrap();
            assert_eq!(
                serde_json::from_str::<WallClockTrust>(&text).unwrap(),
                trust
            );
        }
        assert_eq!(WallClockTrust::Unsynchronized.to_string(), "unsynchronized");
        assert_eq!(
            WallClockTrust::Synced {
                maximum_error_us: 12,
                estimated_error_us: 1
            }
            .to_string(),
            "synchronized (maximum error 12 us)"
        );
    }

    #[test]
    fn live_query_never_panics() {
        let _ = wall_clock_trust();
    }

    fn coordinate(boot: &str, tick_ns: u64) -> OccupancyCoordinate {
        let current = occupancy_now().unwrap_or_else(|_| OccupancyCoordinate {
            contract_digest: crate::sha256_hex(b"contract"),
            incarnation_digest: crate::sha256_hex(b"boot"),
            tick_ns: 0,
        });
        OccupancyCoordinate {
            contract_digest: current.contract_digest,
            incarnation_digest: crate::sha256_hex(boot.as_bytes()),
            tick_ns,
        }
    }

    #[test]
    fn deadline_counts_from_the_anchor_and_reads_elapsed_at_or_after_it() {
        let anchor = LocalClockAnchor::from_retained(coordinate("a", 1_000)).unwrap();
        let deadline = anchor.deadline_after(Duration::from_secs(5)).unwrap();
        assert_eq!(
            deadline.remaining_at(&coordinate("a", 1_000)),
            Duration::from_secs(5)
        );
        assert_eq!(
            deadline.remaining_at(&coordinate("a", 2_000_001_000)),
            Duration::from_secs(3)
        );
        assert!(
            deadline
                .remaining_at(&coordinate("a", 5_000_001_000))
                .is_zero()
        );
        assert!(deadline.remaining_at(&coordinate("a", u64::MAX)).is_zero());
    }

    #[test]
    fn another_boot_proves_nothing() {
        let anchor = LocalClockAnchor::from_retained(coordinate("a", 1_000)).unwrap();
        let deadline = anchor.deadline_after(Duration::from_secs(5)).unwrap();
        assert!(deadline.remaining_at(&coordinate("b", 2_000)).is_zero());
        let other = LocalClockAnchor::from_retained(coordinate("b", 1_000))
            .unwrap()
            .deadline_after(Duration::from_secs(9))
            .unwrap();
        assert!(
            deadline
                .min(&other)
                .remaining_at(&coordinate("a", 0))
                .is_zero()
        );
    }

    #[test]
    fn minimum_picks_the_earlier_deadline_and_overflow_is_refused() {
        let anchor = LocalClockAnchor::from_retained(coordinate("a", 0)).unwrap();
        let early = anchor.deadline_after(Duration::from_secs(1)).unwrap();
        let late = anchor.deadline_after(Duration::from_secs(9)).unwrap();
        assert_eq!(early.min(&late), early);
        assert_eq!(late.min(&early), early);
        let high = LocalClockAnchor::from_retained(coordinate("a", u64::MAX - 5)).unwrap();
        assert!(high.deadline_after(Duration::from_secs(1)).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_anchor_deadline_runs_down() {
        let anchor = LocalClockAnchor::capture().unwrap();
        assert!(anchor.is_current_boot());
        let mut other_boot = anchor.coordinate().clone();
        other_boot.incarnation_digest = crate::sha256_hex(b"another boot");
        assert!(
            !LocalClockAnchor::from_retained(other_boot)
                .unwrap()
                .is_current_boot()
        );
        let deadline = anchor.deadline_after(Duration::from_secs(60)).unwrap();
        assert!(!deadline.has_elapsed());
        assert!(deadline.remaining() <= Duration::from_secs(60));
        assert!(anchor.deadline_after(Duration::ZERO).unwrap().has_elapsed());
        assert!(!deadline.to_monotonic().has_elapsed());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn monotonic_view_counts_the_suspend_inclusive_clock_and_mixes_with_process_deadlines() {
        let anchor = LocalClockAnchor::capture().unwrap();
        let long = anchor
            .deadline_after(Duration::from_secs(3600))
            .unwrap()
            .to_monotonic();
        let short = anchor
            .deadline_after(Duration::from_secs(5))
            .unwrap()
            .to_monotonic();
        assert!(!long.has_elapsed());
        assert!(long.remaining() > Duration::from_secs(3500));
        assert!(short.remaining() <= Duration::from_secs(5));
        // Same clock narrows exactly; across clocks by time left.
        assert!(short.min(long).remaining() <= Duration::from_secs(5));
        assert!(long.min(short).remaining() <= Duration::from_secs(5));
        let process = super::super::MonotonicDeadline::after(Duration::from_secs(60));
        assert!(short.min(process).remaining() <= Duration::from_secs(5));
        assert!(process.min(short).remaining() <= Duration::from_secs(5));
        assert!(process.min(long).remaining() <= Duration::from_secs(60));
        assert!(long.min(process).remaining() <= Duration::from_secs(60));
        // An elapsed window and one from another boot are elapsed at once.
        let elapsed = anchor.deadline_after(Duration::ZERO).unwrap();
        assert!(elapsed.to_monotonic().has_elapsed());
        let mut other = anchor.coordinate().clone();
        other.incarnation_digest = crate::sha256_hex(b"another boot");
        let foreign = LocalClockAnchor::from_retained(other)
            .unwrap()
            .deadline_after(Duration::from_secs(60))
            .unwrap();
        assert!(foreign.to_monotonic().has_elapsed());
        assert!(foreign.to_monotonic().remaining().is_zero());
    }
}
