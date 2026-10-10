use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context as _, Result};
use ryeos_scheduler::SchedulerDb;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

/// The one selected scheduler owner shared by daemon lifecycle, API and
/// maintenance composition. Its DB and gate are always the same instances.
pub struct SchedulerService {
    pub(crate) app_root: PathBuf,
    pub(crate) runtime_directory: Arc<lillux::PinnedDirectory>,
    pub(crate) runtime_identity: lillux::PinnedDirectoryIdentity,
    pub(crate) db: Arc<SchedulerDb>,
    pub(crate) runtime_gate: Arc<RwLock<()>>,
    pub(crate) identity: Arc<ryeos_node_runtime::identity::NodeIdentity>,
    pub(crate) trust_store: Arc<ryeos_engine::trust::TrustStore>,
    pub(crate) reload_tx:
        std::sync::RwLock<Option<tokio::sync::mpsc::Sender<ryeos_scheduler::ReloadSignal>>>,
    pub(crate) project_recovery_pending: Arc<AtomicBool>,
    closed_for_finalization: AtomicBool,
}

impl SchedulerService {
    pub fn new(
        app_root: PathBuf,
        runtime_directory: Arc<lillux::PinnedDirectory>,
        db: Arc<SchedulerDb>,
        runtime_gate: Arc<RwLock<()>>,
        identity: Arc<ryeos_node_runtime::identity::NodeIdentity>,
        trust_store: Arc<ryeos_engine::trust::TrustStore>,
    ) -> Result<Self> {
        let expected_runtime = app_root.join(ryeos_engine::AI_DIR).join("state");
        let expected_runtime = lillux::PinnedDirectory::open(&expected_runtime)?
            .ok_or_else(|| anyhow::anyhow!("scheduler runtime directory is absent"))?;
        if !runtime_directory.is_same_directory(&expected_runtime)? {
            anyhow::bail!("scheduler DB/gate runtime root does not match the configured app root");
        }
        db.require_runtime(&runtime_directory)
            .context("scheduler DB is not bound to the selected runtime directory")?;
        let runtime_identity = runtime_directory.identity()?;
        Ok(Self {
            app_root,
            runtime_directory,
            runtime_identity,
            db,
            runtime_gate,
            identity,
            trust_store,
            reload_tx: std::sync::RwLock::new(None),
            project_recovery_pending: Arc::new(AtomicBool::new(false)),
            closed_for_finalization: AtomicBool::new(false),
        })
    }

    pub fn install_reload_sender(
        &self,
        sender: tokio::sync::mpsc::Sender<ryeos_scheduler::ReloadSignal>,
    ) -> Result<()> {
        let mut slot = self
            .reload_tx
            .write()
            .map_err(|_| anyhow::anyhow!("scheduler reload sender lock was poisoned"))?;
        if slot.is_some() {
            anyhow::bail!("scheduler reload sender already installed");
        }
        *slot = Some(sender);
        Ok(())
    }

    /// Close the selected database only after daemon tasks and admitted work
    /// have drained. The write gate excludes every remaining service operation;
    /// a failed close remains owned by this service's exact DB instance.
    pub fn close_after_shutdown_drain(&self) -> Result<()> {
        if self.closed_for_finalization.load(Ordering::Acquire) {
            return Ok(());
        }
        // Successful task drain must leave this exact gate immediately free.
        // Contention is retained unresolved work, not a new waiting interval.
        let _guard = self.runtime_gate.clone().try_write_owned()
            .map_err(|_| anyhow::anyhow!("scheduler shutdown gate remains occupied after drain"))?;
        self.db
            .close()
            .map_err(anyhow::Error::new)
            .context("close selected scheduler database after shutdown drain")?;
        self.closed_for_finalization.store(true, Ordering::Release);
        Ok(())
    }

    /// Transfer the exact closed scheduler DB out of its service only after
    /// the service itself and the DB have no aliases. A refusal returns a
    /// complete replacement service owner, preserving the closed DB and all
    /// remaining service state for the caller to restore into AppState.
    pub fn into_native_finalization_owner(
        self: Arc<Self>,
    ) -> std::result::Result<ryeos_scheduler::db::SchedulerNativeFinalizationOwner, (Arc<Self>, anyhow::Error)> {
        if !self.closed_for_finalization.load(Ordering::Acquire) {
            return Err((self, anyhow::anyhow!("scheduler service has not completed its shutdown close")));
        }
        let service = match Arc::try_unwrap(self) {
            Ok(service) => service,
            Err(service) => return Err((service, anyhow::anyhow!("scheduler service still has active aliases"))),
        };
        let Self {
            app_root,
            runtime_directory,
            runtime_identity,
            db,
            runtime_gate,
            identity,
            trust_store,
            reload_tx,
            project_recovery_pending,
            closed_for_finalization,
        } = service;
        let db = match Arc::try_unwrap(db) {
            Ok(db) => db,
            Err(db) => return Err((Arc::new(Self {
                app_root, runtime_directory, runtime_identity, db, runtime_gate,
                identity, trust_store, reload_tx, project_recovery_pending,
                closed_for_finalization,
            }), anyhow::anyhow!("scheduler database still has active aliases"))),
        };
        match db.into_native_finalization() {
            Ok(owner) => Ok(owner),
            Err((db, error)) => Err((Arc::new(Self {
                app_root, runtime_directory, runtime_identity, db: Arc::new(db), runtime_gate,
                identity, trust_store, reload_tx, project_recovery_pending,
                closed_for_finalization,
            }), error)),
        }
    }

    pub fn notify_reload(&self, schedule_id: Option<String>) {
        let sender = self.reload_tx.read().ok().and_then(|slot| slot.clone());
        if let Some(sender) = sender
            && let Err(error) = sender.try_send(ryeos_scheduler::ReloadSignal { schedule_id })
        {
            tracing::warn!(%error, "scheduler reload notification was not queued; timer repair will reconcile state");
        }
    }

    pub fn require_dispatch_available(&self) -> Result<()> {
        require_recovery_clear(&self.project_recovery_pending)
    }

    /// Acquire the shared read gate and recheck recovery after waiting. A
    /// caller holds the returned guard across its complete read operation.
    pub async fn acquire_read(&self) -> Result<OwnedRwLockReadGuard<()>> {
        acquire_checked_read_gate(
            self.runtime_gate.clone(),
            self.project_recovery_pending.clone(),
        )
        .await
    }

    /// Acquire the shared mutation gate and recheck recovery after waiting.
    /// This closes the race where a project transaction fails while this call
    /// is queued behind its write guard.
    pub async fn acquire_mutation(&self) -> Result<OwnedRwLockWriteGuard<()>> {
        acquire_checked_write_gate(
            self.runtime_gate.clone(),
            self.project_recovery_pending.clone(),
            || {},
        )
        .await
    }

    pub fn scheduler_db(&self) -> Arc<SchedulerDb> {
        self.db.clone()
    }

    pub fn app_root(&self) -> &std::path::Path {
        &self.app_root
    }

    pub fn trust_store(&self) -> &ryeos_engine::trust::TrustStore {
        &self.trust_store
    }

    pub fn identity(&self) -> &ryeos_node_runtime::identity::NodeIdentity {
        &self.identity
    }

    pub fn runtime_gate(&self) -> Arc<RwLock<()>> {
        self.runtime_gate.clone()
    }
}

fn require_recovery_clear(pending: &AtomicBool) -> Result<()> {
    if pending.load(Ordering::Acquire) {
        anyhow::bail!("scheduler dispatch is blocked by unresolved project schedule recovery");
    }
    Ok(())
}

async fn acquire_checked_read_gate(
    gate: Arc<RwLock<()>>,
    pending: Arc<AtomicBool>,
) -> Result<OwnedRwLockReadGuard<()>> {
    require_recovery_clear(&pending)?;
    let guard = gate.read_owned().await;
    require_recovery_clear(&pending)?;
    Ok(guard)
}

async fn acquire_checked_write_gate(
    gate: Arc<RwLock<()>>,
    pending: Arc<AtomicBool>,
    before_wait: impl FnOnce(),
) -> Result<OwnedRwLockWriteGuard<()>> {
    require_recovery_clear(&pending)?;
    before_wait();
    let guard = gate.write_owned().await;
    require_recovery_clear(&pending)?;
    Ok(guard)
}

pub(crate) struct SchedulerServiceRetention {
    _runtime_gate: OwnedRwLockWriteGuard<()>,
    pub(crate) db: Arc<SchedulerDb>,
    pub(crate) runtime_directory: Arc<lillux::PinnedDirectory>,
    pub(crate) runtime_identity: lillux::PinnedDirectoryIdentity,
    pub(crate) prepared: bool,
    pub(crate) projection_invalidated: bool,
    pub(crate) dry_run: Option<bool>,
    pub(crate) finished: bool,
}

impl SchedulerServiceRetention {
    pub(crate) fn new(
        runtime_gate: OwnedRwLockWriteGuard<()>,
        db: Arc<SchedulerDb>,
        runtime_directory: Arc<lillux::PinnedDirectory>,
        runtime_identity: lillux::PinnedDirectoryIdentity,
    ) -> Self {
        Self {
            _runtime_gate: runtime_gate,
            db,
            runtime_directory,
            runtime_identity,
            prepared: false,
            projection_invalidated: false,
            dry_run: None,
            finished: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn queued_mutation_rechecks_recovery_after_acquiring_gate() {
        let gate = Arc::new(RwLock::new(()));
        let pending = Arc::new(AtomicBool::new(false));
        let project_guard = gate.clone().write_owned().await;
        let queued_gate = gate.clone();
        let queued_pending = pending.clone();
        let mutation_attempted = Arc::new(AtomicBool::new(false));
        let queued_attempted = mutation_attempted.clone();
        let (checked_tx, checked_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _guard = acquire_checked_write_gate(queued_gate, queued_pending, || {
                let _ = checked_tx.send(());
            })
            .await?;
            queued_attempted.store(true, Ordering::Release);
            Ok::<(), anyhow::Error>(())
        });

        checked_rx.await.unwrap();
        pending.store(true, Ordering::Release);
        drop(project_guard);

        assert!(waiter.await.unwrap().is_err());
        assert!(!mutation_attempted.load(Ordering::Acquire));
    }
}
