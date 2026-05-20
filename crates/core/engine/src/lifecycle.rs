//! Thread lifecycle state machine.
//!
//! Implements the thread lifecycle from 17-lifecycle-state-machines.md §1.
//!
//! States: Pending → Running → {Cancelling, Interrupting, Finalizing}
//!         → {Completed, Failed, Cancelled, Continued, Killed}
//!
//! Terminal states reject all transitions.

use crate::error::EngineError;

/// Thread lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Pending,
    Running,
    Cancelling,
    Interrupting,
    Finalizing,
    Completed,
    Failed,
    Cancelled,
    Continued,
    Killed,
}

/// Events that drive thread state transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadEvent {
    Start,
    Cancel,
    Interrupt,
    Kill,
    ExecutionComplete,
    ExecutionFailed,
    CooperativeShutdownComplete,
    StateSaved,
    GracePeriodExpired,
    ContinuationCreated,
    FinalizeCompleted,
    FinalizeFailed,
    FinalizeCancelled,
}

impl ThreadState {
    /// Returns `true` if the state is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Continued | Self::Killed
        )
    }

    /// Attempt a state transition given an event.
    ///
    /// Returns the new state on valid transitions, or
    /// `EngineError::InvalidStateTransition` on invalid ones.
    pub fn transition(&self, event: ThreadEvent) -> Result<ThreadState, EngineError> {
        use ThreadEvent::*;
        use ThreadState::*;

        let next = match (self, event) {
            // Pending
            (Pending, Start) => Running,
            (Pending, Kill) => Killed,

            // Running
            (Running, ExecutionComplete) => Finalizing,
            (Running, ExecutionFailed) => Finalizing,
            (Running, Cancel) => Cancelling,
            (Running, Interrupt) => Interrupting,
            (Running, Kill) => Killed,

            // Cancelling
            (Cancelling, CooperativeShutdownComplete) => Finalizing,
            (Cancelling, GracePeriodExpired) => Killed,
            (Cancelling, Kill) => Killed,

            // Interrupting
            (Interrupting, StateSaved) => Finalizing,
            (Interrupting, GracePeriodExpired) => Killed,
            (Interrupting, Kill) => Killed,

            // Finalizing
            (Finalizing, FinalizeCompleted) => Completed,
            (Finalizing, FinalizeFailed) => Failed,
            (Finalizing, FinalizeCancelled) => Cancelled,
            (Finalizing, ContinuationCreated) => Continued,
            (Finalizing, Kill) => Killed,

            // Terminal states reject everything
            _ if self.is_terminal() => {
                return Err(EngineError::InvalidStateTransition {
                    from: format!("{self:?}"),
                    event: format!("{event:?}"),
                });
            }

            // Any other combination is invalid
            _ => {
                return Err(EngineError::InvalidStateTransition {
                    from: format!("{self:?}"),
                    event: format!("{event:?}"),
                });
            }
        };

        Ok(next)
    }
}

// ── Mirror thread lifecycle (§2) ────────────────────────────────────

/// Mirror thread lifecycle states.
///
/// Tracks the local view of a remotely-forwarded execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorState {
    Pending,
    Accepted,
    Mirroring,
    Stale,
    Completed,
    Failed,
    Abandoned,
}

/// Events that drive mirror state transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorEvent {
    RemoteAccepted,
    RemoteRejected,
    LocalTimeout,
    FirstEventReceived,
    RemoteCompleted,
    RemoteFailed,
    StreamInterrupted,
    Reconnected,
    ReconnectTimeout,
    ReconnectedTerminal,
}

impl MirrorState {
    /// Returns `true` if the state is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Abandoned)
    }

    /// Attempt a state transition given an event.
    pub fn transition(&self, event: MirrorEvent) -> Result<MirrorState, EngineError> {
        use MirrorEvent::*;
        use MirrorState::*;

        let next = match (self, event) {
            // Pending
            (Pending, RemoteAccepted) => Accepted,
            (Pending, RemoteRejected) => Failed,
            (Pending, LocalTimeout) => Abandoned,

            // Accepted
            (Accepted, FirstEventReceived) => Mirroring,
            (Accepted, RemoteCompleted) => Completed,
            (Accepted, RemoteFailed) => Failed,

            // Mirroring
            (Mirroring, StreamInterrupted) => Stale,
            (Mirroring, RemoteCompleted) => Completed,
            (Mirroring, RemoteFailed) => Failed,

            // Stale
            (Stale, Reconnected) => Mirroring,
            (Stale, ReconnectTimeout) => Abandoned,
            (Stale, ReconnectedTerminal) => Completed,

            // Terminal states reject everything
            _ if self.is_terminal() => {
                return Err(EngineError::InvalidStateTransition {
                    from: format!("{self:?}"),
                    event: format!("{event:?}"),
                });
            }

            // Any other combination is invalid
            _ => {
                return Err(EngineError::InvalidStateTransition {
                    from: format!("{self:?}"),
                    event: format!("{event:?}"),
                });
            }
        };

        Ok(next)
    }
}

// ── Remote accept/execute lifecycle (§7) ────────────────────────────

/// Remote execution lifecycle states.
///
/// Tracks the receiving node's view of a delegated execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteExecState {
    Validating,
    Materializing,
    Rejected,
    Accepted,
    Executing,
    Reporting,
    Settled,
}

/// Events that drive remote execution state transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteExecEvent {
    DelegationValid,
    DelegationInvalid,
    SnapshotMaterialized,
    SnapshotUnavailable,
    ThreadStarted,
    ThreadTerminal,
    SettlementAccepted,
}

impl RemoteExecState {
    /// Returns `true` if the state is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Rejected | Self::Settled)
    }

    /// Attempt a state transition given an event.
    pub fn transition(&self, event: RemoteExecEvent) -> Result<RemoteExecState, EngineError> {
        use RemoteExecEvent::*;
        use RemoteExecState::*;

        let next = match (self, event) {
            // Validating
            (Validating, DelegationValid) => Materializing,
            (Validating, DelegationInvalid) => Rejected,

            // Materializing
            (Materializing, SnapshotMaterialized) => Accepted,
            (Materializing, SnapshotUnavailable) => Rejected,

            // Accepted
            (Accepted, ThreadStarted) => Executing,

            // Executing
            (Executing, ThreadTerminal) => Reporting,

            // Reporting
            (Reporting, SettlementAccepted) => Settled,

            // Terminal states reject everything
            _ if self.is_terminal() => {
                return Err(EngineError::InvalidStateTransition {
                    from: format!("{self:?}"),
                    event: format!("{event:?}"),
                });
            }

            // Any other combination is invalid
            _ => {
                return Err(EngineError::InvalidStateTransition {
                    from: format!("{self:?}"),
                    event: format!("{event:?}"),
                });
            }
        };

        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EngineError;

    // ── Valid transitions ────────────────────────────────────────────

    #[test]
    fn pending_start_to_running() {
        assert_eq!(
            ThreadState::Pending.transition(ThreadEvent::Start).unwrap(),
            ThreadState::Running
        );
    }

    #[test]
    fn pending_kill_to_killed() {
        assert_eq!(
            ThreadState::Pending.transition(ThreadEvent::Kill).unwrap(),
            ThreadState::Killed
        );
    }

    #[test]
    fn running_execution_complete_to_finalizing() {
        assert_eq!(
            ThreadState::Running
                .transition(ThreadEvent::ExecutionComplete)
                .unwrap(),
            ThreadState::Finalizing
        );
    }

    #[test]
    fn running_execution_failed_to_finalizing() {
        assert_eq!(
            ThreadState::Running
                .transition(ThreadEvent::ExecutionFailed)
                .unwrap(),
            ThreadState::Finalizing
        );
    }

    #[test]
    fn running_cancel_to_cancelling() {
        assert_eq!(
            ThreadState::Running
                .transition(ThreadEvent::Cancel)
                .unwrap(),
            ThreadState::Cancelling
        );
    }

    #[test]
    fn running_interrupt_to_interrupting() {
        assert_eq!(
            ThreadState::Running
                .transition(ThreadEvent::Interrupt)
                .unwrap(),
            ThreadState::Interrupting
        );
    }

    #[test]
    fn running_kill_to_killed() {
        assert_eq!(
            ThreadState::Running.transition(ThreadEvent::Kill).unwrap(),
            ThreadState::Killed
        );
    }

    #[test]
    fn cancelling_cooperative_shutdown_to_finalizing() {
        assert_eq!(
            ThreadState::Cancelling
                .transition(ThreadEvent::CooperativeShutdownComplete)
                .unwrap(),
            ThreadState::Finalizing
        );
    }

    #[test]
    fn cancelling_grace_period_to_killed() {
        assert_eq!(
            ThreadState::Cancelling
                .transition(ThreadEvent::GracePeriodExpired)
                .unwrap(),
            ThreadState::Killed
        );
    }

    #[test]
    fn cancelling_kill_to_killed() {
        assert_eq!(
            ThreadState::Cancelling
                .transition(ThreadEvent::Kill)
                .unwrap(),
            ThreadState::Killed
        );
    }

    #[test]
    fn interrupting_state_saved_to_finalizing() {
        assert_eq!(
            ThreadState::Interrupting
                .transition(ThreadEvent::StateSaved)
                .unwrap(),
            ThreadState::Finalizing
        );
    }

    #[test]
    fn interrupting_grace_period_to_killed() {
        assert_eq!(
            ThreadState::Interrupting
                .transition(ThreadEvent::GracePeriodExpired)
                .unwrap(),
            ThreadState::Killed
        );
    }

    #[test]
    fn interrupting_kill_to_killed() {
        assert_eq!(
            ThreadState::Interrupting
                .transition(ThreadEvent::Kill)
                .unwrap(),
            ThreadState::Killed
        );
    }

    #[test]
    fn finalizing_continuation_to_continued() {
        assert_eq!(
            ThreadState::Finalizing
                .transition(ThreadEvent::ContinuationCreated)
                .unwrap(),
            ThreadState::Continued
        );
    }

    #[test]
    fn finalizing_finalize_completed_to_completed() {
        assert_eq!(
            ThreadState::Finalizing
                .transition(ThreadEvent::FinalizeCompleted)
                .unwrap(),
            ThreadState::Completed
        );
    }

    #[test]
    fn finalizing_finalize_failed_to_failed() {
        assert_eq!(
            ThreadState::Finalizing
                .transition(ThreadEvent::FinalizeFailed)
                .unwrap(),
            ThreadState::Failed
        );
    }

    #[test]
    fn finalizing_finalize_cancelled_to_cancelled() {
        assert_eq!(
            ThreadState::Finalizing
                .transition(ThreadEvent::FinalizeCancelled)
                .unwrap(),
            ThreadState::Cancelled
        );
    }

    #[test]
    fn finalizing_kill_to_killed() {
        assert_eq!(
            ThreadState::Finalizing
                .transition(ThreadEvent::Kill)
                .unwrap(),
            ThreadState::Killed
        );
    }

    // ── Terminal states ─────────────────────────────────────────────

    #[test]
    fn terminal_states_are_terminal() {
        assert!(ThreadState::Completed.is_terminal());
        assert!(ThreadState::Failed.is_terminal());
        assert!(ThreadState::Cancelled.is_terminal());
        assert!(ThreadState::Continued.is_terminal());
        assert!(ThreadState::Killed.is_terminal());
    }

    #[test]
    fn non_terminal_states_are_not_terminal() {
        assert!(!ThreadState::Pending.is_terminal());
        assert!(!ThreadState::Running.is_terminal());
        assert!(!ThreadState::Cancelling.is_terminal());
        assert!(!ThreadState::Interrupting.is_terminal());
        assert!(!ThreadState::Finalizing.is_terminal());
    }

    #[test]
    fn terminal_states_reject_all_events() {
        let terminals = [
            ThreadState::Completed,
            ThreadState::Failed,
            ThreadState::Cancelled,
            ThreadState::Continued,
            ThreadState::Killed,
        ];
        let events = [
            ThreadEvent::Start,
            ThreadEvent::Cancel,
            ThreadEvent::Interrupt,
            ThreadEvent::Kill,
            ThreadEvent::ExecutionComplete,
            ThreadEvent::ExecutionFailed,
            ThreadEvent::CooperativeShutdownComplete,
            ThreadEvent::StateSaved,
            ThreadEvent::GracePeriodExpired,
            ThreadEvent::ContinuationCreated,
            ThreadEvent::FinalizeCompleted,
            ThreadEvent::FinalizeFailed,
            ThreadEvent::FinalizeCancelled,
        ];

        for state in &terminals {
            for event in &events {
                let err = state.transition(*event).unwrap_err();
                assert!(
                    matches!(err, EngineError::InvalidStateTransition { .. }),
                    "expected InvalidStateTransition for {state:?} + {event:?}, got: {err:?}"
                );
            }
        }
    }

    // ── Invalid transitions ─────────────────────────────────────────

    #[test]
    fn pending_rejects_cancel() {
        let err = ThreadState::Pending
            .transition(ThreadEvent::Cancel)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn pending_rejects_execution_complete() {
        let err = ThreadState::Pending
            .transition(ThreadEvent::ExecutionComplete)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn running_rejects_start() {
        let err = ThreadState::Running
            .transition(ThreadEvent::Start)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn cancelling_rejects_execution_complete() {
        let err = ThreadState::Cancelling
            .transition(ThreadEvent::ExecutionComplete)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn interrupting_rejects_cancel() {
        let err = ThreadState::Interrupting
            .transition(ThreadEvent::Cancel)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn finalizing_rejects_start() {
        let err = ThreadState::Finalizing
            .transition(ThreadEvent::Start)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn finalizing_rejects_execution_complete() {
        let err = ThreadState::Finalizing
            .transition(ThreadEvent::ExecutionComplete)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn running_through_finalizing_to_completed() {
        let s = ThreadState::Running
            .transition(ThreadEvent::ExecutionComplete)
            .unwrap();
        assert_eq!(s, ThreadState::Finalizing);
        let s = s.transition(ThreadEvent::FinalizeCompleted).unwrap();
        assert_eq!(s, ThreadState::Completed);
    }

    #[test]
    fn running_through_finalizing_to_failed() {
        let s = ThreadState::Running
            .transition(ThreadEvent::ExecutionFailed)
            .unwrap();
        assert_eq!(s, ThreadState::Finalizing);
        let s = s.transition(ThreadEvent::FinalizeFailed).unwrap();
        assert_eq!(s, ThreadState::Failed);
    }

    #[test]
    fn cancelling_through_finalizing_to_cancelled() {
        let s = ThreadState::Cancelling
            .transition(ThreadEvent::CooperativeShutdownComplete)
            .unwrap();
        assert_eq!(s, ThreadState::Finalizing);
        let s = s.transition(ThreadEvent::FinalizeCancelled).unwrap();
        assert_eq!(s, ThreadState::Cancelled);
    }

    // ── Mirror lifecycle: valid transitions ─────────────────────────

    #[test]
    fn mirror_pending_accepted() {
        assert_eq!(
            MirrorState::Pending
                .transition(MirrorEvent::RemoteAccepted)
                .unwrap(),
            MirrorState::Accepted
        );
    }

    #[test]
    fn mirror_pending_rejected_to_failed() {
        assert_eq!(
            MirrorState::Pending
                .transition(MirrorEvent::RemoteRejected)
                .unwrap(),
            MirrorState::Failed
        );
    }

    #[test]
    fn mirror_pending_timeout_to_abandoned() {
        assert_eq!(
            MirrorState::Pending
                .transition(MirrorEvent::LocalTimeout)
                .unwrap(),
            MirrorState::Abandoned
        );
    }

    #[test]
    fn mirror_accepted_first_event_to_mirroring() {
        assert_eq!(
            MirrorState::Accepted
                .transition(MirrorEvent::FirstEventReceived)
                .unwrap(),
            MirrorState::Mirroring
        );
    }

    #[test]
    fn mirror_accepted_completed() {
        assert_eq!(
            MirrorState::Accepted
                .transition(MirrorEvent::RemoteCompleted)
                .unwrap(),
            MirrorState::Completed
        );
    }

    #[test]
    fn mirror_accepted_failed() {
        assert_eq!(
            MirrorState::Accepted
                .transition(MirrorEvent::RemoteFailed)
                .unwrap(),
            MirrorState::Failed
        );
    }

    #[test]
    fn mirror_mirroring_interrupted_to_stale() {
        assert_eq!(
            MirrorState::Mirroring
                .transition(MirrorEvent::StreamInterrupted)
                .unwrap(),
            MirrorState::Stale
        );
    }

    #[test]
    fn mirror_mirroring_completed() {
        assert_eq!(
            MirrorState::Mirroring
                .transition(MirrorEvent::RemoteCompleted)
                .unwrap(),
            MirrorState::Completed
        );
    }

    #[test]
    fn mirror_mirroring_failed() {
        assert_eq!(
            MirrorState::Mirroring
                .transition(MirrorEvent::RemoteFailed)
                .unwrap(),
            MirrorState::Failed
        );
    }

    #[test]
    fn mirror_stale_reconnected_to_mirroring() {
        assert_eq!(
            MirrorState::Stale
                .transition(MirrorEvent::Reconnected)
                .unwrap(),
            MirrorState::Mirroring
        );
    }

    #[test]
    fn mirror_stale_reconnect_timeout_to_abandoned() {
        assert_eq!(
            MirrorState::Stale
                .transition(MirrorEvent::ReconnectTimeout)
                .unwrap(),
            MirrorState::Abandoned
        );
    }

    #[test]
    fn mirror_stale_reconnected_terminal_to_completed() {
        assert_eq!(
            MirrorState::Stale
                .transition(MirrorEvent::ReconnectedTerminal)
                .unwrap(),
            MirrorState::Completed
        );
    }

    // ── Mirror lifecycle: terminal & invalid ────────────────────────

    #[test]
    fn mirror_terminal_states_are_terminal() {
        assert!(MirrorState::Completed.is_terminal());
        assert!(MirrorState::Failed.is_terminal());
        assert!(MirrorState::Abandoned.is_terminal());
    }

    #[test]
    fn mirror_non_terminal_states_are_not_terminal() {
        assert!(!MirrorState::Pending.is_terminal());
        assert!(!MirrorState::Accepted.is_terminal());
        assert!(!MirrorState::Mirroring.is_terminal());
        assert!(!MirrorState::Stale.is_terminal());
    }

    #[test]
    fn mirror_terminal_states_reject_all_events() {
        let terminals = [
            MirrorState::Completed,
            MirrorState::Failed,
            MirrorState::Abandoned,
        ];
        let events = [
            MirrorEvent::RemoteAccepted,
            MirrorEvent::RemoteRejected,
            MirrorEvent::LocalTimeout,
            MirrorEvent::FirstEventReceived,
            MirrorEvent::RemoteCompleted,
            MirrorEvent::RemoteFailed,
            MirrorEvent::StreamInterrupted,
            MirrorEvent::Reconnected,
            MirrorEvent::ReconnectTimeout,
            MirrorEvent::ReconnectedTerminal,
        ];

        for state in &terminals {
            for event in &events {
                let err = state.transition(*event).unwrap_err();
                assert!(
                    matches!(err, EngineError::InvalidStateTransition { .. }),
                    "expected InvalidStateTransition for {state:?} + {event:?}, got: {err:?}"
                );
            }
        }
    }

    #[test]
    fn mirror_pending_rejects_stream_interrupted() {
        let err = MirrorState::Pending
            .transition(MirrorEvent::StreamInterrupted)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn mirror_accepted_rejects_reconnected() {
        let err = MirrorState::Accepted
            .transition(MirrorEvent::Reconnected)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn mirror_mirroring_rejects_remote_accepted() {
        let err = MirrorState::Mirroring
            .transition(MirrorEvent::RemoteAccepted)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    // ── Remote exec lifecycle: valid transitions ────────────────────

    #[test]
    fn remote_exec_validating_valid_to_materializing() {
        assert_eq!(
            RemoteExecState::Validating
                .transition(RemoteExecEvent::DelegationValid)
                .unwrap(),
            RemoteExecState::Materializing
        );
    }

    #[test]
    fn remote_exec_validating_invalid_to_rejected() {
        assert_eq!(
            RemoteExecState::Validating
                .transition(RemoteExecEvent::DelegationInvalid)
                .unwrap(),
            RemoteExecState::Rejected
        );
    }

    #[test]
    fn remote_exec_materializing_materialized_to_accepted() {
        assert_eq!(
            RemoteExecState::Materializing
                .transition(RemoteExecEvent::SnapshotMaterialized)
                .unwrap(),
            RemoteExecState::Accepted
        );
    }

    #[test]
    fn remote_exec_materializing_unavailable_to_rejected() {
        assert_eq!(
            RemoteExecState::Materializing
                .transition(RemoteExecEvent::SnapshotUnavailable)
                .unwrap(),
            RemoteExecState::Rejected
        );
    }

    #[test]
    fn remote_exec_accepted_thread_started_to_executing() {
        assert_eq!(
            RemoteExecState::Accepted
                .transition(RemoteExecEvent::ThreadStarted)
                .unwrap(),
            RemoteExecState::Executing
        );
    }

    #[test]
    fn remote_exec_executing_terminal_to_reporting() {
        assert_eq!(
            RemoteExecState::Executing
                .transition(RemoteExecEvent::ThreadTerminal)
                .unwrap(),
            RemoteExecState::Reporting
        );
    }

    #[test]
    fn remote_exec_reporting_settlement_to_settled() {
        assert_eq!(
            RemoteExecState::Reporting
                .transition(RemoteExecEvent::SettlementAccepted)
                .unwrap(),
            RemoteExecState::Settled
        );
    }

    // ── Remote exec lifecycle: terminal & invalid ───────────────────

    #[test]
    fn remote_exec_terminal_states_are_terminal() {
        assert!(RemoteExecState::Rejected.is_terminal());
        assert!(RemoteExecState::Settled.is_terminal());
    }

    #[test]
    fn remote_exec_non_terminal_states_are_not_terminal() {
        assert!(!RemoteExecState::Validating.is_terminal());
        assert!(!RemoteExecState::Materializing.is_terminal());
        assert!(!RemoteExecState::Accepted.is_terminal());
        assert!(!RemoteExecState::Executing.is_terminal());
        assert!(!RemoteExecState::Reporting.is_terminal());
    }

    #[test]
    fn remote_exec_terminal_states_reject_all_events() {
        let terminals = [RemoteExecState::Rejected, RemoteExecState::Settled];
        let events = [
            RemoteExecEvent::DelegationValid,
            RemoteExecEvent::DelegationInvalid,
            RemoteExecEvent::SnapshotMaterialized,
            RemoteExecEvent::SnapshotUnavailable,
            RemoteExecEvent::ThreadStarted,
            RemoteExecEvent::ThreadTerminal,
            RemoteExecEvent::SettlementAccepted,
        ];

        for state in &terminals {
            for event in &events {
                let err = state.transition(*event).unwrap_err();
                assert!(
                    matches!(err, EngineError::InvalidStateTransition { .. }),
                    "expected InvalidStateTransition for {state:?} + {event:?}, got: {err:?}"
                );
            }
        }
    }

    #[test]
    fn remote_exec_validating_rejects_thread_started() {
        let err = RemoteExecState::Validating
            .transition(RemoteExecEvent::ThreadStarted)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn remote_exec_accepted_rejects_settlement() {
        let err = RemoteExecState::Accepted
            .transition(RemoteExecEvent::SettlementAccepted)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }

    #[test]
    fn remote_exec_executing_rejects_delegation_valid() {
        let err = RemoteExecState::Executing
            .transition(RemoteExecEvent::DelegationValid)
            .unwrap_err();
        assert!(matches!(err, EngineError::InvalidStateTransition { .. }));
    }
}
