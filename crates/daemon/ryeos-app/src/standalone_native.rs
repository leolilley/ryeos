//! In-process native host of a standalone service invocation. The caller
//! holds the exclusive node StateLock (the daemon is not running) and issues
//! one process-scoped [`OfflineNativeHost`] funded by the signed maintenance
//! policy's `offline_native` quota for [`STANDALONE_STATE_CLASS`]. The Store
//! opens on that host exactly as a live node opens it and closes into the
//! host's census before the host finishes.
//!
//! Funding is process-local and records no durable debit: the arena is
//! private memory the kernel reclaims when the process ends, and the
//! StateLock excludes the live daemon and every other maintainer. The Store
//! itself takes the runtime namespace lock when it opens.

use anyhow::{Context, Result, ensure};
use lillux::process_control::ResidencyProof;
use lillux_sqlite::offline_host::{
    OfflineNativeAuthority, OfflineNativeFacts, OfflineNativeHost, OfflineNativeReleased,
    OfflineNativeRetained,
};
use lillux_sqlite::{OriginalNativeBudget, OriginalNativeFactory};
use ryeos_node_runtime::config::Config;
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_node_runtime::state_lock::{StateLock, StateLockLease};
use ryeos_scheduler_contract::{NodeMaintenancePolicy, OfflineNativeQuota};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Operation class of a standalone service invocation's node Store.
pub const STANDALONE_STATE_CLASS: &str = "standalone-state";

/// Revalidated on every native boundary of the standalone host.
struct StandaloneAuthority {
    process: u32,
    app_root: PathBuf,
    state_lock: StateLockLease,
    runtime: Arc<lillux::PinnedDirectory>,
    // Held by the caller in the stopped-node maintenance shape; absent when
    // the Store that opens on this host takes the namespace lock itself.
    namespace: Option<lillux::PinnedDirectoryLock>,
    operation: lillux::time::MonotonicDeadline,
    cleanup: lillux::time::MonotonicDeadline,
}

impl OfflineNativeAuthority for StandaloneAuthority {
    fn checkpoint(&self, cleanup: bool) -> Result<()> {
        ensure!(self.process == std::process::id(), "inherited standalone native host refused");
        self.state_lock.ensure_protects_app_root(&self.app_root)?;
        self.runtime.ensure_path_binding()?;
        if let Some(namespace) = &self.namespace {
            namespace.ensure_protects(&self.runtime)?;
        }
        let (deadline, window) = if cleanup {
            (&self.cleanup, "cleanup")
        } else {
            (&self.operation, "operation")
        };
        ensure!(
            !deadline.has_elapsed(),
            "standalone native `{STANDALONE_STATE_CLASS}` {window} window elapsed"
        );
        Ok(())
    }
    fn require_runtime(&self, runtime: &lillux::PinnedDirectory) -> Result<()> {
        ensure!(
            self.runtime.is_same_directory(runtime)?,
            "standalone native host selects a runtime other than this node's"
        );
        Ok(())
    }
    /// The class quota has no SQL dimension: SQL is metered by the raw
    /// budget the host debits right after this call, and the operation
    /// window is enforced at every checkpoint.
    fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
        Ok(())
    }
}

/// Lease authority of the standalone host: admits generation leases of the
/// host's own factory only, within its signed windows.
struct StandaloneLeaseAuthority {
    authority: Arc<StandaloneAuthority>,
    factory: std::sync::Weak<OriginalNativeFactory>,
}

impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for StandaloneLeaseAuthority {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>, cleanup: bool) -> Result<()> {
        self.authority.checkpoint(cleanup)?;
        let issued = self.factory.upgrade().context("standalone native factory absent")?;
        ensure!(Arc::ptr_eq(&issued, factory), "standalone lease selects a foreign factory");
        Ok(())
    }
    fn fork_deadline(&self, factory: &Arc<OriginalNativeFactory>, cleanup: bool)
        -> Result<lillux::time::MonotonicDeadline> {
        lillux_sqlite::OriginalNativeProjectionLeaseAuthority::require_original(self, factory, cleanup)?;
        Ok(if cleanup { self.authority.cleanup } else { self.authority.operation })
    }
}

/// The in-process native host of one standalone service invocation.
pub struct StandaloneNativeHost {
    authority: Arc<StandaloneAuthority>,
    host: OfflineNativeHost,
    // The projection lease authority of this host's factory, held strongly
    // until the host is released; every Store on this host shares it.
    lease: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
    // Generation lease owners of closed Stores, retired against the finished
    // native backend before the host releases.
    retirements: std::cell::RefCell<Vec<ryeos_state::state_db::GenerationLeaseRetirement>>,
}

impl StandaloneNativeHost {
    /// Issue the class's signed envelope under the caller's StateLock, for a
    /// Store that takes the runtime namespace lock itself when it opens.
    ///
    /// `admit_residency` decides the caller's capability floor on the
    /// residency the host's region actually established. A refusal finishes
    /// the host before any connection ticket is issued.
    pub fn issue(
        config: &Config,
        identity: &NodeIdentity,
        state_lock: &StateLock,
        admit_residency: &dyn Fn(&ResidencyProof) -> Result<()>,
    ) -> Result<Self> {
        let runtime_dir = config.runtime_state_dir();
        let runtime = Arc::new(
            lillux::PinnedDirectory::open(&runtime_dir)?
                .with_context(|| format!("runtime state is absent: {}", runtime_dir.display()))?,
        );
        Self::issue_inner(config, identity, state_lock, runtime, None, admit_residency)
    }

    /// Issue the class's signed envelope in the stopped-node maintenance
    /// shape: the caller holds both the StateLock and `namespace`, the
    /// exclusive lock of `runtime`, for the host's whole life, and every
    /// native boundary revalidates both.
    pub fn issue_with_namespace(
        config: &Config,
        identity: &NodeIdentity,
        state_lock: &StateLock,
        runtime: Arc<lillux::PinnedDirectory>,
        namespace: &lillux::PinnedDirectoryLock,
        admit_residency: &dyn Fn(&ResidencyProof) -> Result<()>,
    ) -> Result<Self> {
        namespace.ensure_protects(&runtime)?;
        Self::issue_inner(config, identity, state_lock, runtime, Some(namespace.clone()), admit_residency)
    }

    fn issue_inner(
        config: &Config,
        identity: &NodeIdentity,
        state_lock: &StateLock,
        runtime: Arc<lillux::PinnedDirectory>,
        namespace: Option<lillux::PinnedDirectoryLock>,
        admit_residency: &dyn Fn(&ResidencyProof) -> Result<()>,
    ) -> Result<Self> {
        state_lock.ensure_protects_app_root(&config.app_root)?;
        runtime.ensure_path_binding()?;
        let runtime_dir = config.runtime_state_dir();
        let configured = lillux::PinnedDirectory::open(&runtime_dir)?
            .with_context(|| format!("runtime state is absent: {}", runtime_dir.display()))?;
        ensure!(
            runtime.path() == runtime_dir.as_path() && runtime.is_same_directory(&configured)?,
            "standalone native host runtime differs from this node's runtime state directory {}",
            runtime_dir.display()
        );
        crate::offline_native::require_started_once(&runtime)?;
        let snapshot = crate::offline_native::load_signed_maintenance_snapshot(config, identity)?;
        let quota: OfflineNativeQuota = snapshot
            .require::<NodeMaintenancePolicy>()?
            .offline_native
            .get(STANDALONE_STATE_CLASS)
            .with_context(|| {
                format!("node maintenance policy grants no offline native quota for `{STANDALONE_STATE_CLASS}`")
            })?
            .clone();
        ryeos_node_runtime::node_policy::sections::maintenance::validate_offline_native_quota(&quota)
            .with_context(|| format!("offline native `{STANDALONE_STATE_CLASS}` quota"))?;
        let operation = Duration::from_millis(quota.max_operation_millis);
        let cleanup = operation
            .checked_add(Duration::from_millis(quota.max_cleanup_millis))
            .context("standalone native window overflow")?;
        let authority = Arc::new(StandaloneAuthority {
            process: std::process::id(),
            app_root: config.app_root.clone(),
            state_lock: state_lock.retain(),
            runtime,
            namespace,
            operation: lillux::time::MonotonicDeadline::after(operation),
            cleanup: lillux::time::MonotonicDeadline::after(cleanup),
        });
        authority.checkpoint(false)?;
        // SAFETY: this envelope comes from the node-signed maintenance policy
        // for the standalone class, reserved once for this process while the
        // caller holds the exclusive node StateLock; the authority
        // revalidates it before every native effect.
        let raw = unsafe {
            OriginalNativeBudget::prepaid_by_original_issuer(
                quota.max_native_storage_bytes,
                quota.max_native_allocator_steps,
                quota.max_native_connections,
                quota.max_cleanup_steps,
            )
        }
        .map_err(|refusal| anyhow::anyhow!("standalone native funding refused: {refusal}"))?;
        let host_authority: Arc<dyn OfflineNativeAuthority> = authority.clone();
        // SAFETY: the caller's StateLock excludes the live daemon and every
        // other maintainer of the node's databases for this process, `raw`
        // was minted once above for this host, and the authority revalidates
        // the lock and the class windows on every checkpoint. The Store that
        // opens on this host takes the runtime namespace lock itself.
        match unsafe { OfflineNativeHost::issue(raw, host_authority) } {
            Ok(host) => {
                // The plan the floor was admitted on can be stronger than the
                // basis the region established; with no proof the floor
                // cannot be decided. Either refusal finishes the host while
                // it has issued no connection ticket.
                let admitted = host
                    .residency_proof()
                    .context("standalone native host established no residency proof")
                    .and_then(|proof| admit_residency(&proof));
                if let Err(error) = admitted {
                    return Err(match host.finish() {
                        Ok(_) => error,
                        Err(retained) => anyhow::Error::new(retained)
                            .context(format!("standalone native host refused on its residency: {error:#}")),
                    });
                }
                let lease: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> =
                    match host.factory() {
                        Ok(factory) => Arc::new(StandaloneLeaseAuthority {
                            authority: Arc::clone(&authority),
                            factory: Arc::downgrade(factory),
                        }),
                        Err(error) => {
                            return Err(match host.finish() {
                                Ok(_) => error.context("standalone native host has no factory"),
                                Err(retained) => anyhow::Error::new(retained)
                                    .context(format!("standalone native host has no factory: {error:#}")),
                            });
                        }
                    };
                Ok(Self { authority, host, lease, retirements: std::cell::RefCell::new(Vec::new()) })
            }
            Err(failure) => match failure.release() {
                Ok(error) => Err(error.context("issue standalone native host")),
                Err(retained) => Err(anyhow::Error::new(retained)),
            },
        }
    }

    pub fn factory(&self) -> Result<&Arc<OriginalNativeFactory>> {
        self.host.factory()
    }

    pub fn destination(&self) -> Result<Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>> {
        self.host.destination()
    }

    /// Record one terminal in this host's census; a refusal hands it back.
    pub fn retain_terminal(
        &self,
        terminal: lillux_sqlite::OriginalNativeConnectionTerminal,
    ) -> std::result::Result<(), (lillux_sqlite::OriginalNativeConnectionTerminal, anyhow::Error)> {
        self.host.retain_terminal(terminal)
    }

    /// Open the node Store for an explicit projection rebuild on this host,
    /// in the stopped-node maintenance shape: the host must have been issued
    /// by [`Self::issue_with_namespace`] with this same `runtime` and
    /// `namespace`. Close it with [`Self::close_control_store`].
    pub fn open_rebuild_store(
        &self,
        config: &Config,
        runtime: &lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        signer: Arc<dyn ryeos_state::signer::Signer>,
        write_barrier: crate::write_barrier::WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
    ) -> Result<crate::state_store::StateStore> {
        ensure!(
            self.authority.namespace.is_some() && self.authority.runtime.is_same_directory(runtime)?,
            "projection rebuild needs a host issued under this runtime namespace lock"
        );
        let factory = Arc::clone(self.factory()?);
        let retain = |terminal: lillux_sqlite::OriginalNativeConnectionTerminal| self.host.retain_terminal(terminal);
        let retain_lease = |lease: Arc<lillux_sqlite::ClosedOriginalProjectionLease>| self.host.retain_lease(lease);
        let retire_after = |retirement: ryeos_state::state_db::GenerationLeaseRetirement| {
            self.retirements.borrow_mut().push(retirement)
        };
        // SAFETY: the caller holds the StateLock and `namespace` for the
        // Store's whole life (checked on every boundary by this host's
        // authority); the factory, destination and lease authority are this
        // host's own; every terminal and lease reaches this host's census.
        unsafe {
            crate::state_store::StateStore::new_for_projection_rebuild_native(
                config.app_root.clone(),
                runtime.try_clone()?,
                namespace.clone(),
                config.db_path.clone(),
                signer,
                write_barrier,
                head_trust,
                &factory,
                self.destination()?,
                self.lease_authority()?,
                self.operation_deadline(),
                &retain,
                &retain_lease,
                &retire_after,
            )
        }
    }

    /// Open the node Store for strict projection verification on this host,
    /// in the stopped-node maintenance shape: the host must have been issued
    /// by [`Self::issue_with_namespace`] with this same `runtime` and
    /// `namespace`. Close it with [`Self::close_control_store`].
    pub fn open_verification_store(
        &self,
        config: &Config,
        runtime: &lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        signer: Arc<dyn ryeos_state::signer::Signer>,
        write_barrier: crate::write_barrier::WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
    ) -> Result<crate::state_store::StateStore> {
        ensure!(
            self.authority.namespace.is_some() && self.authority.runtime.is_same_directory(runtime)?,
            "projection verification needs a host issued under this runtime namespace lock"
        );
        let factory = Arc::clone(self.factory()?);
        let retain = |terminal: lillux_sqlite::OriginalNativeConnectionTerminal| self.host.retain_terminal(terminal);
        let retain_lease = |lease: Arc<lillux_sqlite::ClosedOriginalProjectionLease>| self.host.retain_lease(lease);
        let retire_after = |retirement: ryeos_state::state_db::GenerationLeaseRetirement| {
            self.retirements.borrow_mut().push(retirement)
        };
        // SAFETY: as in `open_rebuild_store`; the lease authority admits only
        // this host's factory.
        unsafe {
            crate::state_store::StateStore::new_for_projection_verification_native(
                config.app_root.clone(),
                runtime.try_clone()?,
                namespace.clone(),
                config.db_path.clone(),
                signer,
                write_barrier,
                head_trust,
                &factory,
                self.destination()?,
                self.lease_authority()?,
                self.operation_deadline(),
                &retain,
                &retain_lease,
                &retire_after,
            )
        }
    }

    /// Close `store`, the sole remaining alias of a Store opened by
    /// [`Self::open_rebuild_store`] or [`Self::open_verification_store`], into
    /// this host's census.
    pub fn close_control_store(&self, store: Arc<crate::state_store::StateStore>) -> Result<()> {
        let factory = Arc::clone(self.factory()?);
        let retain = |terminal: lillux_sqlite::OriginalNativeConnectionTerminal| self.host.retain_terminal(terminal);
        let retain_lease = |lease: Arc<lillux_sqlite::ClosedOriginalProjectionLease>| self.host.retain_lease(lease);
        let retire_after = |retirement: ryeos_state::state_db::GenerationLeaseRetirement| {
            self.retirements.borrow_mut().push(retirement)
        };
        store.close_offline_native_control(&factory, &retain, &retain_lease, &retire_after)
    }

    /// The lease authority for a Store opened on this host: it admits only
    /// this host's factory, within the class windows.
    pub fn lease_authority(&self) -> Result<Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>> {
        Ok(Arc::clone(&self.lease))
    }

    /// The end of this host's signed operation window.
    pub fn operation_deadline(&self) -> lillux::time::MonotonicDeadline {
        self.authority.operation
    }

    /// Open the node Store on this host the way a live node opens it.
    pub fn open_store(
        &self,
        config: &Config,
        state_lock: &StateLock,
        signer: Arc<dyn ryeos_state::signer::Signer>,
        write_barrier: crate::write_barrier::WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
    ) -> Result<crate::state_store::StateStore> {
        // SAFETY: this host is the sole issuer of its factory and is
        // finished by its owner after the Store closes into its census; the
        // lease authority admits only that factory; the StateLock of this
        // exact App root is held by the caller for the Store's whole life.
        unsafe {
            crate::state_store::StateStore::new_in_process_native(
                config.app_root.clone(),
                config.runtime_state_dir(),
                config.db_path.clone(),
                signer,
                write_barrier,
                head_trust,
                Arc::new(state_lock.retain()),
                Arc::clone(self.factory()?),
                self.lease_authority()?,
                self.operation_deadline(),
            )
        }
    }

    /// Close `store`, the sole remaining alias of a Store opened by
    /// [`Self::open_store`], into this host's census. A refused close or an
    /// unrecorded terminal keeps its owners inside the returned error.
    pub fn close_store(&self, store: Arc<crate::state_store::StateStore>) -> Result<()> {
        let census = store
            .close_in_process_census(Arc::clone(self.factory()?))
            .map_err(|failure| {
                let error = anyhow::anyhow!("close standalone Store: {failure:#}");
                ryeos_state::operational::CarriedOwners::carry(error, failure)
            })?;
        self.retain_census(census)
    }

    /// Settle a refused [`Self::open_store`] into this host's census and
    /// return the refusal. A refusal raised before any native owner existed
    /// carries none.
    pub fn settle_refused_open(&self, error: anyhow::Error) -> anyhow::Error {
        let message = format!("{error:#}");
        let failure = match error.downcast::<crate::state_store::LiveStateStoreOpenFailure>() {
            Ok(failure) => failure,
            Err(error) => return error,
        };
        let original = anyhow::anyhow!("standalone Store open refused: {message}");
        let released = match failure.close_retained() {
            Ok(released) => released,
            Err(failure) => return ryeos_state::operational::CarriedOwners::carry(original, failure),
        };
        let factory = match self.factory() {
            Ok(factory) => Arc::clone(factory),
            Err(error) => {
                return ryeos_state::operational::CarriedOwners::carry(
                    original.context(format!("standalone host factory absent: {error:#}")),
                    released,
                );
            }
        };
        match released.into_census(factory) {
            Ok(census) => match self.retain_census(census) {
                Ok(()) => original,
                Err(error) => original.context(format!("{error:#}")),
            },
            Err(failure) => ryeos_state::operational::CarriedOwners::carry(original, failure),
        }
    }

    fn retain_census(&self, census: crate::state_store::StateStoreCensus) -> Result<()> {
        let crate::state_store::StateStoreCensus { terminals, leases, lease_retirements, prior_refusal } = census;
        self.retirements.borrow_mut().extend(lease_retirements);
        let mut refused_terminals = Vec::new();
        let mut refused_leases = Vec::new();
        let mut reasons = Vec::new();
        for terminal in terminals {
            if let Err((terminal, reason)) = self.host.retain_terminal(terminal) {
                refused_terminals.push(terminal);
                reasons.push(format!("{reason:#}"));
            }
        }
        for lease in leases {
            if let Err((lease, reason)) = self.host.retain_lease(lease) {
                refused_leases.push(lease);
                reasons.push(format!("{reason:#}"));
            }
        }
        if !reasons.is_empty() {
            return Err(ryeos_state::operational::CarriedOwners::carry(
                anyhow::anyhow!("standalone Store census not retained: {}", reasons.join("; ")),
                (refused_terminals, refused_leases),
            ));
        }
        match prior_refusal {
            Some(error) => Err(error.context("standalone Store close recorded an earlier refusal")),
            None => Ok(()),
        }
    }

    /// Finish the host and return the facts its release captured. A refusal
    /// keeps the host for retry.
    pub fn finish(self) -> std::result::Result<OfflineNativeFacts, (Option<Self>, anyhow::Error)> {
        let Self { authority, mut host, lease, retirements } = self;
        // Closed generation leases retire against the finished backend, then
        // the host releases; a refusal keeps every owner for retry.
        let retired = host.finish_backend().and_then(|backend| {
            for retirement in retirements.borrow().iter() {
                retirement.retire_after_backend(&backend)?;
            }
            Ok(())
        });
        if let Err(error) = retired {
            return Err((Some(Self { authority, host, lease, retirements }), error));
        }
        match host.finish() {
            Ok(OfflineNativeReleased { facts, region: _, durability_pins: _ }) => {
                // The lease authority ends only once the host has released.
                drop(lease);
                facts.ok_or_else(|| {
                    (None, anyhow::anyhow!("standalone native host released without backend facts"))
                })
            }
            Err(OfflineNativeRetained { host, error }) => {
                Err((Some(Self { authority, host, lease, retirements }), error))
            }
        }
    }
}
