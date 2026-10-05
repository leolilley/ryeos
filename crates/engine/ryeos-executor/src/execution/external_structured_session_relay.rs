//! Process-local scheduling for the ordinary StructuredSession file relay.
//!
//! This has its own typed app entrypoints and owner lane. A Ready owner retains
//! the pump ahead of the cleanup lifeline; dropping it always stops and joins.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context as _, Result, ensure};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_app::external_placement::{
    ExternalSessionRelayPending, ExternalSessionRelayStep, ExternalSessionRelayStepProgress,
    advance_external_session_relay_step,
};
use ryeos_app::persistent_session::{
    PersistentSessionRelayFailure, PersistentSessionRelayHealthCell,
    PersistentSessionRelayOwner,
};

const MAXIMUM_SLOT_READS: u32 = 128;

#[derive(Clone, Copy)]
struct RelaySchedule {
    poll_interval_ms: u32,
    poll_deadline_ms: u32,
}

impl RelaySchedule {
    fn pending_delay(&self, pending: ExternalSessionRelayPending, now_ms: i64) -> Result<Duration> {
        ensure!(
            pending.poll_interval_ms == self.poll_interval_ms
                && (1..MAXIMUM_SLOT_READS).contains(&pending.read_attempts),
            "ordinary relay pending changed its signed interval or exhausted its durable GET allowance"
        );
        let remaining_ms = pending
            .deadline_ms
            .checked_sub(now_ms)
            .filter(|remaining| *remaining > 0)
            .context("ordinary relay exact slot deadline elapsed")?;
        let remaining_reads = u64::from(MAXIMUM_SLOT_READS - pending.read_attempts);
        let interval_ms = u64::from(self.poll_interval_ms);
        let retry_window_ms = u64::try_from(remaining_ms)?.saturating_sub(interval_ms);
        let delay_ms = interval_ms.max(retry_window_ms.div_ceil(remaining_reads));
        ensure!(
            delay_ms + interval_ms < u64::from(self.poll_deadline_ms),
            "ordinary relay GET allowance cannot cover its slot within the signed guest polling window"
        );
        ensure!(
            delay_ms < u64::try_from(remaining_ms)?,
            "ordinary relay has no retry before its exact slot deadline"
        );
        Ok(Duration::from_millis(delay_ms))
    }
}

pub(crate) struct ExternalStructuredSessionRelayCursor {
    step: Option<ExternalSessionRelayStep>,
    maximum_frames: u32,
    schedule: RelaySchedule,
    next_contact: MonotonicDeadline,
    slot_deadline_ms: Option<i64>,
    channel_deadline_ms: Option<i64>,
}

impl ExternalStructuredSessionRelayCursor {
    pub(crate) fn new(
        poll_interval_ms: u32,
        poll_deadline_ms: u32,
        maximum_frames: u32,
    ) -> Result<Self> {
        ensure!(
            poll_interval_ms > 0
                && maximum_frames > 0
                && u64::from(poll_interval_ms) * 2 < u64::from(poll_deadline_ms),
            "ordinary relay has no feasible signed polling window or frame allowance"
        );
        Ok(Self {
            step: Some(ExternalSessionRelayStep::Attach),
            maximum_frames,
            schedule: RelaySchedule {
                poll_interval_ms,
                poll_deadline_ms,
            },
            next_contact: MonotonicDeadline::after(Duration::ZERO),
            slot_deadline_ms: None,
            channel_deadline_ms: None,
        })
    }

    pub(crate) fn advance_if_due(
        &mut self,
        state: &ryeos_app::state::AppState,
        placement: &str,
    ) -> Result<()> {
        let step = self
            .step
            .context("ordinary relay exhausted its signed frame allowance before owner settlement")?;
        if !self.next_contact.has_elapsed() {
            return Ok(());
        }
        if matches!(step, ExternalSessionRelayStep::Exchange { .. })
            && self.channel_deadline_ms.is_none()
        {
            self.channel_deadline_ms = Some(
                state
                    .state_store
                    .external_execution_channel(placement)?
                    .expires_at_ms,
            );
        }
        if let Some(deadline) = self.channel_deadline_ms {
            ensure!(
                lillux::time::timestamp_millis() < deadline,
                "ordinary relay channel expired before contact"
            );
        }
        if let Some(deadline) = self.slot_deadline_ms {
            ensure!(
                lillux::time::timestamp_millis() < deadline,
                "ordinary relay exact slot deadline elapsed before retry"
            );
        }
        let progress = advance_external_session_relay_step(state, placement, step)?;
        self.observe(progress, lillux::time::timestamp_millis())
    }

    fn observe(&mut self, progress: ExternalSessionRelayStepProgress, now_ms: i64) -> Result<()> {
        match progress {
            ExternalSessionRelayStepProgress::GuestRequestPending(pending)
            | ExternalSessionRelayStepProgress::ControllerResponsePending(pending) => {
                let mut bounded = pending;
                if let Some(deadline) = self.channel_deadline_ms {
                    bounded.deadline_ms = bounded.deadline_ms.min(deadline);
                }
                let delay = self.schedule.pending_delay(bounded, now_ms)?;
                if let Some(deadline) = self.slot_deadline_ms {
                    ensure!(
                        deadline == pending.deadline_ms,
                        "ordinary relay pending changed its exact slot deadline"
                    );
                }
                self.slot_deadline_ms = Some(pending.deadline_ms);
                self.next_contact = MonotonicDeadline::after(delay);
            }
            ExternalSessionRelayStepProgress::ControllerResponseAccepted => {
                self.step = match self
                    .step
                    .context("ordinary relay accepted beyond its signed frame allowance")?
                {
                    ExternalSessionRelayStep::Attach => {
                        Some(ExternalSessionRelayStep::Exchange { sequence: 1 })
                    }
                    ExternalSessionRelayStep::Exchange { sequence }
                        if sequence < self.maximum_frames =>
                    {
                        Some(ExternalSessionRelayStep::Exchange {
                            sequence: sequence + 1,
                        })
                    }
                    ExternalSessionRelayStep::Exchange { .. } => None,
                };
                self.slot_deadline_ms = None;
                self.next_contact = MonotonicDeadline::after(Duration::ZERO);
            }
        }
        Ok(())
    }

    fn frame_budget_exhausted(&self) -> bool {
        self.step.is_none()
    }

    fn run(
        mut self,
        state: ryeos_app::state::AppState,
        placement: String,
        stop: Receiver<()>,
    ) -> std::result::Result<(), PersistentSessionRelayFailure> {
        loop {
            if self.frame_budget_exhausted() {
                return Err(PersistentSessionRelayFailure::SignedFrameBudgetExhausted);
            }
            let stopped = match stop.recv_timeout(self.next_contact.remaining()) {
                Ok(()) => true,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(PersistentSessionRelayFailure::ContactFailed);
                }
                Err(RecvTimeoutError::Timeout) => false,
            };
            if stopped {
                return Ok(());
            }
            self.advance_if_due(&state, &placement)
                .map_err(|_| PersistentSessionRelayFailure::ContactFailed)?;
        }
    }
}

/// No join handle is detached. This object must be retained before the
/// ordinary external cleanup lifeline and is dropped before that cleanup is
/// requested or released.
pub(crate) struct ExternalStructuredSessionRelayPump {
    stop: Sender<()>,
    thread: Mutex<Option<JoinHandle<()>>>,
    health: PersistentSessionRelayHealthCell,
}

impl ExternalStructuredSessionRelayPump {
    pub(crate) fn start(
        state: &ryeos_app::state::AppState,
        placement: String,
        cursor: ExternalStructuredSessionRelayCursor,
    ) -> Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let state = state.clone();
        let health = PersistentSessionRelayHealthCell::new();
        let thread_health = health.clone();
        let thread = std::thread::Builder::new()
            .name("ordinary-session-relay".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cursor.run(state, placement, receiver)
                }));
                match outcome {
                    Ok(Ok(())) => thread_health.stop(),
                    Ok(Err(failure)) => thread_health.fail(failure),
                    Err(_) => thread_health.fail(PersistentSessionRelayFailure::PumpPanicked),
                }
            })
            .context("start joinable ordinary-session relay pump")?;
        Ok(Self {
            stop,
            thread: Mutex::new(Some(thread)),
            health,
        })
    }

    pub(crate) fn ensure_running(&self) -> Result<()> {
        ensure!(
            matches!(self.health.snapshot(), ryeos_app::persistent_session::PersistentSessionRelayHealth::Live)
                && self.thread
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .is_some_and(|thread| !thread.is_finished()),
            "ordinary-session relay pump failed or exhausted its signed frames before owner settlement"
        );
        Ok(())
    }

    pub(crate) fn health_cell(&self) -> PersistentSessionRelayHealthCell {
        self.health.clone()
    }

    pub(crate) fn stop_and_join_inner(&self) -> Result<()> {
        self.health.stop();
        let _ = self.stop.send(());
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(thread) = thread else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| anyhow::anyhow!("ordinary-session relay pump thread panicked"))
    }
}

impl PersistentSessionRelayOwner for ExternalStructuredSessionRelayPump {
    fn stop_and_join(&self) -> Result<()> {
        self.stop_and_join_inner()
    }
}

impl Drop for ExternalStructuredSessionRelayPump {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_join_inner() {
            tracing::error!(%error, "ordinary-session relay join failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_relay_cursor_requires_a_feasible_signed_poll_window() {
        assert!(ExternalStructuredSessionRelayCursor::new(1_000, 30_000, 16).is_ok());
        assert!(ExternalStructuredSessionRelayCursor::new(0, 30_000, 16).is_err());
        assert!(ExternalStructuredSessionRelayCursor::new(20_000, 30_000, 16).is_err());
        assert!(ExternalStructuredSessionRelayCursor::new(1_000, 30_000, 0).is_err());
    }

    #[test]
    fn ordinary_relay_retry_uses_signed_interval_and_durable_read_allowance() {
        let schedule = RelaySchedule {
            poll_interval_ms: 1_000,
            poll_deadline_ms: 30_000,
        };
        assert!(schedule
            .pending_delay(
                ExternalSessionRelayPending {
                    read_attempts: 1,
                    deadline_ms: 20_000,
                    poll_interval_ms: 1_000,
                },
                1_000,
            )
            .is_ok());
        assert!(schedule
            .pending_delay(
                ExternalSessionRelayPending {
                    read_attempts: 0,
                    deadline_ms: 20_000,
                    poll_interval_ms: 1_000,
                },
                1_000,
            )
            .is_err());
        assert!(schedule
            .pending_delay(
                ExternalSessionRelayPending {
                    read_attempts: 1,
                    deadline_ms: 20_000,
                    poll_interval_ms: 2_000,
                },
                1_000,
            )
            .is_err());
    }

    #[test]
    fn signed_maximum_frame_exhaustion_is_terminal_not_normal_stop() {
        let mut cursor = ExternalStructuredSessionRelayCursor::new(1_000, 30_000, 1).unwrap();
        cursor
            .observe(
                ExternalSessionRelayStepProgress::ControllerResponseAccepted,
                0,
            )
            .unwrap();
        assert!(matches!(
            cursor.step,
            Some(ExternalSessionRelayStep::Exchange { sequence: 1 })
        ));
        cursor
            .observe(
                ExternalSessionRelayStepProgress::ControllerResponseAccepted,
                1,
            )
            .unwrap();
        assert!(cursor.frame_budget_exhausted());
        // run() checks this before observing stop and publishes the typed
        // SignedFrameBudgetExhausted health cause in the thread wrapper.
    }
}
