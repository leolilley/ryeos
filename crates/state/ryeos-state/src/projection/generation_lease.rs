//! One original native lock owner for an exact generation sidecar. Shared
//! references retain this owner; they never open another native lock or ticket.
use anyhow::{Context, Result};
use lillux_sqlite::{
    ClosedOriginalProjectionLease, OriginalNativeFactory, OriginalNativeProjectionLeaseAuthority,
    OriginalProjectionLease, OriginalProjectionLeaseDisposition, OriginalProjectionLeaseFailure,
    OriginalProjectionLeaseInputs, OriginalProjectionLeaseMode,
};
use std::{
    ffi::OsString,
    mem::ManuallyDrop,
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
/// State owns coordinate meaning; Lillux owns the actual paid lock effects.
pub(crate) struct OriginalGenerationLease {
    // Affinity survives as a weak coordinate after native holder retirement;
    // the outside metadata owner must not keep resident storage alive.
    process: u32,
    factory: Weak<OriginalNativeFactory>,
    root: lillux::PinnedDirectory,
    name: OsString,
    identity: (u64, u64),
    custody: ManuallyDrop<Mutex<LeaseCustody>>,
}
impl OriginalGenerationLease {
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
        Ok(Arc::new(Self {
            process: std::process::id(),
            factory: Arc::downgrade(&factory),
            root,
            name,
            identity,
            custody: ManuallyDrop::new(Mutex::new(LeaseCustody::Prepared(inputs))),
        }))
    }
    /// Consume only the genuinely retired owner. This moves the actual root;
    /// it performs no close and never consults a possibly released native arena.
    pub(crate) fn try_into_metadata_root(
        self: Arc<Self>,
    ) -> std::result::Result<lillux::PinnedDirectory, Arc<Self>> {
        if self.process != std::process::id() {
            return Err(self);
        }
        let retired = match self.custody.try_lock() {
            Ok(custody) => matches!(&*custody, LeaseCustody::Retired),
            Err(_) => false,
        };
        if !retired {
            return Err(self);
        }
        match Arc::try_unwrap(self) {
            Ok(owner) => {
                let Self { root, custody, .. } = owner;
                // Only an empty Retired phase can reach this unique transfer.
                // Destroying that bookkeeping mutex does not close a host owner.
                drop(ManuallyDrop::into_inner(custody));
                Ok(root)
            }
            Err(owner) => Err(owner),
        }
    }
    fn require_coordinate(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        factory.require_original_process()?;
        anyhow::ensure!(
            Weak::ptr_eq(&Arc::downgrade(factory), &self.factory),
            "foreign original generation lease factory"
        );
        self.root.ensure_path_binding()?;
        let current = self
            .root
            .entry_no_follow(&self.name)?
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
    pub(crate) fn metadata(&self) -> Result<std::fs::Metadata> {
        let factory = self.factory()?;
        self.require_coordinate(&factory)?;
        self.root
            .open_mount_entry(&self.name)?
            .context("original lease metadata absent")?
            .metadata()
            .map_err(anyhow::Error::from)
    }
    pub(crate) fn factory(&self) -> Result<Arc<OriginalNativeFactory>> {
        let factory = self
            .factory
            .upgrade()
            .context("original generation factory retired")?;
        factory.require_original_process()?;
        Ok(factory)
    }
    pub(crate) fn identity(&self) -> (u64, u64) {
        self.identity
    }
    pub(crate) fn require_active(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        self.require_coordinate(factory)?;
        let custody = self
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
