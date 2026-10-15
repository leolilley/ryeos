//! Actual Store-parent receiver shared by Runtime and State host edges.
//! Preparation retains the whole original owner; it proves no retirement.
use super::live_native::{LiveStateOpen, LiveStoreParentFinalizationParts};
use anyhow::{Context, Result, ensure};
use lillux_sqlite::{OriginalNativeDestinationAuthority, OriginalNativeFactory, OriginalNativeTerminal};
use lillux_sqlite::original_physical_custody::OutsideWork;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub fn quote_store_parent_transfer() -> Result<OutsideWork> {
    Ok(OutsideWork { bytes: u64::try_from(std::mem::size_of::<LiveStateOpen>())?, steps: 8 })
}

pub fn quote_store_receiver() -> Result<OutsideWork> {
    let bytes = std::mem::size_of::<LiveStoreDestinationJoin>()
        .checked_add(2 * std::mem::size_of::<usize>())
        .context("original Store receiver storage quote overflow")?;
    Ok(OutsideWork { bytes: u64::try_from(bytes)?, steps: 8 })
}

pub fn quote_store_leaf_transfers(leaves: usize) -> Result<OutsideWork> {
    let bytes = 2usize.checked_mul(std::mem::size_of::<lillux_sqlite::OriginalDestinationPhysicalSource>())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<StoreLeafContinuation>()))
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<lillux_sqlite::OriginalDestinationPhysicalSource>>()))
        .context("Store leaf outside storage quote overflow")?;
    OutsideWork { bytes: u64::try_from(bytes)?, steps: 8 }
        .checked_mul(u64::try_from(leaves)?)
}

pub(super) struct LiveStoreDestinationJoin {
    process: u32,
    factory: Weak<OriginalNativeFactory>,
    original: Mutex<ManuallyDrop<Option<Arc<LiveStateOpen>>>>,
    // Native ACK does not retire this real Store body. A consuming outside
    // finalization frame must retain the body authorities and verify the
    // canonical physical registrations before clearing this obligation.
    parent_outside_pending: std::sync::atomic::AtomicBool,
    // Captured while the native terminal is still live. It lets the later
    // physical-retirement stage authenticate this parent without touching
    // the disposed native region.
    parent_registry_identity: OnceLock<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
}
impl LiveStoreDestinationJoin {
    /// The registry-free part of parent completion: this process, the
    /// outside completion no longer pending and no parent body retained. A
    /// dropped receiver settles on exactly these checks.
    pub(super) fn require_parent_completed_holders(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original Store receiver");
        ensure!(!self.parent_outside_pending.load(std::sync::atomic::Ordering::Acquire),
            "Store parent outside completion remains pending");
        let original = self.original.try_lock()
            .map_err(|_| anyhow::anyhow!("actual Store receiver busy or poisoned"))?;
        ensure!(original.is_none(), "actual Store parent remains in the destination receiver");
        Ok(())
    }

    pub(super) fn require_original_origin(&self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &OriginalNativeTerminal,
    ) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original Store receiver");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        ensure!(Weak::ptr_eq(&self.factory, &Arc::downgrade(factory)),
            "Store receiver changed original factory");
        Ok(())
    }

    pub(super) fn require_parent_completed(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
    ) -> Result<()> {
        self.require_original_origin(factory, aggregate)?;
        self.require_parent_completed_holders()
    }

    pub(super) fn require_parent_completed_in_registry(
        &self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_parent_completed_holders()?;
        let completed = self.parent_registry_identity.get()
            .context("Store parent completion lacks its pre-disposal registry identity")?;
        completed.require_same_registry(&registry.identity()?)
    }

    pub(super) fn require_parent_completed_in_retired_registry(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_parent_completed_holders()?;
        let completed = self.parent_registry_identity.get()
            .context("Store parent completion lacks its pre-disposal registry identity")?;
        registry.require_same_registry(completed)
    }

    pub(super) fn require_original_parent(&self,
        original: &Arc<LiveStateOpen>, factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
    ) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original Store destination receiver");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        ensure!(Weak::ptr_eq(&self.factory, &Arc::downgrade(factory)),
            "Store destination receiver changed original factory");
        let canonical = self.original.try_lock()
            .map_err(|_| anyhow::anyhow!("actual Store receiver busy or poisoned"))?;
        ensure!(canonical.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, original)),
            "Store destination receiver changed actual parent allocation");
        Ok(())
    }

    /// Clear the Store leaf-retirement barrier only after the exact physical
    /// parent has been consumed into retained finalization parts and all three
    /// parent roots plus the canonical operator lock still belong to this
    /// registry. The payer runs with no receiver mutex held.
    pub(super) fn complete_parent_outside(
        &self,
        parts: &LiveStoreParentFinalizationParts,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(self.process == std::process::id() && parts.process == std::process::id(),
            "foreign Store parent completion");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        ensure!(parts.factory.as_ref().is_some_and(|owner| owner.same_original(factory)),
            "Store finalization parts changed original native factory");
        parts.lease_authority.as_ref()
            .context("Store finalization projection lease authority was consumed early")?
            .require_original(factory, false)?;
        ensure!(parts.signer.is_some() && parts.trust.is_some()
            && parts.runtime_path.is_some() && parts.deadline.is_some()
            && parts.process_identity.is_some() && parts.scheduler_admission_phase.is_some()
            && parts.accounting_admission_phase.is_some(),
            "Store finalization metadata authority was not retained as a complete frame");
        ensure!(std::ptr::eq(parts.join.as_ref(), self),
            "Store finalization parts belong to a different receiver");
        ensure!(Weak::ptr_eq(&self.factory, &Arc::downgrade(factory)),
            "Store parent completion changed original factory");
        let has_no_parent = {
            let original = self.original.try_lock()
                .map_err(|_| anyhow::anyhow!("actual Store receiver busy or poisoned"))?;
            original.is_none()
        };
        ensure!(has_no_parent,
            "actual Store parent body remains in the destination receiver");

        let membership = registry.membership_work_bound(4, 0)?;
        let quote = OutsideWork { bytes: membership.bytes,
            steps: membership.steps.checked_add(8)
                .context("Store parent completion work quote overflow")? };
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            registry.prepay_transfer(prepay, quote.bytes, quote.steps))?;

        // Revalidate all evidence after the irreversible debit and before the
        // one-way pending-bit transition. The parts retain the actual lease;
        // its owner must be the same allocation registered as the operator.
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let retained_factory = parts.factory.as_ref()
            .context("Store finalization factory disappeared after funding")?;
        ensure!(retained_factory.same_original(factory),
            "Store finalization factory changed after funding");
        parts.lease_authority.as_ref()
            .context("Store finalization lease authority disappeared after funding")?
            .require_original(factory, false)?;
        let original = self.original.try_lock()
            .map_err(|_| anyhow::anyhow!("actual Store receiver busy or poisoned after funding"))?;
        ensure!(original.is_none(), "actual Store parent reappeared after transfer");
        drop(original);
        for (slot, binding) in parts.roots.iter().enumerate() {
            registry.require_membership(binding)?;
            let identity = binding.observed_source_identity()?;
            ensure!(if slot == 2 {
                matches!(identity,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })
            } else {
                matches!(identity,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
            }, "Store parent root membership changed role");
        }
        registry.require_membership(&parts.operator)?;
        ensure!(parts.operator.is_operator_lock()?,
            "Store parent operator membership changed role");
        let identity = parts.exclusion_identity.upgrade()
            .context("Store parent operator identity expired")?;
        let lease = parts.exclusion_alias.as_ref()
            .context("Store parent exclusion lease was retired before settlement")?;
        let alias = lease.original_owner_alias()?;
        ensure!(Arc::ptr_eq(&alias, &identity),
            "Store parent exclusion lease no longer matches the registered operator owner");
        drop(alias);
        drop(identity);

        let identity = registry.identity()?;
        if let Err(identity) = self.parent_registry_identity.set(identity) {
            self.parent_registry_identity
                .get()
                .context("Store parent completion identity cache disappeared")?
                .require_same_registry(&identity)?;
        }

        match self.parent_outside_pending.compare_exchange(
            true, false, std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(false) => Ok(()), // Exact already-completed retry; checks above still ran.
            Err(_) => anyhow::bail!("Store parent completion state changed unexpectedly"),
        }
    }

    /// The real closed Store caller has handed every selected native source
    /// to its typed receiver and cached all child frames. Retain the complete
    /// parent on uniqueness refusal; no census is derived from Arc counts.
    pub(super) fn transfer_original_parent(&self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &OriginalNativeTerminal,
        destination: &mut Option<LiveStateOpen>, prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign Store parent transfer");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        ensure!(Weak::ptr_eq(&self.factory, &Arc::downgrade(factory)),
            "Store parent transfer changed original factory");
        ensure!(destination.is_none(), "actual Store parent body already cached");
        let quote = quote_store_parent_transfer()?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        ensure!(self.process == std::process::id(), "foreign Store parent after funding");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let original = {
            let mut slot = self.original.try_lock()
                .map_err(|_| anyhow::anyhow!("actual Store parent checkout busy or poisoned"))?;
            slot.take().context("actual Store parent body absent from receiver")?
        };
        // Never invoke a native callback while the retention mutex is held.
        match Arc::try_unwrap(original) {
            Ok(original) => { *destination = Some(original); Ok(()) }
            Err(original) => {
                let mut slot = self.original.lock().unwrap_or_else(|p| p.into_inner());
                **slot = Some(original);
                anyhow::bail!("actual Store parent retains external aliases");
            }
        }
        // parent_outside_pending stays true until the body and its authorities
        // move into the retained finalization frame.
    }

    fn discard_unpublished(self: Arc<Self>, _original: &Arc<LiveStateOpen>) {
        // Private callers pass the same still-owned source used to construct
        // this unpublished candidate. Take only its redundant Arc under the
        // Rust gate, then drop that alias outside the gate. No reference-count
        // observation supplies authority, and no branch forgets the candidate.
        let alias = {
            let mut canonical = self.original.lock().unwrap_or_else(|p| p.into_inner());
            canonical.take()
        };
        drop(alias);
        // An unpublished candidate never had a parent to complete: it ends
        // settled, holding nothing.
        self.parent_outside_pending.store(false, std::sync::atomic::Ordering::Release);
    }

    fn prepare(original: &Arc<LiveStateOpen>, factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal, prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<Arc<Self>> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        original.require_original(factory)?;
        let existing = {
            let link = original.destination_join_link().try_lock()
                .map_err(|_| anyhow::anyhow!("original Store receiver link busy or poisoned"))?;
            link.upgrade()
        };
        if let Some(existing) = existing {
            existing.require_original_parent(original, factory, aggregate)?;
            return Ok(existing);
        }
        let quote = quote_store_receiver()?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        original.require_original(factory)?;
        let candidate = Arc::new(Self {
            process: std::process::id(), factory: Arc::downgrade(factory),
            original: Mutex::new(ManuallyDrop::new(Some(Arc::clone(original)))),
            parent_outside_pending: std::sync::atomic::AtomicBool::new(true),
            parent_registry_identity: OnceLock::new(),
        });
        // No native/account callback runs under this publication gate.
        let mut link = match original.destination_join_link().try_lock() {
            Ok(link) => link,
            Err(_) => {
                candidate.discard_unpublished(original);
                anyhow::bail!("original Store receiver link busy or poisoned");
            }
        };
        if let Some(existing) = link.upgrade() {
            drop(link);
            candidate.discard_unpublished(original);
            existing.require_original_parent(original, factory, aggregate)?;
            return Ok(existing);
        }
        *link = Arc::downgrade(&candidate);
        Ok(candidate)
    }
}

impl Drop for LiveStoreDestinationJoin {
    fn drop(&mut self) {
        // Settled exactly when parent completion would accept this receiver
        // without a registry: no outside completion pending and no parent
        // body retained. The slot is taken exactly once, here; otherwise the
        // parent body stays retained until the process exits. Exclusive
        // access reaches the slot without entering its mutex.
        if lillux::exec::in_forked_child() || self.process != std::process::id() {
            // A copy in another process holds none of these owners: the parent body, the
            // factory edge and the captured registry identity are
            // relinquished whole.
            let original = unsafe { std::mem::ManuallyDrop::take(self.original.get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)) };
            let factory = std::mem::replace(&mut self.factory, Weak::new());
            let identity = std::mem::take(&mut self.parent_registry_identity);
            lillux::custody::relinquish_in_foreign_process((original, factory, identity));
            return;
        }
        // Settlement is evaluated only in the original process: the check
        // may allocate its refusal.
        let settled = self.require_parent_completed_holders().is_ok();
        let original = unsafe { std::mem::ManuallyDrop::take(self.original.get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)) };
        if settled {
            drop(original);
        } else {
            lillux::custody::abandon("ryeos-app::store-destination-join", original);
        }
    }
}

/// Cache the exact original receiver on the real normal/failed Store frame.
/// This is preparation only; native/physical/account dispositions remain open.
pub(super) fn prepare_store_destination_join(
    destination: &OnceLock<Arc<LiveStoreDestinationJoin>>,
    original: &Arc<LiveStateOpen>, factory: &Arc<OriginalNativeFactory>,
    aggregate: &OriginalNativeTerminal, prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    if let Some(join) = destination.get() {
        return join.require_original_parent(original, factory, aggregate);
    }
    let join = LiveStoreDestinationJoin::prepare(original, factory, aggregate, prepay)?;
    match destination.set(join) {
        Ok(()) => Ok(()),
        Err(join) => {
            let actual = destination.get().context("actual Store receiver publication absent")?;
            ensure!(Arc::ptr_eq(actual, &join), "Store source selected conflicting original receivers");
            actual.require_original_parent(original, factory, aggregate)
        }
    }
}

struct StoreLeafContinuation {
    join: Arc<LiveStoreDestinationJoin>,
    evidence: lillux_sqlite::OriginalNativeOutsideLeafEvidence,
}
// This continuation owns no new admission. Its same original Store receiver
// retains the entire native-affinity graph until the real body/disposal join.
// Candidate source: cumulative independent lifecycle review is still required.
unsafe impl lillux_sqlite::OriginalOutsideDestinationContinuation for StoreLeafContinuation {
    fn require_original_process(&self) -> Result<()> {
        ensure!(self.join.process == std::process::id(), "foreign Store leaf continuation");
        Ok(())
    }
    fn retire_native_references(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal, _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.evidence.require_original(factory, aggregate)?;
        ensure!(Weak::ptr_eq(&self.join.factory, &Arc::downgrade(factory)),
            "Store leaf continuation changed original factory");
        let original = self.join.original.try_lock()
            .map_err(|_| anyhow::anyhow!("actual Store receiver busy or poisoned"))?;
        ensure!(original.is_none()
            && !self.join.parent_outside_pending.load(std::sync::atomic::Ordering::Acquire),
            "actual Store parent body/native-affinity retirement remains incomplete");
        Ok(())
    }
    unsafe fn register_shared_roots(&mut self,
        _registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()?;
        // Leaf root aliases are actual sources in the lower outside token.
        // The original Store body also contains App root/exclusion/control
        // owners; those require their complete typed parent continuation.
        ensure!(!self.join.parent_outside_pending.load(std::sync::atomic::Ordering::Acquire),
            "actual Store parent physical handoff remains incomplete");
        Ok(())
    }
    fn physical_holders_retired(&self) -> Result<bool> {
        self.require_original_process()?;
        let original = self.join.original.try_lock()
            .map_err(|_| anyhow::anyhow!("actual Store receiver busy or poisoned"))?;
        Ok(original.is_none()
            && !self.join.parent_outside_pending.load(std::sync::atomic::Ordering::Acquire))
    }
    fn physical_holders_retirement_quote(&self) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        self.require_original_process()?;
        Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 })
    }
}

pub(super) fn transfer_store_leaf(
    original: Arc<LiveStateOpen>, factory: &Arc<OriginalNativeFactory>,
    aggregate: &OriginalNativeTerminal,
    evidence: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> std::result::Result<lillux_sqlite::OriginalDestinationOutsideCustody,
    lillux_sqlite::OriginalDestinationCustodyFailure> {
    let prepared = (|| -> Result<_> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        original.require_original(factory)?;
        let quote = quote_store_leaf_transfers(1)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        original.require_original(factory)?;
        let authority: Arc<dyn OriginalNativeDestinationAuthority> = original.clone();
        evidence.require_original_authority(&authority, factory, aggregate)?;
        drop(authority);
        let join = {
            let link = original.destination_join_link().try_lock()
                .map_err(|_| anyhow::anyhow!("actual Store receiver link busy or poisoned"))?;
            link.upgrade().context("actual Store supervisor has not prepared its receiver")?
        };
        join.require_original_parent(&original, factory, aggregate)?;
        let mut sources = Vec::new();
        sources.try_reserve_exact(2)?;
        sources.extend(original.original_destination_roots());
        let continuation = Box::new(StoreLeafContinuation { join, evidence: evidence.clone() });
        Ok((sources, continuation))
    })();
    let (sources, continuation) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(
            factory.clone(), original, error)),
    };
    // The complete original parent is already retained by the actual receiver;
    // cache all genuine root aliases and continuation before ending this alias.
    let outside = unsafe { lillux_sqlite::OriginalDestinationOutsideCustody::issued(sources, continuation) };
    drop(original);
    Ok(outside)
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const KIND: &str = "ryeos-app::store-destination-join";

    fn join(parent_outside_pending: bool) -> LiveStoreDestinationJoin {
        LiveStoreDestinationJoin {
            process: std::process::id(),
            factory: Weak::new(),
            original: Mutex::new(ManuallyDrop::new(None)),
            parent_outside_pending: std::sync::atomic::AtomicBool::new(parent_outside_pending),
            parent_registry_identity: OnceLock::new(),
        }
    }

    #[test]
    fn dropped_completed_join_releases_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let join = join(false);
        join.require_parent_completed_holders().unwrap();
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(join);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn dropped_join_with_pending_parent_completion_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let join = join(true);
        assert!(join.require_parent_completed_holders().is_err());
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(join);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
    }
}

#[cfg(test)]
mod quote_tests {
    use super::{quote_store_leaf_transfers, quote_store_parent_transfer, quote_store_receiver};
    use lillux_sqlite::original_physical_custody::OutsideWork;
    use std::mem::size_of;

    #[test]
    fn store_destination_quotes_match_the_charged_shapes() {
        assert_eq!(quote_store_parent_transfer().unwrap().steps, 8);
        assert_eq!(quote_store_receiver().unwrap().steps, 8);
        let one_leaf = quote_store_leaf_transfers(1).unwrap();
        assert_eq!(one_leaf.steps, 8);
        assert_eq!(quote_store_leaf_transfers(3).unwrap(), one_leaf.checked_mul(3).unwrap());
        assert_eq!(one_leaf.bytes, (2 * size_of::<lillux_sqlite::OriginalDestinationPhysicalSource>()
            + size_of::<super::StoreLeafContinuation>()
            + size_of::<Vec<lillux_sqlite::OriginalDestinationPhysicalSource>>()) as u64);
        assert_eq!(quote_store_receiver().unwrap().bytes, (size_of::<super::LiveStoreDestinationJoin>()
            + 2 * size_of::<usize>()) as u64);
        assert_eq!(OutsideWork::ZERO.checked_add(one_leaf).unwrap(), one_leaf);
    }
}
