//! Process-wide tallies of two kinds of engine work that dominate request
//! time: verifications of the registered bundle generation, and handler
//! subprocess launches (every parse, compose and effective validation is one).
//!
//! Diagnostic only. A request handler reads the totals before and after a
//! phase and logs the difference, so the figure for one phase also includes
//! work done by any concurrent request. Nothing decides anything from them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub(crate) struct WorkTally {
    count: AtomicU64,
    nanos: AtomicU64,
}

impl WorkTally {
    pub(crate) const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        }
    }

    pub(crate) fn record(&self, elapsed: Duration) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn totals(&self) -> WorkTotals {
        WorkTotals {
            count: self.count.load(Ordering::Relaxed),
            elapsed: Duration::from_nanos(self.nanos.load(Ordering::Relaxed)),
        }
    }
}

/// How many times one kind of work ran, and for how long in total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkTotals {
    pub count: u64,
    pub elapsed: Duration,
}

impl WorkTotals {
    /// The work done since `earlier` was read.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            count: self.count.saturating_sub(earlier.count),
            elapsed: self.elapsed.saturating_sub(earlier.elapsed),
        }
    }
}

pub(crate) static GENERATION_VERIFICATIONS: WorkTally = WorkTally::new();
pub(crate) static HANDLER_LAUNCHES: WorkTally = WorkTally::new();

/// Verifications of a registered bundle generation run by this process.
pub fn generation_verification_totals() -> WorkTotals {
    GENERATION_VERIFICATIONS.totals()
}

/// Handler subprocesses launched by this process.
pub fn handler_launch_totals() -> WorkTotals {
    HANDLER_LAUNCHES.totals()
}
