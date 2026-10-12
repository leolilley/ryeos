//! Narrow scheduler observation and retained maintenance contracts.
//! Concrete runtime, gate, database and classification belong to composition.

pub use crate::thread_lifecycle::DurableThreadTerminal;
use anyhow::Result;
use ryeos_scheduler_contract::ProjectScheduleDeployParticipant;
use ryeos_state::gc::retention::FireRetentionTarget;
use std::future::Future;
use std::pin::Pin;

pub type SchedulerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalObservationDisposition {
    Recorded,
    NotScheduled,
    Deferred,
}

pub trait SchedulerParticipant: ProjectScheduleDeployParticipant + Send + Sync {
    fn observe_terminal(
        &self,
        observation: &DurableThreadTerminal<'_>,
    ) -> Result<TerminalObservationDisposition>;
    fn acquire_retention(&self) -> SchedulerFuture<'_, Box<dyn SchedulerRetentionGuard>>;
}

/// Owns the service's runtime write gate until dropped. Implementations verify
/// current projection before returning and bind all operations to the same
/// descriptor-pinned runtime and DB. Failed mutation leaves repair required.
pub trait SchedulerRetentionGuard: Send + Sync {
    /// Called under StateStore's inner lock: short DB access only, no re-entry.
    fn count_thread_pins(&self, thread_ids: &[String]) -> Result<u64>;
    /// Exact retained scheduler snapshot roots for the existing CAS sweep.
    /// Uses the guard's current bound DB; unavailable interpretation is an
    /// error, not an empty root set. No raw DB or spec records cross this port.
    fn operational_snapshot_roots(&self) -> Result<Vec<String>>;
    /// Reject a mismatched runtime before outbox or projection mutation.
    fn prepare_fire_retention(
        &mut self,
        runtime: &lillux::PinnedDirectory,
        dry_run: bool,
    ) -> Result<()>;
    /// Requires prepare; validates exact targets and repairs projection/cursor.
    fn finish_fire_retention(&mut self, targets: &[FireRetentionTarget]) -> Result<usize>;
    /// Recover a failed mutating retention while retaining this same gate and
    /// pinned runtime. Rebuild authoritative fires and existing spec cursors
    /// in the service; reject an invalid phase and preserve invalid projection
    /// if repair fails. Never substitutes a new store or infers empty history.
    fn repair_fire_retention(&mut self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct ObservationOnly;
    impl ProjectScheduleDeployParticipant for ObservationOnly {
        fn acquire_project_schedules(
            &self,
        ) -> ryeos_scheduler_contract::ProjectScheduleFuture<
            '_,
            Box<dyn ryeos_scheduler_contract::ProjectScheduleSession>,
        > {
            Box::pin(async { anyhow::bail!("test participant has no project schedule authority") })
        }

        fn try_acquire_project_schedules(
            &self,
        ) -> Result<Option<Box<dyn ryeos_scheduler_contract::ProjectScheduleSession>>> {
            anyhow::bail!("test participant has no project schedule authority")
        }
    }
    impl SchedulerParticipant for ObservationOnly {
        fn observe_terminal(
            &self,
            _: &DurableThreadTerminal<'_>,
        ) -> Result<TerminalObservationDisposition> {
            Ok(TerminalObservationDisposition::Deferred)
        }
        fn acquire_retention(&self) -> SchedulerFuture<'_, Box<dyn SchedulerRetentionGuard>> {
            Box::pin(async { anyhow::bail!("test participant has no retention authority") })
        }
    }

    #[test]
    fn app_and_lifecycle_share_one_participant_until_shutdown_withdrawal() {
        let root = tempfile::TempDir::new().unwrap();
        let (_state_owner, state) = crate::state::test_support::build(root.path()).unwrap();
        assert!(state.scheduler().is_none());
        let participant: Arc<dyn SchedulerParticipant> = Arc::new(ObservationOnly);
        state
            .install_scheduler_participant(participant.clone())
            .unwrap();
        assert!(Arc::ptr_eq(&state.scheduler().unwrap(), &participant));
        assert!(Arc::ptr_eq(
            &state.threads.scheduler_participant().unwrap(),
            &participant
        ));
        assert!(
            state
                .install_scheduler_participant(Arc::new(ObservationOnly))
                .is_err()
        );
        assert!(Arc::ptr_eq(&state.scheduler().unwrap(), &participant));
        assert!(Arc::ptr_eq(
            &state.take_scheduler_participant_for_shutdown().unwrap(),
            &participant
        ));
        assert!(state.scheduler().is_none());
        assert!(state
            .install_scheduler_participant(Arc::new(ObservationOnly))
            .is_err());
    }
}
