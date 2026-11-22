//! One original native lock owner for an exact generation sidecar. Shared
//! references retain this owner; they never open another native lock or ticket.
use anyhow::{Context, Result};
use lillux::custody::Held;
use lillux_sqlite::{
    ClosedOriginalProjectionLease, OriginalNativeFactory, OriginalNativeProjectionLeaseAuthority,
    OriginalProjectionLease, OriginalProjectionLeaseDisposition, OriginalProjectionLeaseFailure,
    OriginalProjectionLeaseInputs, OriginalProjectionLeaseMode,
};
use std::{
    ffi::OsString,
    sync::{Arc, Mutex, Weak},
};

enum LeaseCustody {
    Prepared(OriginalProjectionLeaseInputs),
    Live(OriginalProjectionLease),
    Refused(OriginalProjectionLeaseFailure),
    Terminal(OriginalProjectionLeaseDisposition),
    Checkout,
    Retired,
}
const GENERATION_LEASE_KIND: &str = "ryeos-state::generation-lease";

/// Everything the lease owns, in the order its release drops it. Constructible
/// only in this module, so no caller can swap the whole out from under its
/// custody.
struct LeaseParts {
    // Affinity survives as a weak coordinate after native holder retirement;
    // the outside metadata owner must not keep resident storage alive.
    factory: Weak<OriginalNativeFactory>,
    root: lillux::PinnedDirectory,
    name: OsString,
    custody: Mutex<LeaseCustody>,
}

/// Settled exactly when the lease is retired: every other phase holds prepared
/// inputs, a live native lock, a refusal, a closed disposition or an effect
/// checkout, so the whole owner, its pinned root included, stays retained until
/// the process exits.
fn lease_parts_retired(parts: &LeaseParts) -> bool {
    // A held lock means a step is mid-flight, so the lease is not retired. The
    // predicate never blocks: it may run during an unwind.
    match parts.custody.try_lock() {
        Ok(custody) => matches!(*custody, LeaseCustody::Retired),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            matches!(*poisoned.into_inner(), LeaseCustody::Retired)
        }
        Err(std::sync::TryLockError::WouldBlock) => false,
    }
}

/// State owns coordinate meaning; Lillux owns the actual paid lock effects.
pub(crate) struct OriginalGenerationLease {
    inner: Held<LeaseParts>,
    identity: (u64, u64),
}
impl OriginalGenerationLease {
    fn from_parts(parts: LeaseParts, identity: (u64, u64)) -> Self {
        Self {
            inner: Held::new(GENERATION_LEASE_KIND, parts, lease_parts_retired),
            identity,
        }
    }
    /// # Safety
    /// The actual original issuer must have funded this complete host/native
    /// owner and retained generation envelope before metadata/input allocation.
    /// Keep the returned owner in the original State recovery registry BEFORE
    /// acquisition; retain authority and namespace through final settlement.
    pub(crate) unsafe fn prepare(
        factory: Arc<OriginalNativeFactory>,
        root: lillux::PinnedDirectory,
        name: OsString,
        authority: Arc<dyn OriginalNativeProjectionLeaseAuthority>,
    ) -> Result<Arc<Self>> {
        factory.require_original_process()?;
        authority.require_original(&factory, false)?;
        root.ensure_path_binding()?;
        let metadata = root
            .open_mount_entry(&name)?
            .context("original generation lease metadata absent")?;
        root.ensure_open_regular_same_mount(&metadata)?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            let observed = metadata.metadata()?;
            anyhow::ensure!(
                observed.is_file(),
                "original generation lease is not regular"
            );
            (observed.dev(), observed.ino())
        };
        #[cfg(not(unix))]
        anyhow::bail!("original generation lease identity unavailable");
        let inputs_root = Arc::new(root.try_clone()?);
        // SAFETY: the trusted caller supplies original State issuer custody,
        // funding and clocks; this exact sidecar was observed through O_PATH.
        let inputs = unsafe {
            OriginalProjectionLeaseInputs::issued(
                inputs_root,
                metadata,
                OriginalProjectionLeaseMode::Shared,
                authority,
            )
        };
        Ok(Arc::new(Self::from_parts(
            LeaseParts {
                factory: Arc::downgrade(&factory),
                root,
                name,
                custody: Mutex::new(LeaseCustody::Prepared(inputs)),
            },
            identity,
        )))
    }
    /// Consume only the genuinely retired owner. This moves the actual root;
    /// it performs no close and never consults a possibly released native arena.
    pub(crate) fn try_into_metadata_root(
        self: Arc<Self>,
    ) -> std::result::Result<lillux::PinnedDirectory, Arc<Self>> {
        if self.inner.is_foreign() {
            return Err(self);
        }
        let retired = match self.inner.custody.try_lock() {
            Ok(custody) => matches!(&*custody, LeaseCustody::Retired),
            Err(_) => false,
        };
        if !retired {
            return Err(self);
        }
        match Arc::try_unwrap(self) {
            Ok(owner) => {
                // Only an empty Retired phase can reach this unique transfer.
                // The actual root moves out; no close is performed and a
                // possibly released native arena is never consulted.
                let Self { inner, identity } = owner;
                match inner.take_whole() {
                    Ok(LeaseParts {
                        factory,
                        root,
                        name,
                        custody,
                    }) => {
                        drop(factory);
                        drop(name);
                        drop(custody);
                        Ok(root)
                    }
                    Err(inner) => Err(Arc::new(Self { inner, identity })),
                }
            }
            Err(owner) => Err(owner),
        }
    }
    fn require_coordinate(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        factory.require_original_process()?;
        anyhow::ensure!(
            Weak::ptr_eq(&Arc::downgrade(factory), &self.inner.factory),
            "foreign original generation lease factory"
        );
        self.inner.root.ensure_path_binding()?;
        let current = self
            .inner
            .root
            .entry_no_follow(&self.inner.name)?
            .context("original generation lease sidecar absent")?;
        anyhow::ensure!(
            current.entry_type == lillux::PinnedEntryType::Regular
                && (current.containing_device, current.inode) == self.identity,
            "original generation lease sidecar changed"
        );
        Ok(())
    }
    pub(crate) fn retire_after_native_terminal(
        &self,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<()> {
        let factory = self.factory()?;
        terminal.require_original(&factory)?;
        let retired = {
            let mut custody = self
                .inner
                .custody
                .try_lock()
                .map_err(|_| anyhow::anyhow!("generation lease retirement custody busy"))?;
            anyhow::ensure!(
                matches!(&*custody, LeaseCustody::Terminal(_) | LeaseCustody::Retired),
                "generation lease retirement has no actual physical close disposition"
            );
            std::mem::replace(&mut *custody, LeaseCustody::Retired)
        };
        // Dispose the same genuine terminal holders outside the mutex only
        // after aggregate issuer ACK. The actual coordinate/root remains with
        // its outside owner; weak affinity cannot retain resident factory data.
        drop(retired);
        Ok(())
    }
    /// Whether this lease is retired. Every other phase still holds a native
    /// owner, refusal or closed disposition, and each of those holds the
    /// factory. The custody is never dropped with the lease, so an unretired
    /// lease keeps that factory alias for the life of the process.
    pub(crate) fn is_retired(&self) -> Result<bool> {
        let custody = self
            .inner
            .custody
            .try_lock()
            .map_err(|_| anyhow::anyhow!("generation lease custody busy"))?;
        Ok(matches!(&*custody, LeaseCustody::Retired))
    }
    pub(crate) fn metadata(&self) -> Result<std::fs::Metadata> {
        let factory = self.factory()?;
        self.require_coordinate(&factory)?;
        self.inner
            .root
            .open_mount_entry(&self.inner.name)?
            .context("original lease metadata absent")?
            .metadata()
            .map_err(anyhow::Error::from)
    }
    pub(crate) fn factory(&self) -> Result<Arc<OriginalNativeFactory>> {
        let factory = self
            .inner
            .factory
            .upgrade()
            .context("original generation factory retired")?;
        factory.require_original_process()?;
        Ok(factory)
    }
    pub(crate) fn require_active(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        self.require_coordinate(factory)?;
        let custody = self
            .inner
            .custody
            .try_lock()
            .map_err(|_| anyhow::anyhow!("generation lease custody busy"))?;
        anyhow::ensure!(
            matches!(&*custody, LeaseCustody::Live(_)),
            "original generation lease is not active"
        );
        Ok(())
    }
    pub(crate) fn acquire(&self) -> Result<()> {
        let factory = self.factory()?;
        self.require_coordinate(&factory)?;
        let checkout = {
            let mut custody = self
                .inner
                .custody
                .try_lock()
                .map_err(|_| anyhow::anyhow!("generation lease custody busy"))?;
            anyhow::ensure!(
                matches!(&*custody, LeaseCustody::Prepared(_)),
                "original generation lease admission already attempted"
            );
            std::mem::replace(&mut *custody, LeaseCustody::Checkout)
        };
        // No native effect is performed while the retention mutex is held.
        let LeaseCustody::Prepared(inputs) = checkout else {
            unreachable!("checked lease preparation")
        };
        let next = match factory.acquire_projection_lease(inputs) {
            Ok(owner) => LeaseCustody::Live(owner),
            Err(failure) => LeaseCustody::Refused(failure),
        };
        let succeeded = matches!(&next, LeaseCustody::Live(_));
        // Restore even if an earlier observation poisoned bookkeeping; the
        // original native owner cannot be discarded by a retry refusal.
        *self
            .inner
            .custody
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        anyhow::ensure!(
            succeeded,
            "original native generation lease acquisition retained"
        );
        self.require_active(&factory)
    }
    /// # Safety
    /// Every actual generation consumer has quiesced under its real State/CAS
    /// owner and native SQL terminals. A reference count is insufficient. The
    /// original caller keeps the registry, roots and authority on each refusal.
    pub(crate) unsafe fn close_after_consumers(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<Option<Arc<ClosedOriginalProjectionLease>>> {
        self.require_coordinate(factory)?;
        let checkout = {
            let mut custody = self
                .inner
                .custody
                .try_lock()
                .map_err(|_| anyhow::anyhow!("generation lease custody busy"))?;
            anyhow::ensure!(
                !matches!(&*custody, LeaseCustody::Checkout),
                "generation lease effect checkout requires original recovery"
            );
            std::mem::replace(&mut *custody, LeaseCustody::Checkout)
        };
        let next = match checkout {
            LeaseCustody::Prepared(inputs) => {
                match factory.release_unacquired_projection_lease_inputs(inputs) {
                    Ok(receipt) => LeaseCustody::Terminal(receipt),
                    Err(failure) => LeaseCustody::Refused(failure),
                }
            }
            LeaseCustody::Live(owner) => match owner.close() {
                Ok(receipt) => LeaseCustody::Terminal(receipt),
                Err(failure) => LeaseCustody::Refused(failure),
            },
            LeaseCustody::Refused(mut failure) => {
                if failure.retained_admission().is_some()
                    && failure.recover_retained_admission().is_err()
                {
                    // The lower failure retains the same producer and exact
                    // refusal/panic evidence before returning this status.
                    LeaseCustody::Refused(failure)
                } else {
                    match failure.into_owner() {
                        Ok(owner) => match owner.close() {
                            Ok(receipt) => LeaseCustody::Terminal(receipt),
                            Err(failure) => LeaseCustody::Refused(failure),
                        },
                        Err(failure) => LeaseCustody::Refused(failure),
                    }
                }
            }
            terminal @ LeaseCustody::Terminal(_) => terminal,
            other => other,
        };
        let receipt = {
            let mut custody = self
                .inner
                .custody
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *custody = next;
            match &*custody {
                LeaseCustody::Terminal(OriginalProjectionLeaseDisposition::Closed(receipt)) => {
                    Some(Arc::clone(receipt))
                }
                LeaseCustody::Terminal(OriginalProjectionLeaseDisposition::NotAcquired(_)) => None,
                _ => anyhow::bail!("original generation lease close retained for retry"),
            }
        };
        if let Some(receipt) = &receipt {
            receipt.require_original(factory)?;
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const KIND: &str = "ryeos-state::generation-lease";

    fn lease(custody: LeaseCustody) -> OriginalGenerationLease {
        let tmp = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        OriginalGenerationLease::from_parts(
            LeaseParts {
                factory: Weak::new(),
                root,
                name: OsString::from("generated.sqlite3.lease"),
                custody: Mutex::new(custody),
            },
            (0, 0),
        )
    }

    #[test]
    fn dropped_retired_lease_releases_normally() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(lease(LeaseCustody::Retired));
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn dropped_unretired_lease_is_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let before = lillux::custody::abandoned_custody().count(KIND);
        // An effect checkout is the lightest unretired phase: it holds no
        // native owner itself, and the retention rule does not depend on one.
        drop(lease(LeaseCustody::Checkout));
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
    }

    #[test]
    fn only_a_retired_lease_transfers_its_root() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let checkout = Arc::new(lease(LeaseCustody::Checkout));
        let before = lillux::custody::abandoned_custody().count(KIND);
        let checkout = checkout
            .try_into_metadata_root()
            .expect_err("an unretired lease is refused");
        drop(checkout);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
        let root = Arc::new(lease(LeaseCustody::Retired))
            .try_into_metadata_root()
            .ok()
            .expect("a retired lease transfers its root");
        drop(root);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use lillux_sqlite::offline_host::{OfflineNativeAuthority, OfflineNativeRetained};
    use lillux_sqlite::test_host::{TestNativeEnvelope, issue_offline_host};

    /// Host authority whose windows always stay open.
    struct OpenWindows;
    impl OfflineNativeAuthority for OpenWindows {
        fn checkpoint(&self, _cleanup: bool) -> Result<()> {
            Ok(())
        }
        fn require_runtime(&self, _runtime: &lillux::PinnedDirectory) -> Result<()> {
            Ok(())
        }
        fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
            Ok(())
        }
    }

    /// Lease authority that admits every lease for the test's own host.
    struct AdmittedLease;
    impl OriginalNativeProjectionLeaseAuthority for AdmittedLease {
        fn require_original(
            &self,
            _factory: &Arc<OriginalNativeFactory>,
            _cleanup: bool,
        ) -> Result<()> {
            Ok(())
        }
        fn fork_deadline(
            &self,
            _factory: &Arc<OriginalNativeFactory>,
            _cleanup: bool,
        ) -> Result<lillux::time::MonotonicDeadline> {
            Ok(lillux::time::MonotonicDeadline::after(
                std::time::Duration::from_secs(60),
            ))
        }
    }

    #[test]
    fn a_closed_generation_lease_holds_the_factory_until_retired() {
        let (mut host, _serial) =
            issue_offline_host(TestNativeEnvelope::default(), Arc::new(OpenWindows));
        let factory = Arc::clone(host.factory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("generated.sqlite3.lease")).unwrap();
        let root = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let authority: Arc<dyn OriginalNativeProjectionLeaseAuthority> = Arc::new(AdmittedLease);
        // SAFETY: the test funds the host and is the sole user of this fresh
        // sidecar; the lease authority admits only this host's factory.
        let lease = unsafe {
            OriginalGenerationLease::prepare(
                Arc::clone(&factory),
                root,
                OsString::from("generated.sqlite3.lease"),
                authority,
            )
        }
        .unwrap();
        lease.acquire().unwrap();
        // SAFETY: no consumer of this generation exists in the test.
        let closed = unsafe { lease.close_after_consumers(&factory) }
            .unwrap()
            .expect("an acquired lease closes into a receipt");
        if let Err((_, error)) = host.retain_lease(closed) {
            panic!("lease census refused: {error:#}");
        }
        drop(host.finish_backend().unwrap());
        drop(factory);
        assert!(!lease.is_retired().unwrap());

        // The closed lease still holds the factory through its receipt, so
        // the host refuses its release and keeps everything.
        let mut host = match host.release() {
            Ok(_) => panic!("an unretired lease must keep the host from releasing"),
            Err(OfflineNativeRetained { host, error }) => {
                let error = format!("{error:#}");
                assert!(
                    error.contains("factory strong count 3 (host holds 2)"),
                    "{error}"
                );
                host
            }
        };

        // Retiring against the finished backend ends that alias.
        let terminal = host.finish_backend().unwrap();
        lease.retire_after_native_terminal(&terminal).unwrap();
        assert!(lease.is_retired().unwrap());
        drop(terminal);
        if let Err(OfflineNativeRetained { error, .. }) = host.release() {
            panic!("a retired lease must not keep the host: {error:#}");
        }
    }
}
