//! Process-local scheduling for the app-owned Q relay journal.
//!
//! Startup lends this cursor synchronously between start advancements. Only
//! Ready may move it to a joinable pump; dropping that pump stops and joins
//! before the caller releases its remote cleanup obligation.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;

use anyhow::{Context as _, Result, ensure};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_app::external_placement::{
    ExternalQualificationRelayPending, ExternalQualificationRelayStep,
    ExternalQualificationRelayStepProgress, advance_external_qualification_relay_step,
};

const MAXIMUM_SLOT_READS: u32 = 128;

#[derive(Clone, Copy)]
struct RelaySchedule {
    poll_interval_ms: u32,
    poll_deadline_ms: u32,
}

impl RelaySchedule {
    fn pending_delay(
        &self,
        pending: ExternalQualificationRelayPending,
        now_ms: i64,
    ) -> Result<Duration> {
        ensure!(
            pending.poll_interval_ms == self.poll_interval_ms
                && (1..MAXIMUM_SLOT_READS).contains(&pending.read_attempts),
            "Q relay pending changed its signed interval or exhausted its durable GET allowance"
        );
        let remaining_ms = pending
            .deadline_ms
            .checked_sub(now_ms)
            .filter(|remaining| *remaining > 0)
            .context("Q relay exact slot deadline elapsed")?;
        // Spread the remaining allowance across the whole retained slot. A
        // short startup loop must not consume the reads needed for a late
        // Worker/native frame. Target the final read one signed interval
        // before expiry, so no retry is intentionally scheduled at expiry.
        let remaining_reads = u64::from(MAXIMUM_SLOT_READS - pending.read_attempts);
        let interval_ms = u64::from(self.poll_interval_ms);
        let retry_window_ms = u64::try_from(remaining_ms)?.saturating_sub(interval_ms);
        let delay_ms = interval_ms.max(retry_window_ms.div_ceil(remaining_reads));
        // Leave at least one signed guest poll before its response timeout.
        // Provider contact latency is additionally bounded by the app. This
        // proves scheduling feasibility, never successful remote delivery.
        ensure!(
            delay_ms + u64::from(self.poll_interval_ms) < u64::from(self.poll_deadline_ms),
            "Q relay GET allowance cannot cover its slot within the signed guest polling window"
        );
        ensure!(
            delay_ms < u64::try_from(remaining_ms)?,
            "Q relay has no retry before its exact slot deadline"
        );
        Ok(Duration::from_millis(delay_ms))
    }
}

pub(crate) struct ExternalQualificationRelayCursor {
    step: Option<ExternalQualificationRelayStep>,
    maximum_frames: u32,
    schedule: RelaySchedule,
    next_contact: MonotonicDeadline,
    slot_deadline_ms: Option<i64>,
    channel_deadline_ms: Option<i64>,
}

impl ExternalQualificationRelayCursor {
    pub(crate) fn new(
        poll_interval_ms: u32,
        poll_deadline_ms: u32,
        maximum_frames: u32,
    ) -> Result<Self> {
        ensure!(
            poll_interval_ms > 0
                && maximum_frames > 0
                && u64::from(poll_interval_ms) * 2 < u64::from(poll_deadline_ms),
            "Q relay has no feasible signed polling window or frame allowance"
        );
        Ok(Self {
            step: Some(ExternalQualificationRelayStep::Attach),
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
            .context("Q relay exhausted its signed frame allowance before readiness")?;
        if !self.next_contact.has_elapsed() {
            return Ok(());
        }
        if matches!(step, ExternalQualificationRelayStep::Exchange { .. })
            && self.channel_deadline_ms.is_none()
        {
            // The channel clock starts at attachment, while the relay intent
            // uses the later bootstrap attachment deadline. Both bounds apply.
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
                "Q relay channel expired before contact"
            );
        }
        if let Some(deadline) = self.slot_deadline_ms {
            ensure!(
                lillux::time::timestamp_millis() < deadline,
                "Q relay exact slot deadline elapsed before retry"
            );
        }
        let progress = advance_external_qualification_relay_step(state, placement, step)?;
        self.observe(progress, lillux::time::timestamp_millis())
    }

    fn observe(
        &mut self,
        progress: ExternalQualificationRelayStepProgress,
        now_ms: i64,
    ) -> Result<()> {
        match progress {
            ExternalQualificationRelayStepProgress::GuestRequestPending(pending)
            | ExternalQualificationRelayStepProgress::ControllerResponsePending(pending) => {
                // In particular an ambiguous PUT retains this exact cursor;
                // the app can only GET its already committed response slot.
                let mut bounded = pending;
                if let Some(deadline) = self.channel_deadline_ms {
                    bounded.deadline_ms = bounded.deadline_ms.min(deadline);
                }
                let delay = self.schedule.pending_delay(bounded, now_ms)?;
                if let Some(deadline) = self.slot_deadline_ms {
                    ensure!(
                        deadline == pending.deadline_ms,
                        "Q relay pending changed its exact slot deadline"
                    );
                }
                self.slot_deadline_ms = Some(pending.deadline_ms);
                self.next_contact = MonotonicDeadline::after(delay);
            }
            ExternalQualificationRelayStepProgress::ControllerResponseAccepted => {
                self.step = match self
                    .step
                    .context("Q relay accepted beyond its signed frame allowance")?
                {
                    ExternalQualificationRelayStep::Attach => {
                        Some(ExternalQualificationRelayStep::Exchange { sequence: 1 })
                    }
                    ExternalQualificationRelayStep::Exchange { sequence }
                        if sequence < self.maximum_frames =>
                    {
                        Some(ExternalQualificationRelayStep::Exchange {
                            sequence: sequence + 1,
                        })
                    }
                    ExternalQualificationRelayStep::Exchange { .. } => None,
                };
                self.slot_deadline_ms = None;
                self.next_contact = MonotonicDeadline::after(Duration::ZERO);
            }
        }
        Ok(())
    }

    fn run(
        mut self,
        state: ryeos_app::state::AppState,
        placement: String,
        stop: Receiver<()>,
    ) -> Result<()> {
        loop {
            // The stop channel interrupts even a long cadence. A provider
            // call already in progress remains joined until its app deadline.
            let stopped = match self.step {
                // Finishing the signed frame allowance is observable to the
                // owner. Only its independent native proof can accept this
                // as normal completion; otherwise ensure_running fails.
                None => return Ok(()),
                Some(_) => match stop.recv_timeout(self.next_contact.remaining()) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
                    Err(RecvTimeoutError::Timeout) => false,
                },
            };
            if stopped {
                return Ok(());
            }
            self.advance_if_due(&state, &placement)?;
        }
    }
}

/// Must be retained ahead of the cleanup lifeline in the owning Ready value.
/// No join handle is detached, including unwinding and early error returns.
pub(crate) struct ExternalQualificationRelayPump {
    stop: Sender<()>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl ExternalQualificationRelayPump {
    pub(crate) fn start(
        state: &ryeos_app::state::AppState,
        placement: String,
        cursor: ExternalQualificationRelayCursor,
    ) -> Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let state = state.clone();
        let thread = std::thread::Builder::new()
            .name("qualification-relay".into())
            .spawn(move || cursor.run(state, placement, receiver))
            .context("start joinable Q relay pump")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub(crate) fn ensure_running(&self) -> Result<()> {
        ensure!(
            self.thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished()),
            "Q relay pump failed or exhausted its signed frames before Worker/native settlement"
        );
        Ok(())
    }

    pub(crate) fn stop_and_join(&mut self) -> Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        let _ = self.stop.send(());
        thread
            .join()
            .map_err(|_| anyhow::anyhow!("Q relay pump panicked"))?
    }
}

impl Drop for ExternalQualificationRelayPump {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_join() {
            tracing::error!(%error, "Q relay stopped with an error before cleanup handoff");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(attempts: u32, deadline: i64) -> ExternalQualificationRelayPending {
        ExternalQualificationRelayPending {
            read_attempts: attempts,
            deadline_ms: deadline,
            poll_interval_ms: 1000,
        }
    }

    #[test]
    fn late_slot_reads_are_spread_over_the_retained_deadline() {
        let schedule = RelaySchedule {
            poll_interval_ms: 1000,
            poll_deadline_ms: 30000,
        };
        assert_eq!(
            schedule.pending_delay(pending(1, 1_270_000), 0).unwrap(),
            Duration::from_millis(9993)
        );
        assert_eq!(
            schedule
                .pending_delay(pending(127, 1_270_000), 1_260_000)
                .unwrap(),
            Duration::from_secs(9)
        );
        assert!(
            schedule
                .pending_delay(pending(128, 1_270_000), 1_260_000)
                .is_err()
        );
        assert!(schedule.pending_delay(pending(1, 4_000_000), 0).is_err());
        assert!(schedule.pending_delay(pending(1, 1000), 1000).is_err());
    }

    #[test]
    fn ambiguity_stays_on_the_same_slot_until_exact_response_acceptance() {
        let mut cursor = ExternalQualificationRelayCursor::new(1000, 30000, 2).unwrap();
        cursor
            .observe(
                ExternalQualificationRelayStepProgress::GuestRequestPending(pending(1, 10000)),
                0,
            )
            .unwrap();
        cursor
            .observe(
                ExternalQualificationRelayStepProgress::ControllerResponsePending(pending(
                    1, 10000,
                )),
                0,
            )
            .unwrap();
        assert_eq!(cursor.step, Some(ExternalQualificationRelayStep::Attach));
        for sequence in 1..=2 {
            cursor
                .observe(
                    ExternalQualificationRelayStepProgress::ControllerResponseAccepted,
                    0,
                )
                .unwrap();
            assert_eq!(
                cursor.step,
                Some(ExternalQualificationRelayStep::Exchange { sequence })
            );
        }
        cursor
            .observe(
                ExternalQualificationRelayStepProgress::ControllerResponseAccepted,
                0,
            )
            .unwrap();
        assert_eq!(cursor.step, None);
    }

    #[test]
    fn pending_retry_cannot_use_the_later_intent_deadline_after_channel_expiry() {
        let mut cursor = ExternalQualificationRelayCursor::new(1000, 30000, 2).unwrap();
        cursor
            .observe(
                ExternalQualificationRelayStepProgress::ControllerResponseAccepted,
                0,
            )
            .unwrap();
        cursor.channel_deadline_ms = Some(1000);
        assert!(
            cursor
                .observe(
                    ExternalQualificationRelayStepProgress::GuestRequestPending(pending(1, 10000)),
                    0
                )
                .is_err()
        );
    }

    #[test]
    fn dropping_pump_joins_before_the_following_cleanup_owner() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Cleanup(Arc<AtomicBool>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                assert!(self.0.load(Ordering::Acquire));
            }
        }
        struct Owners {
            _pump: ExternalQualificationRelayPump,
            _cleanup: Cleanup,
        }
        let joined = Arc::new(AtomicBool::new(false));
        let finished = Arc::clone(&joined);
        let (stop, receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            receiver.recv().unwrap();
            finished.store(true, Ordering::Release);
            Ok(())
        });
        let owners = Owners {
            _pump: ExternalQualificationRelayPump {
                stop,
                thread: Some(thread),
            },
            _cleanup: Cleanup(joined),
        };
        drop(owners);
    }
}
