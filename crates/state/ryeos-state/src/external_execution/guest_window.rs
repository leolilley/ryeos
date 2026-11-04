//! The lifetime of an attached channel as a guest measures it.
//!
//! The controller issues a binding with absolute wall-clock times and enforces
//! them on its own clock. A guest on another machine cannot compare those
//! times with its own wall clock: any skew would shift or void the window. The
//! guest instead measures the window's length from a local instant captured
//! before it asked for the binding. The controller issued the binding after
//! that instant, so each local deadline can only fall before the controller's
//! own end of the same window, whatever either wall clock reads.

use super::ExecutionChannelBinding;
use anyhow::{Result, ensure};
use lillux::time::{Duration, LocalClockAnchor, LocalDeadline, MonotonicDeadline};

#[derive(Debug, Clone)]
enum Deadline {
    /// Counts host suspension and can be retained in a journal.
    Local(LocalDeadline),
    /// Handed to a child process, which holds only a monotonic deadline.
    Handed(MonotonicDeadline),
}

impl Deadline {
    fn remaining(&self) -> Duration {
        match self {
            Self::Local(deadline) => deadline.remaining(),
            Self::Handed(deadline) => deadline.remaining(),
        }
    }

    fn monotonic(&self) -> MonotonicDeadline {
        match self {
            Self::Local(deadline) => deadline.to_monotonic(),
            Self::Handed(deadline) => *deadline,
        }
    }
}

/// Execution and channel-expiry deadlines on the guest's own clock. Every
/// consumer on the guest asks this value whether a window is still open; none
/// compares a binding's absolute times with the guest wall clock.
#[derive(Debug, Clone)]
pub struct GuestChannelWindow {
    execution: Deadline,
    expiry: Deadline,
}

impl GuestChannelWindow {
    /// Derive both deadlines from the window lengths the binding declares.
    /// `anchor` must have been captured before the binding was requested.
    pub fn derive(anchor: &LocalClockAnchor, binding: &ExecutionChannelBinding) -> Result<Self> {
        binding.validate()?;
        ensure!(
            anchor.is_current_boot(),
            "the guest's local clock anchor is from an earlier boot and cannot bound this channel"
        );
        let execution_ms = binding
            .execution_deadline_ms
            .checked_sub(binding.issued_at_ms)
            .and_then(|length| u64::try_from(length).ok())
            .filter(|length| *length > 0);
        let expiry_ms = binding
            .expires_at_ms
            .checked_sub(binding.issued_at_ms)
            .and_then(|length| u64::try_from(length).ok())
            .filter(|length| *length > 0);
        let (Some(execution_ms), Some(expiry_ms)) = (execution_ms, expiry_ms) else {
            anyhow::bail!("external channel window lengths are not positive");
        };
        ensure!(
            expiry_ms > execution_ms,
            "external channel has no bounded execution and cleanup window"
        );
        let window = Self {
            execution: Deadline::Local(
                anchor
                    .deadline_after(Duration::from_millis(execution_ms))
                    .map_err(anyhow::Error::msg)?,
            ),
            expiry: Deadline::Local(
                anchor
                    .deadline_after(Duration::from_millis(expiry_ms))
                    .map_err(anyhow::Error::msg)?,
            ),
        };
        ensure!(
            !window.expiry_elapsed() && !window.execution_elapsed(),
            "external channel window already elapsed on the guest's own clock"
        );
        Ok(window)
    }

    /// Rebuild the window in a child process from the deadline pair its parent
    /// handed down.
    pub fn from_handed_down(
        execution: MonotonicDeadline,
        expiry: MonotonicDeadline,
    ) -> Result<Self> {
        ensure!(
            expiry.remaining() >= execution.remaining() && !execution.has_elapsed(),
            "handed-down external channel window is inverted or elapsed"
        );
        Ok(Self {
            execution: Deadline::Handed(execution),
            expiry: Deadline::Handed(expiry),
        })
    }

    pub fn execution_remaining(&self) -> Duration {
        self.execution.remaining()
    }

    pub fn expiry_remaining(&self) -> Duration {
        self.expiry.remaining()
    }

    pub fn execution_elapsed(&self) -> bool {
        self.execution.remaining().is_zero()
    }

    pub fn expiry_elapsed(&self) -> bool {
        self.expiry.remaining().is_zero()
    }

    /// Suspend-inclusive monotonic views of the two deadlines.
    pub fn execution_monotonic(&self) -> MonotonicDeadline {
        self.execution.monotonic()
    }

    pub fn expiry_monotonic(&self) -> MonotonicDeadline {
        self.expiry.monotonic()
    }

    /// Both deadlines as suspend-inclusive monotonic views, for components
    /// that carry only a monotonic deadline and for handing to a child
    /// process. Each is an exact coordinate on the host clock, so their order
    /// cannot be perturbed by two separate samples.
    pub fn monotonic_pair(&self) -> (MonotonicDeadline, MonotonicDeadline) {
        (self.execution.monotonic(), self.expiry.monotonic())
    }

    /// A window that began now and runs for the binding's own declared
    /// lengths. For harnesses that issue the binding and run the guest in one
    /// process on one clock; a real guest derives its window from an anchor
    /// captured before it asked for the binding.
    #[cfg(any(test, feature = "test-support"))]
    pub fn starting_now_for_single_clock_harness(
        binding: &ExecutionChannelBinding,
    ) -> Result<Self> {
        Self::derive(
            &LocalClockAnchor::capture().map_err(anyhow::Error::msg)?,
            binding,
        )
    }
}
