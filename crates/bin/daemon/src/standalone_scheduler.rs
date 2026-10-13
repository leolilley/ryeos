//! Scheduler owner of a stopped-node projection rebuild.
//!
//! A projection rebuild recovers pending terminal-chain removals, and that
//! recovery asks the scheduler how many fires still pin each chain. The live
//! daemon answers through its scheduler service; a standalone rebuild has no
//! service, so this module opens the node's scheduler store on the
//! invocation's standalone native host, the way the daemon opens it on its
//! live factory, and serves the retention reads from that store. The node is
//! stopped: the owner observes no terminals, deploys no project schedules and
//! retains no fires. The store closes into the host's census before the host
//! finishes; every refusal keeps the owner it was raised on.

use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use ryeos_app::scheduler_boundary::{
    DurableThreadTerminal, SchedulerFuture, SchedulerParticipant, SchedulerRetentionGuard,
    TerminalObservationDisposition,
};
use ryeos_app::standalone_native::StandaloneNativeHost;
use ryeos_scheduler::db::SchedulerDb;
use ryeos_scheduler_contract::{
    ProjectScheduleDeployParticipant, ProjectScheduleFuture, ProjectScheduleSession,
};
use ryeos_state::gc::retention::FireRetentionTarget;
use ryeos_state::operational::CarriedOwners;
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};

/// The scheduler store of a stopped node, open for one projection rebuild.
pub struct StoppedNodeScheduler {
    db: Arc<SchedulerDb>,
    // Serializes retention guards the way the live service's runtime gate
    // does, so one rebuild's pin reads never interleave with another guard.
    gate: Arc<RwLock<()>>,
}

impl StoppedNodeScheduler {
    /// Open `scheduler.sqlite3` under `runtime` on `host`, then recover the
    /// durable fire outbox and any invalid fire projection, as the daemon
    /// does before its first pin inspection, so the pins this owner reports
    /// come from a complete projection.
    ///
    /// A refused open closes its owners into the host's census through the
    /// same failure value and returns that value inside the error.
    pub fn open(
        host: &StandaloneNativeHost,
        runtime: &Arc<lillux::PinnedDirectory>,
        identity: &ryeos_node_runtime::identity::NodeIdentity,
    ) -> Result<Arc<Self>> {
        let factory = Arc::clone(host.factory()?);
        ryeos_scheduler_service::validate_retirement_startup(&factory, runtime, identity)
            .context("validate scheduler retirement journal before the stopped-node scheduler open")?;
        let destination = host.destination()?;
        // SAFETY: the host's destination authority admits only this host's
        // factory on this node's runtime directory, under the caller's
        // StateLock and runtime namespace lock; only the fixed scheduler
        // schema-reference role is issued, and its terminal reaches the
        // host's census below on every path.
        let reference_inputs = unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::NodeSchedulerSchemaReference,
                Arc::clone(&destination),
            )
        }
        .context("issue the stopped-node scheduler schema reference inputs")?;
        let reference = match SchedulerDb::prepare_scheduler_schema_reference(
            Arc::clone(&factory),
            reference_inputs,
        ) {
            Ok(reference) => reference,
            Err(refused) => return Err(settle_refused_reference(host, &factory, refused)),
        };
        // SAFETY: as above; only the fixed scheduler database role is issued
        // at the scheduler's own coordinate, and the opened store closes into
        // this host's census before the host finishes.
        let inputs = unsafe {
            lillux_sqlite::OriginalNativeBoundInputs::issued(
                Arc::clone(runtime),
                OsStr::new("scheduler.sqlite3"),
                lillux_sqlite::OriginalNativeBoundMode::OpenOrCreate,
                lillux_sqlite::OriginalNativeDomain::NodeScheduler,
                destination,
                None,
            )
        };
        let inputs = match inputs {
            Ok(inputs) => inputs,
            Err(error) => {
                // The closed reference holds a census terminal and a factory
                // alias; it stays with the refusal.
                return Err(CarriedOwners::carry(
                    error.context("issue the stopped-node scheduler database inputs"),
                    reference,
                ));
            }
        };
        let db = match SchedulerDb::open_original_scheduler(factory, inputs, reference) {
            Ok(db) => Arc::new(db),
            Err(failure) => return Err(settle_refused_open(host, failure)),
        };
        let scheduler = Arc::new(Self {
            db,
            gate: Arc::new(RwLock::new(())),
        });
        if let Err(error) = scheduler.recover_fire_projection(runtime) {
            return Err(match Arc::clone(&scheduler).settle(host) {
                Ok(()) => error,
                Err(settle) => CarriedOwners::carry(
                    error.context(format!("stopped-node scheduler store did not settle: {settle:#}")),
                    settle,
                ),
            });
        }
        Ok(scheduler)
    }

    fn recover_fire_projection(&self, runtime: &lillux::PinnedDirectory) -> Result<()> {
        self.db
            .drain_fire_outbox_in_directory(runtime)
            .context("recover the scheduler fire outbox before projection rebuild")?;
        if !self.db.fire_projection_is_current()? {
            ryeos_scheduler::projection::rebuild_fires_from_runtime_directory(runtime, &self.db)
                .context("rebuild the scheduler fire projection before projection rebuild")?;
        }
        Ok(())
    }

    /// Close the store, the sole remaining alias of this owner, into the
    /// host's census and end its aliases of the host's factory. A refusal
    /// keeps the owner it was raised on inside the returned error.
    pub fn settle(self: Arc<Self>, host: &StandaloneNativeHost) -> Result<()> {
        let scheduler = Arc::try_unwrap(self).map_err(|scheduler| {
            CarriedOwners::carry(
                anyhow!(
                    "stopped-node scheduler still has {} other aliases",
                    Arc::strong_count(&scheduler) - 1
                ),
                scheduler,
            )
        })?;
        let Self { db, gate } = scheduler;
        drop(gate);
        if let Err(error) = db.close() {
            return Err(CarriedOwners::carry(
                anyhow!("close the stopped-node scheduler store: {error}"),
                db,
            ));
        }
        let terminals = match db.retained_native_terminals() {
            Ok(terminals) => terminals,
            Err(error) => {
                return Err(CarriedOwners::carry(
                    error.context("collect the stopped-node scheduler terminals"),
                    db,
                ));
            }
        };
        if let Err(error) = retain_terminals(host, terminals) {
            return Err(CarriedOwners::carry(error, db));
        }
        let db = Arc::try_unwrap(db).map_err(|db| {
            CarriedOwners::carry(
                anyhow!("stopped-node scheduler store still has other aliases"),
                db,
            )
        })?;
        db.release_settled_into_census().map_err(|(db, error)| {
            CarriedOwners::carry(
                error.context("release the settled stopped-node scheduler store"),
                db,
            )
        })
    }

    #[cfg(test)]
    fn over_store(db: Arc<SchedulerDb>) -> Arc<Self> {
        Arc::new(Self {
            db,
            gate: Arc::new(RwLock::new(())),
        })
    }
}

/// File `terminals` in the host's census; the terminals it refuses ride the
/// returned error.
fn retain_terminals(
    host: &StandaloneNativeHost,
    terminals: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
) -> Result<()> {
    let mut refused = Vec::new();
    let mut reasons = Vec::new();
    for terminal in terminals {
        if let Err((terminal, reason)) = host.retain_terminal(terminal) {
            refused.push(terminal);
            reasons.push(format!("{reason:#}"));
        }
    }
    if reasons.is_empty() {
        return Ok(());
    }
    Err(CarriedOwners::carry(
        anyhow!("stopped-node scheduler census not retained: {}", reasons.join("; ")),
        refused,
    ))
}

/// Close a refused schema reference through its same owner and file its
/// terminal in the host's census. The released reference keeps its factory
/// alias and rides the returned error with the original refusal.
fn settle_refused_reference(
    host: &StandaloneNativeHost,
    factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    refused: ryeos_state::sqlite_schema::NativeSchemaSqlReferenceFailure,
) -> anyhow::Error {
    let original = anyhow!("stopped-node scheduler schema reference refused: {refused}");
    let released = match refused.close_retained() {
        Ok(released) => released,
        Err(refused) => return CarriedOwners::carry(original, refused),
    };
    let error = match released.terminal(factory) {
        Ok(Some(terminal)) => match retain_terminals(host, vec![terminal.clone()]) {
            Ok(()) => original,
            Err(census) => CarriedOwners::carry(original, census),
        },
        Ok(None) => original,
        Err(error) => original.context(format!("{error:#}")),
    };
    CarriedOwners::carry(error, released)
}

/// Close a refused store open through its same owner and file its terminals
/// in the host's census. The failure keeps its store and rides the returned
/// error with the original refusal.
fn settle_refused_open(
    host: &StandaloneNativeHost,
    failure: ryeos_scheduler::db::SchedulerDbOpenFailure,
) -> anyhow::Error {
    let original = anyhow!("stopped-node scheduler store open refused: {failure}");
    let failure = match failure.retry_close() {
        Ok(failure) => failure,
        Err(failure) => {
            let message = format!("refused scheduler store did not close: {failure}");
            return CarriedOwners::carry(original.context(message), failure);
        }
    };
    let error = match failure.retained_native_terminals() {
        Ok(terminals) => match retain_terminals(host, terminals) {
            Ok(()) => original,
            Err(census) => CarriedOwners::carry(original, census),
        },
        Err(error) => original.context(format!("{error:#}")),
    };
    CarriedOwners::carry(error, failure)
}

impl ProjectScheduleDeployParticipant for StoppedNodeScheduler {
    fn acquire_project_schedules(
        &self,
    ) -> ProjectScheduleFuture<'_, Box<dyn ProjectScheduleSession>> {
        Box::pin(async {
            anyhow::bail!("a stopped-node projection rebuild deploys no project schedules")
        })
    }

    fn try_acquire_project_schedules(&self) -> Result<Option<Box<dyn ProjectScheduleSession>>> {
        anyhow::bail!("a stopped-node projection rebuild deploys no project schedules")
    }
}

impl SchedulerParticipant for StoppedNodeScheduler {
    fn observe_terminal(
        &self,
        observation: &DurableThreadTerminal<'_>,
    ) -> Result<TerminalObservationDisposition> {
        anyhow::bail!(
            "a stopped-node projection rebuild records no terminal for thread {}",
            observation.thread_id()
        )
    }

    fn acquire_retention(&self) -> SchedulerFuture<'_, Box<dyn SchedulerRetentionGuard>> {
        Box::pin(async move {
            let gate: OwnedRwLockWriteGuard<()> = self.gate.clone().write_owned().await;
            if !self.db.fire_projection_is_current()? {
                anyhow::bail!(
                    "stopped-node scheduler retention requires a current fire projection before pin inspection"
                );
            }
            Ok(Box::new(StoppedNodeRetention {
                _gate: gate,
                db: Arc::clone(&self.db),
            }) as Box<dyn SchedulerRetentionGuard>)
        })
    }
}

/// Pin and snapshot-root reads over the stopped node's fire projection.
/// Fire retention is a live-node mutation and is refused here.
struct StoppedNodeRetention {
    _gate: OwnedRwLockWriteGuard<()>,
    db: Arc<SchedulerDb>,
}

impl SchedulerRetentionGuard for StoppedNodeRetention {
    fn count_thread_pins(&self, thread_ids: &[String]) -> Result<u64> {
        let mut pins = 0u64;
        for thread_id in thread_ids {
            if self.db.find_fire_by_thread(thread_id)?.is_some() {
                pins = pins
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("scheduler pin count overflow"))?;
            }
        }
        Ok(pins)
    }

    fn operational_snapshot_roots(&self) -> Result<Vec<String>> {
        self.db.operational_snapshot_roots()
    }

    fn prepare_fire_retention(
        &mut self,
        _runtime: &lillux::PinnedDirectory,
        _dry_run: bool,
    ) -> Result<()> {
        anyhow::bail!("a stopped-node projection rebuild retains no scheduler fires")
    }

    fn finish_fire_retention(&mut self, _targets: &[FireRetentionTarget]) -> Result<usize> {
        anyhow::bail!("a stopped-node projection rebuild retains no scheduler fires")
    }

    fn repair_fire_retention(&mut self) -> Result<()> {
        anyhow::bail!("a stopped-node projection rebuild retains no scheduler fires")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_node_retention_serves_pins_and_roots_but_retains_no_fires() {
        let store = ryeos_scheduler::test_native::TestSchedulerDb::open();
        let scheduler = StoppedNodeScheduler::over_store(store.shared());
        let participant: Arc<dyn SchedulerParticipant> = scheduler.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build test runtime");
        let mut guard = runtime
            .block_on(participant.acquire_retention())
            .expect("acquire stopped-node retention");
        assert_eq!(
            guard
                .count_thread_pins(&["thread-without-fire".to_string()])
                .expect("count pins"),
            0
        );
        assert!(guard.operational_snapshot_roots().expect("snapshot roots").is_empty());
        assert!(guard.finish_fire_retention(&[]).is_err());
        assert!(guard.repair_fire_retention().is_err());
        assert!(scheduler.try_acquire_project_schedules().is_err());
        // Every alias of the store ends before the test store settles it.
        drop(guard);
        drop(participant);
        drop(scheduler);
        drop(runtime);
        drop(store);
    }
}
