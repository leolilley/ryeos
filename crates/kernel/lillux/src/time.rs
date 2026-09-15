use std::thread;
pub use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::Subcommand;

/// One process-local monotonic measurement owned by the Lillux host-time
/// boundary. The value is intentionally opaque: callers may retain it only to
/// measure elapsed duration in the same process and clock domain.
#[derive(Debug)]
pub struct MonotonicTimer {
    started_at: Instant,
}

/// Opaque process-local monotonic expiry authority.
///
/// Callers can ask whether the deadline has elapsed, but never receive the
/// host `Instant` or compare it as durable identity. This keeps callback and
/// lease expiry mechanics inside the Lillux time boundary.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicDeadline {
    deadline: Instant,
}

impl MonotonicDeadline {
    pub fn after(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
        }
    }

    pub fn has_elapsed(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Narrow two process-local expiry authorities without restarting either
    /// clock or exposing the underlying host instant.
    pub fn min(self, other: Self) -> Self {
        Self {
            deadline: self.deadline.min(other.deadline),
        }
    }

    /// Remaining duration in this process-local monotonic clock domain.
    /// Returning zero is the only expiry representation exposed to callers;
    /// the host `Instant` never escapes Lillux.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

impl MonotonicTimer {
    pub fn start() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn elapsed_millis(&self) -> u64 {
        u64::try_from(self.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub fn elapsed_micros(&self) -> u64 {
        u64::try_from(self.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

/// Sleep the current thread for one process-local duration. Raw host timing
/// remains below the Lillux boundary even for synchronous protocol loops.
pub fn sleep(duration: Duration) {
    thread::sleep(duration);
}

pub fn iso8601_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso8601_from_unix_secs(now.as_secs())
}

/// Format an absolute Unix timestamp (seconds) as ISO-8601 UTC — same
/// encoding `iso8601_now` emits, for computing comparable cutoffs.
pub fn iso8601_from_unix_secs(secs: u64) -> String {
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let seconds = day_secs % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Current wall-clock time as milliseconds since Unix epoch.
pub fn timestamp_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[derive(Subcommand)]
pub enum TimeAction {
    /// Current wall-clock time
    Now,
    /// Sleep for N milliseconds
    After {
        #[arg(long)]
        ms: u64,
    },
}

pub fn run(action: TimeAction) -> serde_json::Value {
    match action {
        TimeAction::Now => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            serde_json::json!({
                "timestamp_ns": now.as_nanos() as u64,
                "timestamp_ms": now.as_millis() as u64,
            })
        }
        TimeAction::After { ms } => {
            let start = Instant::now();
            thread::sleep(Duration::from_millis(ms));
            let elapsed = start.elapsed().as_millis() as u64;
            serde_json::json!({ "elapsed_ms": elapsed })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_intersection_preserves_the_original_earliest_instant() {
        let earlier = MonotonicDeadline::after(Duration::ZERO);
        let later = MonotonicDeadline::after(Duration::from_secs(60));
        assert_eq!(earlier.min(later).deadline, earlier.deadline);
        assert_eq!(later.min(earlier).deadline, earlier.deadline);
        assert!(later.min(earlier).has_elapsed());
        assert_eq!(earlier.min(earlier).deadline, earlier.deadline);
    }
}
