//! Actual live StateStore namespace and partial-start custody. Admission is
//! issued only from the retained opening owner, never from a path/domain DTO.

use super::*;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeBoundMode, OriginalNativeDestinationAuthority,
    OriginalNativeDomain, OriginalNativeFactory,
};

pub(super) struct LiveStateOpen {
    outside_join: Mutex<std::sync::Weak<super::live_destination_join::LiveStoreDestinationJoin>>,
    app_root: lillux::PinnedDirectory,
    runtime: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
    exclusion: Arc<ryeos_node_runtime::state_lock::StateLockLease>,
    pub(super) factory: Arc<OriginalNativeFactory>,
    lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
    signer: Arc<dyn Signer>,
    trust: Arc<ryeos_state::refs::TrustStore>,
    runtime_path: PathBuf,
    deadline: lillux::time::MonotonicDeadline,
    process: lillux::process_control::ExactProcessIdentity,
    scheduler_admission_phase: std::sync::atomic::AtomicU8,
    accounting_admission_phase: std::sync::atomic::AtomicU8,
}

/// Actual Store descriptors plus all remaining original host obligations.
/// Registration is not descriptor disposal or durable settlement.
pub(super) struct LiveStorePhysicalParent {
    inner: lillux::custody::Held<LiveStorePhysicalParentParts>,
}
const STORE_PHYSICAL_PARENT_KIND: &str = "ryeos-app::store-physical-parent";

/// Every field of a Store physical parent, in the order its release drops
/// them. No drop settles a physical parent: it always holds the Store host
/// with its factory, lease authority and exclusion lease, which only the
/// finalization-parts transfer consumes, so it is held until taken. Fields are
/// private to this module.
pub(super) struct LiveStorePhysicalParentParts {
    host: LiveStorePhysicalHost,
    directories: [Option<Arc<lillux::PinnedDirectory>>; 2],
    namespace: Option<lillux::PinnedDirectoryLock>,
    registrations: [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>; 3],
}
impl std::ops::Deref for LiveStorePhysicalParent {
    type Target = LiveStorePhysicalParentParts;
    fn deref(&self) -> &LiveStorePhysicalParentParts { &self.inner }
}
impl LiveStorePhysicalParent {
    fn parts_mut(&mut self) -> &mut LiveStorePhysicalParentParts { self.inner.get_mut() }
    /// Move every field out; a copy in another process is refused and comes
    /// back still held. The issuing process is the first element.
    fn into_parts(self) -> std::result::Result<(u32, LiveStorePhysicalParentParts), Self> {
        let process = self.inner.issuer();
        self.inner.take_whole().map(|parts| (process, parts)).map_err(|inner| Self { inner })
    }
}

/// Hands a held owner on whole. Every caller has already refused a foreign
/// process before it holds anything, so a foreign copy reached here is
/// relinquished and the process aborts.
pub(super) fn take_held<T: Send + 'static>(held: lillux::custody::Held<T>) -> T {
    held.take_or_abort()
}

/// Kinds under which an owner held across a fallible stretch is retained
/// when the stretch unwinds before handing it back.
const SCHEDULER_ADMISSION_UNWOUND_KIND: &str = "ryeos-app::scheduler-admission-unwound";
/// Kind under which a refused startup recovery retains its close progress.
const STORE_STARTUP_RECOVERY_KIND: &str = "ryeos-app::store-startup-recovery-failure";
struct LiveStorePhysicalHost {
    _outside_join: Mutex<std::sync::Weak<super::live_destination_join::LiveStoreDestinationJoin>>,
    exclusion_alias: Arc<ryeos_node_runtime::state_lock::StateLockLease>,
    exclusion_owner: Option<Arc<lillux::ExactExclusiveFileLock>>,
    exclusion_registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
    exclusion_identity: Option<std::sync::Weak<lillux::ExactExclusiveFileLock>>,
    factory: Arc<OriginalNativeFactory>,
    _lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
    _signer: Arc<dyn Signer>,
    _trust: Arc<ryeos_state::refs::TrustStore>,
    _runtime_path: PathBuf,
    _deadline: lillux::time::MonotonicDeadline,
    _process: lillux::process_control::ExactProcessIdentity,
    _scheduler_admission_phase: std::sync::atomic::AtomicU8,
    _accounting_admission_phase: std::sync::atomic::AtomicU8,
}

/// Store-side owners that must survive disposal of the live Store body.
/// Physical roots and the canonical operator lock already reside in the same
/// registry; this frame explicitly retains the original lease/factory
/// authorities and StateLockLease until their own finalizer stages consume
/// them.
pub(super) struct LiveStoreParentFinalizationParts {
    pub(super) process: u32,
    pub(super) join: Arc<super::live_destination_join::LiveStoreDestinationJoin>,
    pub(super) roots: [lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration; 3],
    pub(super) operator: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    pub(super) exclusion_alias: Option<Arc<ryeos_node_runtime::state_lock::StateLockLease>>,
    pub(super) exclusion_identity: std::sync::Weak<lillux::ExactExclusiveFileLock>,
    pub(super) factory: Option<Arc<OriginalNativeFactory>>,
    pub(super) lease_authority: Option<Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>>,
    pub(super) signer: Option<Arc<dyn Signer>>,
    pub(super) trust: Option<Arc<ryeos_state::refs::TrustStore>>,
    pub(super) runtime_path: Option<PathBuf>,
    pub(super) deadline: Option<lillux::time::MonotonicDeadline>,
    pub(super) process_identity: Option<lillux::process_control::ExactProcessIdentity>,
    pub(super) scheduler_admission_phase: Option<std::sync::atomic::AtomicU8>,
    pub(super) accounting_admission_phase: Option<std::sync::atomic::AtomicU8>,
    pub(super) native_authorities_retired: bool,
}

/// Quote the completed Store facade edge before it is retired after all
/// child custody and the exact parent receiver have been checked.
pub fn quote_store_parent_facade_retirement() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 4 }
}

/// Quote transfer of one Runtime or Operational packet vector into the
/// retained outside progress frame. The packet owner separately quotes its
/// bounded per-source storage and normalization work.
pub fn quote_store_packet_vector_handoff() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 4 }
}

/// Quote the State/Operational parent-authority handoff performed before a
/// child source owner can bind to the same Store parent.
pub fn quote_store_parent_authority_handoff() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
}

/// Quote one upper physical-owner alias retirement after the child has joined
/// the same registry and the exact Store parent has been revalidated.
pub fn quote_store_parent_alias_retirement() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
}

/// Quote the normal-close child alias join, which retains both the destination
/// authority and projection lease through the same State retirement call.
pub fn quote_store_child_parent_alias_retirement() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 4 }
}

/// Quote the direct Operational source attachment used by failed startup.
pub fn quote_store_direct_operational_handoff() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
}

/// Quote preparation of the Store destination join over the retained parent.
pub fn quote_store_destination_join_preparation() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
}

/// Quote the fixed transfer bookkeeping before a Store root is registered.
pub fn quote_store_root_registration_handoff() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
}

/// Quote initial registration of the canonical operator lock. Registry-owned
/// allocation and registration charges are separately priced by the registry.
pub fn quote_store_operator_lock_registration() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 }
}

/// Quote a retry check of the already registered canonical operator lock.
pub fn quote_store_operator_lock_membership(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    registry.membership_work_bound(1, 0)
}

/// Quote a Store parent-root pass, including the repeated completed-root
/// census and fixed role/control work. Each newly registered root has its own
/// handoff quote and registry registration quote.
pub fn quote_store_parent_root_membership(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    already_registered_directory_roots: usize,
    additional_sources: usize,
) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(already_registered_directory_roots <= 2,
        "Store root quote exceeds its two directory slots");
    anyhow::ensure!(additional_sources <= 3,
        "Store root quote exceeds its two directories and namespace");
    let queries = already_registered_directory_roots.checked_add(3)
        .context("Store root validation query count overflow")?;
    registry.membership_work_bound(u64::try_from(queries)?, additional_sources)?
        .checked_add(lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: 0,
            steps: 8,
        })
}

pub fn quote_store_completed_parent_root_membership(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    registry.membership_work_bound(3, 0)?.checked_add(
        lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 },
    )
}

/// Quote the final native-dependent Store-parent validation pass before the
/// resident region is released. The four physical registrations are checked
/// once more, along with the typed parent/factory authorities.
pub fn quote_store_parent_native_authority_retirement(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    registry.membership_work_bound(4, 0)?.checked_add(
        lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 16 },
    )
}

pub fn quote_store_physical_parent() -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let bytes = std::mem::size_of::<LiveStorePhysicalParent>()
        .checked_add(std::mem::size_of::<lillux::PinnedDirectory>())
        .and_then(|n| n.checked_add(2usize.checked_mul(std::mem::size_of::<usize>())?))
        .context("Store physical parent storage quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps: 8,
    })
}

/// Quote the retained Store owner frame that survives database disposal.
pub fn quote_store_parent_finalization_parts()
    -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<LiveStoreParentFinalizationParts>())?,
        steps: 8,
    })
}

impl LiveStorePhysicalParent {
    pub(super) fn require_original(&self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal) -> Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign Store physical parent");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(self.host.factory.same_original(factory), "Store physical parent changed factory");
        Ok(())
    }

    /// Consume the actual physical parent only after its enclosing progress
    /// owner has cached the returned frame. Transfer its exact factory,
    /// projection authority and StateLockLease into that retained frame so
    /// later finalizer stages can end each alias in the reviewed order.
    pub(super) fn into_finalization_parts(
        self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<LiveStoreParentFinalizationParts, (Self, anyhow::Error)> {
        let prepared = (|| -> Result<Arc<super::live_destination_join::LiveStoreDestinationJoin>> {
            self.require_original(factory, aggregate)?;
            anyhow::ensure!(self.directories.iter().all(Option::is_none)
                && self.namespace.is_none() && self.registrations.iter().all(Option::is_some),
                "actual Store physical roots are not fully registered");
            let host = &self.host;
            anyhow::ensure!(host.exclusion_owner.is_none(),
                "actual Store exclusion owner was not transferred to the canonical registry");
            let registration = host.exclusion_registration.as_ref()
                .context("actual Store operator-lock registration absent")?;
            anyhow::ensure!(registration.is_operator_lock()?,
                "actual Store exclusion registration has a foreign role");
            let identity = host.exclusion_identity.as_ref()
                .and_then(std::sync::Weak::upgrade)
                .context("actual Store operator-lock owner identity expired")?;
            let alias = host.exclusion_alias.original_owner_alias()?;
            anyhow::ensure!(Arc::ptr_eq(&alias, &identity),
                "actual Store exclusion lease changed canonical operator owner");
            drop(alias);
            drop(identity);
            let join = host._outside_join.try_lock()
                .map_err(|_| anyhow::anyhow!("actual Store parent receiver link busy or poisoned"))?
                .upgrade()
                .context("actual Store parent receiver disappeared before completion")?;
            join.require_original_origin(factory, aggregate)?;
            let quote = quote_store_parent_finalization_parts()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            self.require_original(factory, aggregate)?;
            anyhow::ensure!(self.directories.iter().all(Option::is_none)
                && self.namespace.is_none() && self.registrations.iter().all(Option::is_some),
                "actual Store physical roots changed while finalization parts were funded");
            let identity = host.exclusion_identity.as_ref()
                .and_then(std::sync::Weak::upgrade)
                .context("actual Store operator-lock owner identity expired after funding")?;
            let alias = host.exclusion_alias.original_owner_alias()?;
            anyhow::ensure!(Arc::ptr_eq(&alias, &identity),
                "actual Store exclusion lease changed owner during finalization funding");
            drop(alias);
            drop(identity);
            Ok(join)
        })();
        let join = match prepared {
            Ok(join) => join,
            Err(error) => return Err((self, error)),
        };

        let (process, LiveStorePhysicalParentParts { host, directories: _, namespace: _, registrations }) =
            match self.into_parts() {
                Ok(parts) => parts,
                Err(this) => return Err((this, anyhow::anyhow!("Store physical parent belongs to another process"))),
            };
        let LiveStorePhysicalHost {
            _outside_join: _, exclusion_alias, exclusion_owner, exclusion_registration,
            exclusion_identity, factory: native_factory, _lease_authority: lease_authority,
            _signer: signer, _trust: trust, _runtime_path: runtime_path, _deadline: deadline,
            _process: process_identity, _scheduler_admission_phase: scheduler_admission_phase,
            _accounting_admission_phase: accounting_admission_phase,
        } = host;
        // Keep every actual host authority in the retained finalization frame;
        // later stages consume it only after the native/descriptor joins.
        drop(exclusion_owner);
        let [Some(root0), Some(root1), Some(root2)] = registrations
            else { unreachable!("validated Store root registrations disappeared") };
        let operator = exclusion_registration
            .expect("validated Store operator registration disappeared");
        let exclusion_identity = exclusion_identity
            .expect("validated Store operator identity disappeared");
        Ok(LiveStoreParentFinalizationParts {
            process,
            join,
            roots: [root0, root1, root2],
            operator,
            exclusion_alias: Some(exclusion_alias),
            exclusion_identity,
            factory: Some(native_factory),
            lease_authority: Some(lease_authority),
            signer: Some(signer),
            trust: Some(trust),
            runtime_path: Some(runtime_path),
            deadline: Some(deadline),
            process_identity: Some(process_identity),
            scheduler_admission_phase: Some(scheduler_admission_phase),
            accounting_admission_phase: Some(accounting_admission_phase),
            native_authorities_retired: false,
        })
    }
    #[cfg(target_os = "linux")]
    pub(super) fn register_exclusion(&mut self, owner: &Arc<lillux::ExactExclusiveFileLock>,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original(factory, aggregate)?;
        owner.require_original_process()?;
        {
            let host = &self.host;
            if let Some(binding) = &host.exclusion_registration {
                anyhow::ensure!(host.exclusion_owner.is_none(), "registered Store exclusion retains pending alias");
                let quote = quote_store_operator_lock_membership(registry)?;
                registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
                self.require_original(factory, aggregate)?;
                owner.require_original_process()?;
                registry.require_membership(binding)?;
                anyhow::ensure!(binding.is_operator_lock()? && host.exclusion_identity.as_ref()
                    .is_some_and(|identity| std::sync::Weak::ptr_eq(identity, &Arc::downgrade(owner))),
                    "registered Store exclusion changed actual outside owner");
                return Ok(());
            }
        }
        let quote = quote_store_operator_lock_registration();
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        self.require_original(factory, aggregate)?;
        owner.require_original_process()?;
        let host = &mut self.parts_mut().host;
        if host.exclusion_owner.is_none() {
            // Admission inputs still retain this SAME lease until durable
            // settlement. Inventory registration must not require premature
            // uniqueness or end that original account protection.
            host.exclusion_owner = Some(host.exclusion_alias.original_owner_alias()?);
        }
        let original = host.exclusion_owner.as_ref().context("actual Store exclusion owner absent")?;
        anyhow::ensure!(Arc::ptr_eq(original, owner), "Store exclusion differs from actual outside operator owner");
        host.exclusion_identity = Some(Arc::downgrade(original));
        let original = host.exclusion_owner.take().expect("checked actual Store exclusion owner");
        match unsafe { registry.register_operator_lock(original, prepay) } {
            Ok(binding) => host.exclusion_registration = Some(binding),
            Err((original, error)) => { host.exclusion_owner = Some(original); return Err(error); }
        }
        let binding = host.exclusion_registration.as_ref().expect("cached actual Store exclusion registration");
        registry.require_membership(binding)?;
        anyhow::ensure!(binding.is_operator_lock()?, "actual Store exclusion registered wrong source role");
        Ok(())
    }
    fn from_original(original: LiveStateOpen) -> Self {
        let LiveStateOpen { outside_join, app_root, runtime, namespace, exclusion, factory,
            lease_authority, signer, trust, runtime_path, deadline, process, scheduler_admission_phase,
            accounting_admission_phase } = original;
        Self {
            inner: lillux::custody::Held::until_taken(
                STORE_PHYSICAL_PARENT_KIND,
                LiveStorePhysicalParentParts {
                    host: LiveStorePhysicalHost {
                        _outside_join: outside_join, exclusion_alias: exclusion,
                        exclusion_owner: None, exclusion_registration: None, exclusion_identity: None, factory, _lease_authority: lease_authority,
                        _signer: signer, _trust: trust, _runtime_path: runtime_path, _deadline: deadline,
                        _process: process, _scheduler_admission_phase: scheduler_admission_phase,
                        _accounting_admission_phase: accounting_admission_phase,
                    },
                    directories: [Some(Arc::new(app_root)), Some(runtime)],
                    namespace: Some(namespace),
                    registrations: std::array::from_fn(|_| None),
                },
            ),
        }
    }
    fn register_roots(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original(factory, aggregate)?;
        if self.directories.iter().all(Option::is_none)
            && self.namespace.is_none()
            && self.registrations.iter().all(Option::is_some)
        {
            let quote = quote_store_completed_parent_root_membership(registry)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.require_original(factory, aggregate)?;
            return self.require_registered_roots(registry);
        }
        let additional_sources = (0..2)
            .filter(|slot| self.registrations[*slot].is_none() && self.directories[*slot].is_some())
            .count()
            + usize::from(self.registrations[2].is_none() && self.namespace.is_some());
        // Existing directory bindings are checked in the loop, then all three
        // bindings are checked again by require_registered_roots. Reserve for
        // both passes before the first source moves; projected capacity still
        // covers only the sources this stage can add.
        let existing_directory_checks = self.registrations[..2]
            .iter()
            .filter(|binding| binding.is_some())
            .count();
        let quote = quote_store_parent_root_membership(
            registry, existing_directory_checks, additional_sources,
        )?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        self.require_original(factory, aggregate)?;
        for slot in 0..2 {
            if let Some(binding) = &self.registrations[slot] {
                registry.require_membership(binding)?;
                anyhow::ensure!(self.directories[slot].is_none(), "registered Store directory retains source");
                continue;
            }
            let quote = quote_store_root_registration_handoff();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.require_original(factory, aggregate)?;
            let original = self.parts_mut().directories[slot].take().context("actual Store directory absent without registration")?;
            // Same outside holder caches the registration or restores the actual
            // source on refusal; no descriptor is duplicated or reopened.
            match unsafe { registry.register_directory(original, prepay) } {
                Ok(binding) => self.parts_mut().registrations[slot] = Some(binding),
                Err((original, error)) => { self.parts_mut().directories[slot] = Some(original); return Err(error); }
            }
        }
        if self.registrations[2].is_none() {
            let quote = quote_store_root_registration_handoff();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.require_original(factory, aggregate)?;
            let original = self.parts_mut().namespace.take().context("actual Store namespace absent without registration")?;
            match unsafe { registry.register_namespace(original, prepay) } {
                Ok(binding) => self.parts_mut().registrations[2] = Some(binding),
                Err((original, error)) => { self.parts_mut().namespace = Some(original); return Err(error); }
            }
        }
        anyhow::ensure!(self.directories.iter().all(Option::is_none) && self.namespace.is_none(),
            "actual Store physical roots remain unregistered");
        self.require_registered_roots(registry)
    }

    fn require_registered_roots(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    ) -> Result<()> {
        anyhow::ensure!(self.directories.iter().all(Option::is_none) && self.namespace.is_none()
            && self.registrations.iter().all(Option::is_some),
            "actual Store physical root transfer is incomplete");
        for (slot, binding) in self.registrations.iter().enumerate() {
            let binding = binding.as_ref().context("actual Store root registration absent")?;
            registry.require_membership(binding)?;
            let identity = binding.observed_source_identity()?;
            anyhow::ensure!(if slot == 2 {
                matches!(identity, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })
            } else {
                matches!(identity, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
            }, "actual Store physical source role changed");
        }
        Ok(())
    }
}
/// Reuse one complete Store-body decomposition for normal and failed startup.
/// Both original body and partial parts remain in the same outside supervisor.
pub(super) fn register_store_parent_roots(
    body: &mut Option<LiveStateOpen>, parts: &mut Option<LiveStorePhysicalParent>,
    registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
    prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    if parts.is_none() {
        let original = body.as_ref().context("actual Store body absent before physical decomposition")?;
        original.require_close_factory(factory)?;
        let quote = quote_store_physical_parent()?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        body.as_ref().expect("retained original Store body").require_close_factory(factory)?;
        let original = body.take().expect("checked original Store body");
        *parts = Some(LiveStorePhysicalParent::from_original(original));
    }
    anyhow::ensure!(body.is_none(), "Store retains decomposed parts and duplicate body");
    parts.as_mut().expect("cached original Store physical parts")
        .register_roots(factory, aggregate, registry, prepay)
}

impl LiveStoreParentFinalizationParts {
    /// Retire every parent-frame reference that can call into resident native
    /// state while the genuine aggregate is still live. The cached registry
    /// identity and receiver completion then support a physical-only check
    /// after region disposal.
    pub(super) fn retire_native_authorities(
        &mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(!super::live_close::foreign_process(self.process), "foreign Store finalization parts");
        if self.native_authorities_retired {
            anyhow::ensure!(self.factory.is_none() && self.lease_authority.is_none(),
                "retired Store native authority reappeared");
            return self.join.require_parent_completed_in_registry(registry);
        }

        let quote = quote_store_parent_native_authority_retirement(registry)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(self.factory.as_ref().is_some_and(|owner| owner.same_original(factory)),
            "Store finalization parts changed original native factory");
        self.lease_authority.as_ref()
            .context("Store finalization projection lease authority absent before native retirement")?
            .require_original(factory, false)?;
        self.join.require_parent_completed(factory, aggregate)?;
        self.join.require_parent_completed_in_registry(registry)?;

        for (slot, registration) in self.roots.iter().enumerate() {
            registry.require_membership(registration)?;
            let identity = registration.observed_source_identity()?;
            anyhow::ensure!(if slot == 2 {
                matches!(identity,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })
            } else {
                matches!(identity,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
            }, "Store parent root changed role before native disposal");
        }
        registry.require_membership(&self.operator)?;
        anyhow::ensure!(self.operator.is_operator_lock()?,
            "Store parent operator membership changed before native disposal");
        let identity = self.exclusion_identity.upgrade()
            .context("Store parent operator identity expired before native disposal")?;
        let lease = self.exclusion_alias.as_ref()
            .context("Store parent exclusion lease absent before native disposal")?;
        let alias = lease.original_owner_alias()?;
        anyhow::ensure!(Arc::ptr_eq(&alias, &identity),
            "Store parent exclusion lease no longer matches the registered operator owner");
        drop(alias);
        drop(identity);

        let factory_authority = self.factory.take();
        let lease_authority = self.lease_authority.take();
        let (factory_authority, lease_authority) = match (factory_authority, lease_authority) {
            (Some(factory_authority), Some(lease_authority)) => (factory_authority, lease_authority),
            (factory_authority, lease_authority) => {
                self.factory = factory_authority;
                self.lease_authority = lease_authority;
                anyhow::bail!("Store native authority frame is incomplete before retirement");
            }
        };
        drop(factory_authority);
        drop(lease_authority);
        self.native_authorities_retired = true;
        Ok(())
    }

    pub(super) fn require_retired(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        anyhow::ensure!(!super::live_close::foreign_process(self.process), "foreign Store finalization parts");
        anyhow::ensure!(self.native_authorities_retired
            && self.factory.is_none() && self.lease_authority.is_none(),
            "retired Store parent retains native-dependent authorities");
        anyhow::ensure!(self.exclusion_alias.is_none(),
            "retired Store finalization parts retain a StateLockLease alias");
        anyhow::ensure!(self.exclusion_identity.upgrade().is_none(),
            "retired Store finalization parts retain an operator allocation alias");
        anyhow::ensure!(self.signer.is_some() && self.trust.is_some()
            && self.runtime_path.is_some() && self.deadline.is_some()
            && self.process_identity.is_some() && self.scheduler_admission_phase.is_some()
            && self.accounting_admission_phase.is_some(),
            "retired Store finalization metadata frame is incomplete");
        self.join.require_parent_completed_in_retired_registry(registry)?;
        for (slot, registration) in self.roots.iter().enumerate() {
            let identity = registration.observed_source_identity()?;
            anyhow::ensure!(if slot == 2 {
                matches!(identity,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })
            } else {
                matches!(identity,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
            }, "retired Store parent root registration changed role");
            drop(registration.receipt_in(registry)?);
        }
        anyhow::ensure!(self.operator.is_operator_lock()?,
            "retired Store parent operator registration changed role");
        #[cfg(target_os = "linux")]
        anyhow::ensure!(self.operator.operator_target_retired()?,
            "retired Store parent operator target lacks its close evidence");
        drop(self.operator.receipt_in(registry)?);
        Ok(())
    }

    /// End the Store's redundant StateLockLease only after the settled
    /// finalizer has retired every non-operator descriptor. The canonical
    /// operator owner remains in the same registry for its later target and
    /// parent close stages.
    pub(super) unsafe fn retire_settled_exclusion_alias(
        &mut self,
        admission_alias: Option<&Arc<ryeos_node_runtime::state_lock::StateLockLease>>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(!super::live_close::foreign_process(self.process), "foreign Store finalization parts");
        if self.exclusion_alias.is_none() && admission_alias.is_none() {
            return Ok(());
        }
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("original operator exclusion retirement is unsupported on this host");
        #[cfg(target_os = "linux")]
        {
        let alias_count = u64::try_from(usize::from(self.exclusion_alias.is_some())
            + usize::from(admission_alias.is_some()))?;
        let quote = registry.membership_work_bound(alias_count, 0)?.checked_add(
            lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 },
        )?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        anyhow::ensure!(!super::live_close::foreign_process(self.process), "foreign Store finalization after funding");
        anyhow::ensure!(registry.non_account_descriptors_retired(),
            "Store non-operator retirement changed after alias funding");
        let canonical = self.exclusion_identity.upgrade()
            .context("canonical Store operator owner disappeared before target retirement")?;
        if let Some(lease) = self.exclusion_alias.as_ref() {
            let alias = lease.original_owner_alias()?;
            registry.require_operator_lock_alias(&self.operator, &alias)?;
            anyhow::ensure!(Arc::ptr_eq(&alias, &canonical),
                "retained Store exclusion lease changed canonical operator owner");
            drop(alias);
        }
        if let Some(lease) = admission_alias {
            let alias = lease.original_owner_alias()?;
            registry.require_operator_lock_alias(&self.operator, &alias)?;
            anyhow::ensure!(Arc::ptr_eq(&alias, &canonical),
                "settled admission exclusion changed canonical operator owner");
            drop(alias);
        }
        drop(canonical);
        if self.exclusion_alias.is_some() {
            drop(self.exclusion_alias.take().expect("validated retained Store lease"));
        }
        Ok(())
        }
    }
}

impl LiveStateOpen {
    pub(super) fn require_unopened_native_authority(
        self: &Arc<Self>,
        authority: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<()> {
        factory.require_original_process()?;
        anyhow::ensure!(Arc::ptr_eq(&self.factory, factory),
            "unopened native authority uses another original factory");
        let expected: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> = self.clone();
        anyhow::ensure!(Arc::ptr_eq(authority, &expected),
            "unopened native input selects another Store authority");
        Ok(())
    }

    pub(super) fn require_unopened_scheduler_bound_input(
        self: &Arc<Self>,
        root: &Arc<lillux::PinnedDirectory>,
        authority: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<()> {
        self.require_unopened_native_authority(authority, factory)?;
        anyhow::ensure!(Arc::ptr_eq(root, &self.runtime),
            "unopened scheduler input selects another pinned State directory");
        Ok(())
    }

    pub(super) fn destination_join_link(&self)
        -> &Mutex<std::sync::Weak<super::live_destination_join::LiveStoreDestinationJoin>> {
        &self.outside_join
    }

    pub(super) fn original_destination_roots(&self)
        -> [lillux_sqlite::OriginalDestinationPhysicalSource; 2] {
        // SAME original allocations/lease, never new descriptors or reopened
        // paths. The caller prepays and retains the whole Store parent first.
        [lillux_sqlite::OriginalDestinationPhysicalSource::Directory(Arc::clone(&self.runtime)),
            lillux_sqlite::OriginalDestinationPhysicalSource::Namespace(self.namespace.clone())]
    }

    pub(super) fn capture(
        app_root: &Path,
        runtime_state_dir: &Path,
        runtime_path: &Path,
        exclusion: Arc<ryeos_node_runtime::state_lock::StateLockLease>,
        factory: Arc<OriginalNativeFactory>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        signer: Arc<dyn Signer>,
        trust: Arc<ryeos_state::refs::TrustStore>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Arc<Self>> {
        exclusion.ensure_protects_app_root(app_root)?;
        let app_root = lillux::PinnedDirectory::open(app_root)?.context("live App root absent")?;
        let runtime = Arc::new(
            lillux::PinnedDirectory::open(runtime_state_dir)?.context("live State root absent")?,
        );
        anyhow::ensure!(
            runtime_state_dir
                == ryeos_engine::roots::RuntimeRoot::new(app_root.path().to_owned()).state(),
            "live State root differs from initialized node"
        );
        anyhow::ensure!(
            runtime_path.parent() == Some(runtime.path()),
            "live Runtime database must belong to original State namespace"
        );
        factory.require_ready()?;
        lease_authority.require_original(&factory, false)?;
        factory.require_runtime_binding(&runtime)?;
        ryeos_state::signer::ensure_signer_trusted(signer.as_ref(), &trust)?;
        let namespace = runtime.lock_exclusive()?;
        let process = lillux::process_control::capture_current_process_identity()
            .map_err(anyhow::Error::msg)?;
        let original = Arc::new(Self {
            outside_join: Mutex::new(std::sync::Weak::new()),
            app_root,
            runtime,
            namespace,
            exclusion,
            factory,
            lease_authority,
            signer,
            trust,
            runtime_path: runtime_path.to_owned(),
            deadline,
            process,
            scheduler_admission_phase: std::sync::atomic::AtomicU8::new(0),
            accounting_admission_phase: std::sync::atomic::AtomicU8::new(0),
        });
        original.require_original(&original.factory)?;
        Ok(original)
    }

    pub(super) fn scheduler_admission(self: &Arc<Self>) -> Result<LiveSchedulerNativeAdmission> {
        // Preserve claim and prepared metadata before every fallible join.
        // No native ticket exists at this layer; failure never resets the claim.
        let mut attempt = LiveSchedulerAdmissionAttempt {
            original: Arc::clone(self), claimed: false, inputs: None, reference: None,
        };
        // Ordinary RAII before the claim: errors and unwind release only this
        // temporary reference, without retaining an unclaimed namespace owner.
        self.require_original(&self.factory)?;
        self.factory.require_ready()?;
        self.factory.require_runtime_binding(&self.runtime)?;
        anyhow::ensure!(!self.deadline.has_elapsed(), "original scheduler admission expired");
        self.scheduler_admission_phase.compare_exchange(0, 1,
            Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("original scheduler admission already issued or closed"))?;
        attempt.claimed = true;
        // No fallible operation occurs between claim and retained custody; an
        // unwind through issuance retains the claimed attempt, counted.
        let mut attempt = lillux::custody::Held::until_taken(SCHEDULER_ADMISSION_UNWOUND_KIND, attempt);
        let result = (|| -> Result<()> {
            let authority: Arc<dyn OriginalNativeDestinationAuthority> = self.clone();
            // SAFETY: same actual StateStore namespace, process, trusted node
            // and paid factory; only the fixed role is issued. Caller gates
            // still own scheduler selection, service mutation and shutdown.
            attempt.get_mut().inputs = Some(unsafe { OriginalNativeBoundInputs::issued(
                Arc::clone(&self.runtime), std::ffi::OsStr::new("scheduler.sqlite3"),
                OriginalNativeBoundMode::OpenOrCreate, OriginalNativeDomain::NodeScheduler,
                Arc::clone(&authority), None,
            ) }?);
            attempt.get_mut().reference = Some(unsafe { lillux_sqlite::OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::NodeSchedulerSchemaReference, authority,
            ) }?);
            self.require_original(&self.factory)?;
            anyhow::ensure!(self.scheduler_admission_phase.load(Ordering::Acquire) == 1,
                "scheduler metadata admission closed during issuance");
            Ok(())
        })();
        if let Err(error) = result {
            return Err(anyhow::Error::new(LiveSchedulerNativeAdmissionFailure {
                inner: lillux::custody::Held::until_taken(
                    SCHEDULER_ADMISSION_FAILURE_KIND,
                    LiveSchedulerNativeAdmissionFailureParts { error, attempt: take_held(attempt) },
                ),
            }));
        }
        let mut attempt = take_held(attempt);
        Ok(LiveSchedulerNativeAdmission {
            factory: Arc::clone(&attempt.original.factory),
            inputs: attempt.inputs.take().expect("prepared fixed scheduler inputs"),
            reference: attempt.reference.take().expect("prepared fixed scheduler reference"),
        })
    }

    pub(super) fn close_scheduler_admission(&self) {
        // Monotonic local denial only, no clock/native/namespace effect or ACK.
        // A racing pre-close claim retains its attempt; later claims refuse.
        self.scheduler_admission_phase.store(2, Ordering::Release);
    }

    /// One-use issuance of the node accounting ledger inputs from this actual
    /// Store opening owner: the fixed bound database and its schema
    /// reference, both under the same namespace, factory and authority as
    /// Runtime. An established ledger (initialization marker present) opens
    /// `Existing` only, so a lost financial database is never recreated.
    /// A refused claim is never reset; the ledger then stays fail-closed.
    pub(super) fn accounting_admission(self: &Arc<Self>) -> Result<LiveAccountingNativeAdmission> {
        self.require_original(&self.factory)?;
        self.factory.require_ready()?;
        self.factory.require_runtime_binding(&self.runtime)?;
        anyhow::ensure!(!self.deadline.has_elapsed(), "original accounting admission expired");
        self.accounting_admission_phase.compare_exchange(0, 1,
            Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("original accounting admission already issued"))?;
        let established = crate::accounting_db::ledger_established(&self.runtime)?;
        let mode = if established {
            OriginalNativeBoundMode::Existing
        } else {
            OriginalNativeBoundMode::OpenOrCreate
        };
        let directory = self.runtime.try_clone()?;
        let authority: Arc<dyn OriginalNativeDestinationAuthority> = self.clone();
        // SAFETY: same actual StateStore namespace, process, trusted node
        // and paid factory as Runtime; only the fixed ledger role is issued.
        // The ledger owner retains the inputs through open, close and the
        // Store's outside join.
        let inputs = unsafe { OriginalNativeBoundInputs::issued(
            Arc::clone(&self.runtime),
            std::ffi::OsStr::new(crate::accounting_db::ACCOUNTING_DB_FILENAME),
            mode, OriginalNativeDomain::NodeAccounting, Arc::clone(&authority), None,
        ) }?;
        // SAFETY: the same genuine namespace owner and prepaid factory; the
        // reference may build only the declared ledger schema.
        let reference = unsafe { lillux_sqlite::OriginalNativeTransientInputs::issued(
            OriginalNativeDomain::NodeAccountingSchemaReference, authority,
        ) }?;
        Ok(LiveAccountingNativeAdmission {
            factory: Arc::clone(&self.factory), directory, established, inputs, reference,
        })
    }

    pub(super) fn require_close_factory(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        self.require_original(factory)
    }

    pub(super) fn directory(&self) -> Result<lillux::PinnedDirectory> {
        self.require_original(&self.factory)?;
        self.runtime.try_clone()
    }
    pub(super) fn namespace(&self) -> lillux::PinnedDirectoryLock {
        self.namespace.clone()
    }

    pub(super) fn open_state(
        self: &Arc<Self>,
        directory: lillux::PinnedDirectory,
        namespace: lillux::PinnedDirectoryLock,
        repair_sink: Arc<dyn ryeos_state::ProjectionRepairSink>,
        trust: Arc<ryeos_state::refs::TrustStore>,
        observer: Option<Arc<dyn ryeos_state::ProjectionRecoveryObserver>>,
        runtime: &runtime_db::RuntimeDb,
    ) -> Result<StateDb> {
        self.require_original(&self.factory)?;
        let host: Arc<dyn OriginalNativeDestinationAuthority> = self.clone();
        // SAFETY: this opening owner retains the same node issuer checked at
        // capture, actual process/factory, original exclusion and State namespace.
        // Both delegated authorities retain this owner through cleanup; neither
        // service nor cleanup endpoint is supplied by an operation DTO.
        unsafe { StateDb::open_live_with_recovery_runtime_liveness_and_namespace_authority(
            self.runtime.path(),
            directory,
            namespace,
            repair_sink,
            trust,
            observer,
            runtime,
            Arc::clone(&self.factory),
            host,
            self.clone(),
            self.deadline,
        ) }
    }

    pub(super) fn open_runtime(self: &Arc<Self>) -> Result<runtime_db::RuntimeDb> {
        self.require_original(&self.factory)?;
        self.factory.require_ready()?;
        self.factory.require_runtime_binding(&self.runtime)?;
        anyhow::ensure!(
            !self.deadline.has_elapsed(),
            "original live State SQL deadline elapsed"
        );
        let name = self
            .runtime_path
            .file_name()
            .context("live Runtime basename absent")?;
        let authority: Arc<dyn OriginalNativeDestinationAuthority> = self.clone();
        // SAFETY: this is the actual StateStore opening owner under its original
        // node exclusion, pinned State namespace, trusted head writer and paid
        // factory. Lower Bound owns file acquisition; no SQLite alias is opened.
        let inputs = unsafe {
            OriginalNativeBoundInputs::issued(
                Arc::clone(&self.runtime),
                name,
                OriginalNativeBoundMode::OpenOrCreate,
                OriginalNativeDomain::NodeRuntime,
                authority,
                None,
            )
        }?;
        let reference_authority: Arc<dyn OriginalNativeDestinationAuthority> = self.clone();
        // Genuine Runtime namespace owner and same prepaid factory; the schema
        // reference gets its own actual nonrecycled transient ticket.
        let reference_inputs = unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::NodeRuntimeSchemaReference,
                reference_authority,
            )
        }?;
        let schema_reference = ryeos_state::sqlite_schema::prepare_native_schema_sql_reference(
            Arc::clone(&self.factory),
            reference_inputs,
            OriginalNativeDomain::NodeRuntimeSchemaReference,
            &crate::runtime_db::runtime_schema_sql(),
        )
        .map_err(anyhow::Error::new)?;
        runtime_db::RuntimeDb::open_live_with_namespace_authority(
            &self.runtime_path,
            Arc::clone(&self.runtime),
            self.namespace.clone(),
            Arc::clone(&self.factory),
            inputs,
            self.deadline,
            schema_reference,
        )
        .map_err(anyhow::Error::new)
    }
}

impl OriginalNativeDestinationAuthority for LiveStateOpen {
    fn into_outside_custody(self: Arc<Self>, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<lillux_sqlite::OriginalDestinationOutsideCustody,
        lillux_sqlite::OriginalDestinationCustodyFailure> {
        super::live_destination_join::transfer_store_leaf(self, factory, aggregate, leaf, prepay)
    }

    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        // Failing to read this process's own identity says nothing about the
        // original; the mismatch below is the fact.
        let process = lillux::process_control::capture_current_process_identity()
            // The class is the root, so it survives any wrapper a caller adds.
            .map_err(|error| anyhow::Error::new(lillux_sqlite::NativeCheckUnobservable).context(error))?;
        anyhow::ensure!(
            process == self.process && Arc::ptr_eq(factory, &self.factory),
            "live State admission differs from original process/factory"
        );
        self.exclusion
            .ensure_protects_app_root(self.app_root.path())?;
        self.app_root.ensure_path_binding()?;
        self.runtime.ensure_path_binding()?;
        self.namespace.ensure_protects(&self.runtime)?;
        ryeos_state::signer::ensure_signer_trusted(self.signer.as_ref(), &self.trust)?;
        // Destination custody remains valid in cleanup. Native service/cleanup
        // scopes enforce the original paid clock; a namespace check must not
        // turn service expiry into loss of the retained cleanup owner.
        Ok(())
    }
}

/// Local checkout, without a retention mutex across SQL/recovery. Successful
/// construction transfers both actual databases into StateStore; a refusal
/// transfers this guard into the typed error before any Drop can release them.
pub(super) struct StateOpenCustody {
    inner: lillux::custody::Held<StateOpenCustodyParts>,
}

/// The opening owner and the databases acquired from it. No implicit SQL
/// close or namespace release happens on late refusal or panic: supervised
/// close/recovery must consume the actual owners, and without it they stay
/// retained whole in the process-lifetime registry. Constructible only in
/// this module.
pub(super) struct StateOpenCustodyParts {
    original: Option<Arc<LiveStateOpen>>,
    runtime: Option<runtime_db::RuntimeDb>,
    state: Option<StateDb>,
}

/// The kind under which a Store opening discarded without settlement is
/// retained for the life of the process: its native handles may still be
/// live, so the databases and the original startup owner stay reachable.
const STATE_STORE_OPENING_KIND: &str = "ryeos-app::state-store-opening";

/// Settled unless the opening owner still holds an acquired database.
fn state_open_custody_settled(parts: &StateOpenCustodyParts) -> bool {
    !(parts.original.is_some() && (parts.runtime.is_some() || parts.state.is_some()))
}
impl StateOpenCustody {
    pub(super) fn new(original: Arc<LiveStateOpen>) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                STATE_STORE_OPENING_KIND,
                StateOpenCustodyParts { original: Some(original), runtime: None, state: None },
                state_open_custody_settled,
            ),
        }
    }
    pub(super) fn original(&self) -> Option<&Arc<LiveStateOpen>> {
        self.inner.original.as_ref()
    }
    pub(super) fn runtime(&self) -> Option<&runtime_db::RuntimeDb> {
        self.inner.runtime.as_ref()
    }
    pub(super) fn state(&self) -> Option<&StateDb> {
        self.inner.state.as_ref()
    }
    pub(super) fn set_runtime(&mut self, runtime: runtime_db::RuntimeDb) {
        self.inner.get_mut().runtime = Some(runtime);
    }
    pub(super) fn set_state(&mut self, state: StateDb) {
        self.inner.get_mut().state = Some(state);
    }
    pub(super) fn take_runtime(&mut self) -> Option<runtime_db::RuntimeDb> {
        self.inner.get_mut().runtime.take()
    }
    pub(super) fn take_state(&mut self) -> Option<StateDb> {
        self.inner.get_mut().state.take()
    }
    pub(super) fn take_original(&mut self) -> Option<Arc<LiveStateOpen>> {
        self.inner.get_mut().original.take()
    }
    pub(super) fn fail(self, error: anyhow::Error) -> anyhow::Error {
        if let Some(original) = self.original() {
            let factory = Arc::clone(&original.factory);
            anyhow::Error::new(LiveStateStoreOpenFailure {
                error,
                factory,
                _custody: Mutex::new(self),
            })
        } else {
            error
        }
    }
}

/// Count of Store openings this process retains because they were discarded
/// without settlement. Any nonzero count means their native custody stays
/// held until the process exits.
pub fn abandoned_state_store_openings() -> usize {
    lillux::custody::abandoned_custody().count(STATE_STORE_OPENING_KIND)
}

pub struct LiveStateStoreOpenFailure {
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
    // Only retention is synchronized. A recovery caller must consume this
    // mutex into its actual checkout before performing SQL/native effects.
    _custody: Mutex<StateOpenCustody>,
}
impl std::fmt::Debug for LiveStateStoreOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStateStoreOpenFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveStateStoreOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "live StateStore partial startup retained: {}",
            self.error
        )
    }
}
impl std::error::Error for LiveStateStoreOpenFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Affine fixed-role admission from the actual live StateStore owner.
/// This is neither a schema receipt nor an opened/qualified scheduler.
pub struct LiveSchedulerNativeAdmission {
    factory: Arc<OriginalNativeFactory>,
    inputs: OriginalNativeBoundInputs,
    reference: lillux_sqlite::OriginalNativeTransientInputs,
}
impl LiveSchedulerNativeAdmission {
    pub fn require_factory(&self, expected: &Arc<OriginalNativeFactory>) -> Result<()> {
        anyhow::ensure!(Arc::ptr_eq(&self.factory, expected),
            "scheduler admission selects another native factory");
        Ok(())
    }
    pub fn into_parts(self) -> (
        Arc<OriginalNativeFactory>, OriginalNativeBoundInputs,
        lillux_sqlite::OriginalNativeTransientInputs,
    ) { (self.factory, self.inputs, self.reference) }
}

/// Fixed-role accounting ledger inputs issued once by the live Store owner.
/// This is neither an opened ledger nor a schema receipt.
pub struct LiveAccountingNativeAdmission {
    factory: Arc<OriginalNativeFactory>,
    directory: lillux::PinnedDirectory,
    established: bool,
    inputs: OriginalNativeBoundInputs,
    reference: lillux_sqlite::OriginalNativeTransientInputs,
}
impl LiveAccountingNativeAdmission {
    pub fn require_factory(&self, expected: &Arc<OriginalNativeFactory>) -> Result<()> {
        anyhow::ensure!(Arc::ptr_eq(&self.factory, expected),
            "accounting admission selects another native factory");
        Ok(())
    }
    pub(crate) fn into_parts(self) -> (
        Arc<OriginalNativeFactory>, lillux::PinnedDirectory, bool, OriginalNativeBoundInputs,
        lillux_sqlite::OriginalNativeTransientInputs,
    ) { (self.factory, self.directory, self.established, self.inputs, self.reference) }
}

struct LiveSchedulerAdmissionAttempt {
    original: Arc<LiveStateOpen>,
    claimed: bool,
    inputs: Option<OriginalNativeBoundInputs>,
    reference: Option<lillux_sqlite::OriginalNativeTransientInputs>,
}
/// Retains the actual one-use claim and any prepared metadata after refusal.
/// It is not a native open failure or ticket/terminal receipt.
pub struct LiveSchedulerNativeAdmissionFailure {
    inner: lillux::custody::Held<LiveSchedulerNativeAdmissionFailureParts>,
}
const SCHEDULER_ADMISSION_FAILURE_KIND: &str = "ryeos-app::scheduler-admission-failure";

/// The refusal and the whole attempt. No drop settles a refused admission:
/// the attempt always holds the original Store alias and any prepared inputs,
/// which only the outside-sources transfer consumes, so it is held until
/// taken. Constructible only in this module.
struct LiveSchedulerNativeAdmissionFailureParts {
    error: anyhow::Error,
    attempt: LiveSchedulerAdmissionAttempt,
}
impl LiveSchedulerNativeAdmissionFailure {
    pub fn claimed(&self) -> bool { self.inner.attempt.claimed }
    pub fn prepared_bound_inputs(&self) -> bool { self.inner.attempt.inputs.is_some() }
    pub fn prepared_reference_inputs(&self) -> bool { self.inner.attempt.reference.is_some() }

    /// Move the exact failed one-use attempt into a retained Store-join frame.
    /// This issues no ticket or retry; any prepared bound/transient inputs and
    /// the original Store alias remain available for post-ACK retirement.
    pub fn into_outside_sources(
        self,
    ) -> std::result::Result<
        (LiveSchedulerNativeAdmissionOutsideSources, anyhow::Error),
        (Self, anyhow::Error),
    > {
        if self.inner.is_foreign()
            || self.inner.attempt.original.factory.require_original_process().is_err()
        {
            return Err((self, anyhow::anyhow!(
                "foreign Scheduler admission failure cannot transfer its original Store owner")));
        }
        let (error, mut attempt) = match self.into_parts() {
            Ok(parts) => parts,
            Err(this) => return Err((this, anyhow::anyhow!(
                "Scheduler admission failure belongs to another process"))),
        };
        attempt.original.close_scheduler_admission();
        let sources = LiveSchedulerNativeAdmissionOutsideSources::new(
            attempt.original, attempt.inputs.take(), attempt.reference.take());
        Ok((sources, error))
    }
}
impl LiveSchedulerNativeAdmissionFailure {
    /// Move the error and the whole attempt out; a copy in another process is
    /// refused and comes back still held.
    fn into_parts(self) -> std::result::Result<(anyhow::Error, LiveSchedulerAdmissionAttempt), Self> {
        self.inner.take_whole()
            .map(|parts| (parts.error, parts.attempt))
            .map_err(|inner| Self { inner })
    }
}
impl std::fmt::Debug for LiveSchedulerNativeAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSchedulerNativeAdmissionFailure")
            .field("error", &self.inner.error).field("claimed", &self.claimed()).finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveSchedulerNativeAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original scheduler admission attempt retained: {}", self.inner.error)
    }
}
impl std::error::Error for LiveSchedulerNativeAdmissionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.inner.error.as_ref()) }
}

/// Failed pre-open Scheduler admission retained until its unused inputs and
/// exact Store-authority alias join the same physical inventory.
pub struct LiveSchedulerNativeAdmissionOutsideSources {
    inner: lillux::custody::Held<SchedulerAdmissionOutsideSourceSlots>,
}
const SCHEDULER_ADMISSION_OUTSIDE_SOURCES_KIND: &str = "ryeos-app::scheduler-admission-outside-sources";

/// Every source slot, in the order their release drops them. Constructible
/// only in this module.
struct SchedulerAdmissionOutsideSourceSlots {
    original: Option<Arc<LiveStateOpen>>,
    bound_inputs: Option<OriginalNativeBoundInputs>,
    transient_inputs: Option<lillux_sqlite::OriginalNativeTransientInputs>,
    unopened_database: Option<super::live_outside::UnopenedSchedulerDatabaseProgress>,
    unopened_reference: Option<super::live_outside::UnopenedNativeTransientProgress>,
    joined: bool,
}
impl SchedulerAdmissionOutsideSourceSlots {
    /// Exhaustion without the process check: joined, no Store alias or unused
    /// input left, and every nested unopened source exhausted.
    fn require_exhausted(&self) -> Result<()> {
        anyhow::ensure!(self.joined && self.original.is_none() && self.bound_inputs.is_none()
            && self.transient_inputs.is_none(),
            "failed Scheduler admission retains a source at exhaustion");
        if let Some(database) = self.unopened_database.as_ref() {
            database.require_exhausted()?;
        }
        if let Some(reference) = self.unopened_reference.as_ref() {
            reference.require_exhausted()?;
        }
        Ok(())
    }
}
/// Settled exactly when exhaustion would accept the sources; otherwise the
/// alias, inputs and nested sources stay retained together until the process
/// exits.
fn scheduler_admission_sources_exhausted(slots: &SchedulerAdmissionOutsideSourceSlots) -> bool {
    slots.require_exhausted().is_ok()
}
impl LiveSchedulerNativeAdmissionOutsideSources {
    fn new(
        original: Arc<LiveStateOpen>,
        bound_inputs: Option<OriginalNativeBoundInputs>,
        transient_inputs: Option<lillux_sqlite::OriginalNativeTransientInputs>,
    ) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                SCHEDULER_ADMISSION_OUTSIDE_SOURCES_KIND,
                SchedulerAdmissionOutsideSourceSlots {
                    original: Some(original), bound_inputs, transient_inputs,
                    unopened_database: None, unopened_reference: None, joined: false,
                },
                scheduler_admission_sources_exhausted,
            ),
        }
    }
}

impl LiveSchedulerNativeAdmissionOutsideSources {
    pub(super) fn advance(
        &mut self,
        expected_store: Option<&Arc<LiveStateOpen>>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(),
            "foreign failed Scheduler admission source");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let parts = self.inner.get_mut();
        if parts.joined {
            anyhow::ensure!(parts.original.is_none() && parts.bound_inputs.is_none()
                && parts.transient_inputs.is_none()
                && parts.unopened_database.as_ref().is_none_or(
                    super::live_outside::UnopenedSchedulerDatabaseProgress::is_joined)
                && parts.unopened_reference.as_ref().is_none_or(
                    super::live_outside::UnopenedNativeTransientProgress::is_joined),
                "joined failed Scheduler admission retains source custody");
            return Ok(());
        }
        let expected_store = expected_store.context(
            "failed Scheduler admission lacks the actual Store owner")?;
        let original = parts.original.as_ref()
            .context("failed Scheduler admission Store alias absent")?;
        anyhow::ensure!(Arc::ptr_eq(original, expected_store),
            "failed Scheduler admission selected another Store owner");

        super::live_outside::advance_unopened_scheduler_database(
            &mut parts.bound_inputs, &mut parts.unopened_database, Some(expected_store),
            factory, aggregate, registry, prepay, None,
        )?;
        super::live_outside::advance_unopened_scheduler_transient(
            &mut parts.transient_inputs, &mut parts.unopened_reference, Some(expected_store),
            factory, aggregate, prepay,
        )?;
        anyhow::ensure!(parts.bound_inputs.is_none() && parts.transient_inputs.is_none()
            && parts.unopened_database.as_ref().is_none_or(
                super::live_outside::UnopenedSchedulerDatabaseProgress::is_joined)
            && parts.unopened_reference.as_ref().is_none_or(
                super::live_outside::UnopenedNativeTransientProgress::is_joined),
            "failed Scheduler admission inputs remain outside the Store inventory");

        registry.prepay_transfer(prepay, 0, 4)?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(Arc::ptr_eq(
            parts.original.as_ref().context("failed Scheduler Store alias disappeared")?,
            expected_store), "failed Scheduler admission Store owner changed during join");
        let original = parts.original.take()
            .context("failed Scheduler admission Store alias disappeared")?;
        drop(original);
        parts.joined = true;
        Ok(())
    }

    pub(super) fn is_joined(&self) -> bool {
        !self.inner.is_foreign() && self.inner.joined
            && self.inner.original.is_none() && self.inner.bound_inputs.is_none()
            && self.inner.transient_inputs.is_none()
            && self.inner.unopened_database.as_ref().is_none_or(
                super::live_outside::UnopenedSchedulerDatabaseProgress::is_joined)
            && self.inner.unopened_reference.as_ref().is_none_or(
                super::live_outside::UnopenedNativeTransientProgress::is_joined)
    }
}

impl LiveSchedulerNativeAdmissionOutsideSources {
    /// Consume the nested unopened Scheduler root registration with its exact
    /// retired-registry receipt. Refusal keeps every source in place.
    pub(super) fn consume_retired(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        anyhow::ensure!(!self.inner.is_foreign() && self.inner.joined
            && self.inner.original.is_none() && self.inner.bound_inputs.is_none()
            && self.inner.transient_inputs.is_none(),
            "failed Scheduler admission did not join the Store inventory");
        if let Some(database) = self.inner.get_mut().unopened_database.as_mut() {
            database.consume_retired(registry)?;
        }
        Ok(())
    }

    pub(super) fn require_exhausted(&self) -> Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(),
            "foreign failed Scheduler admission source at exhaustion");
        self.inner.require_exhausted()
    }

    /// End exhausted sources. They settle on the exhaustion check the
    /// caller already passed, so every slot is released normally.
    pub(super) fn exhaust(self) {
        drop(self);
    }
}
enum StoreStartupStatePhysical {
    Closed(ryeos_state::state_db::ClosedLiveStatePhysicalCustody),
    Failed(ryeos_state::state_db::ReleasedLiveStatePhysicalCustody),
}
impl StoreStartupStatePhysical {
    fn retire_parent_host_aliases(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn OriginalNativeDestinationAuthority>,
        lease: &Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        match self {
            Self::Closed(frame) => frame.retire_parent_host_aliases(factory, aggregate, store, lease, registry, prepay),
            Self::Failed(frame) => frame.retire_parent_host_aliases(factory, aggregate, store, lease, registry, prepay),
        }
    }

    unsafe fn register_parent_roots(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        match self {
            Self::Closed(frame) => unsafe { frame.register_parent_roots(factory, aggregate, registry, prepay) },
            Self::Failed(frame) => unsafe { frame.register_parent_roots(factory, aggregate, registry, prepay) },
        }
    }

    unsafe fn advance_parent(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        match self {
            Self::Closed(frame) => unsafe { frame.advance_parent(factory, aggregate, prepay) },
            Self::Failed(frame) => unsafe { frame.advance_parent(factory, aggregate, prepay) },
        }
    }

    fn require_original(&self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal) -> Result<()> {
        match self {
            Self::Closed(frame) => frame.require_original(factory, aggregate),
            Self::Failed(frame) => frame.require_original(factory, aggregate),
        }
    }
    fn require_operational_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        match self {
            Self::Closed(frame) => frame.require_operational_registered(registry, prepay),
            Self::Failed(frame) => frame.require_operational_registered(registry, prepay),
        }
    }

    fn retire_native_sources(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_metadata_pin_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    ) -> Result<()> {
        match self {
            Self::Closed(frame) => frame.retire_native_sources(
                factory, terminal, registry, prepay, issue_metadata_pin_inputs),
            Self::Failed(frame) => frame.retire_native_sources(
                factory, terminal, registry, prepay, issue_metadata_pin_inputs),
        }
    }

    fn native_sources_retired(&self) -> bool {
        match self {
            Self::Closed(frame) => frame.native_sources_retired(),
            Self::Failed(frame) => frame.native_sources_retired(),
        }
    }

    fn live_parent_completed(&self) -> Result<bool> {
        match self {
            Self::Closed(frame) => frame.live_parent_completed(),
            Self::Failed(frame) => frame.live_parent_completed(),
        }
    }

    /// A failed start has no PinnedStateAuthority, so a closed State frame
    /// ends only its own RecoveryStore aliases. A released frame has none.
    fn retire_recovery_aliases(&mut self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        match self {
            Self::Closed(frame) => frame.retire_recovery_aliases(&mut None, registry, prepay),
            Self::Failed(_) => Ok(()),
        }
    }

    fn recovery_aliases_retired(&self) -> bool {
        match self {
            Self::Closed(frame) => frame.recovery_aliases_retired(),
            Self::Failed(_) => true,
        }
    }

    fn consume_retired_projection_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        match self {
            Self::Closed(frame) => frame.consume_retired_projection_sources(registry, prepay),
            Self::Failed(frame) => frame.consume_retired_projection_sources(registry, prepay),
        }
    }

    fn consume_retired_root_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        match self {
            Self::Closed(frame) => frame.consume_retired_root_sources(registry, prepay),
            Self::Failed(frame) => frame.consume_retired_root_sources(registry, prepay),
        }
    }
}
impl StoreStartupStatePhysical {
    fn into_exhausted(
        self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<(), (Self, anyhow::Error)> {
        match self {
            Self::Closed(frame) => frame.into_exhausted(registry, prepay)
                .map_err(|(frame, error)| (Self::Closed(frame), error)),
            Self::Failed(frame) => frame.into_exhausted(registry, prepay)
                .map_err(|(frame, error)| (Self::Failed(frame), error)),
        }
    }
}
const STORE_STARTUP_CLOSE_PROGRESS_KIND: &str = "ryeos-app::store-startup-close-progress";
const RELEASED_STORE_STARTUP_KIND: &str = "ryeos-app::released-store-startup";

/// Every owner a failed Store start's close holds. It has no destructor of
/// its own: it is always held in a `Held`, which drops it only when final
/// exhaustion would accept it and otherwise retains every parent, packet,
/// Runtime, State, Operational and schema owner together until the process
/// exits. Constructible only in this module.
struct StoreStartupCloseProgress {
    store_destination_join: std::sync::OnceLock<Arc<super::live_destination_join::LiveStoreDestinationJoin>>,
    runtime_packet_parts: [Option<Vec<super::live_outside::OriginalPacketPartsProgress>>; 2],
    operational_packet_parts: [Option<Vec<super::live_outside::OriginalPacketPartsProgress>>; 3],
    scheduler_packet_parts: Option<Vec<super::live_outside::OriginalPacketPartsProgress>>,
    scheduler_unopened_database: Option<super::live_outside::UnopenedSchedulerDatabaseProgress>,
    scheduler_unopened_schema_reference: Option<super::live_outside::UnopenedNativeTransientProgress>,
    runtime_schema_source_parts: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    runtime_schema_unused_inputs: Option<lillux_sqlite::OriginalNativeTransientInputs>,
    runtime_unopened_schema_reference: Option<super::live_outside::UnopenedNativeTransientProgress>,
    runtime_schema_refusal: Option<anyhow::Error>,
    scheduler_admission_sources: Option<LiveSchedulerNativeAdmissionOutsideSources>,
    failed_auxiliary_sources: FailedStartupAuxiliaryReceipt,
    // Closed/failed State and direct pre-State Operational routes retain distinct vectors.
    operational_sql_custody: [Option<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>; 3],
    // Distinct original success/refusal vectors; these slots transfer actual
    // packets without any new payload allocation or native completion claim.
    runtime_sql_custody: [Option<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>; 2],
    physical_registry: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry>,
    original: Option<Arc<LiveStateOpen>>,
    store_parent: Option<LiveStateOpen>,
    store_parent_parts: Option<LiveStorePhysicalParent>,
    store_parent_finalization: Option<LiveStoreParentFinalizationParts>,
    store_parent_finalization_retired: bool,
    runtime: Option<runtime_db::RuntimeDb>,
    runtime_failure: Option<runtime_db::OriginalRuntimeCloseFailure>,
    runtime_closed: Option<runtime_db::OriginalRuntimeClosed>,
    runtime_outside: Option<runtime_db::OriginalLiveRuntimeOutsideCustody>,
    runtime_open_failure: Option<runtime_db::OriginalRuntimeAcquisitionFailure>,
    runtime_released: Option<runtime_db::OriginalRuntimeAcquisitionReleased>,
    runtime_released_outside: Option<runtime_db::OriginalRuntimeAcquisitionOutsideCustody>,
    state: Option<StateDb>,
    state_failure: Option<ryeos_state::state_db::LiveStateCloseFailure>,
    state_closed: Option<ryeos_state::state_db::ClosedLiveState>,
    state_open_failure: Option<ryeos_state::state_db::LiveStateDatabaseOpenFailure>,
    state_recovery_failure: Option<ryeos_state::state_db::LiveStateStartupRecoveryFailure>,
    state_released: Option<ryeos_state::state_db::ReleasedLiveStateStartup>,
    state_physical: Option<StoreStartupStatePhysical>,
    operational_open_failure: Option<ryeos_state::operational::LiveOperationalOpenFailure>,
    operational_recovery_failure: Option<ryeos_state::operational::LiveOperationalStartupRecoveryFailure>,
    operational_released: Option<ryeos_state::operational::ReleasedLiveOperationalStartup>,
    operational_physical: Option<ryeos_state::operational::ReleasedLiveOperationalPhysicalCustody>,
    schema_failure: Option<ryeos_state::sqlite_schema::NativeSchemaSqlReferenceFailure>,
    schema_released: Option<ryeos_state::sqlite_schema::ReleasedNativeSchemaSqlReference>,
    prior_refusal: Option<anyhow::Error>,
}

/// Producer-qualified evidence that the failed-start Store path never built
/// the normal closed-Store auxiliary metadata owner. This is created only by
/// `LiveStateStoreOpenFailure::close_retained`; an absent `Option` or the
/// FailedStartup enum variant is not evidence of this source disposition.
enum FailedStartupAuxiliaryReceipt {
    NotConstructed {
        process: u32,
        retired_registry: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    },
}
impl FailedStartupAuxiliaryReceipt {
    fn not_constructed_at_close_retained() -> Self {
        Self::NotConstructed { process: std::process::id(), retired_registry: None }
    }

    fn consume_retired_registry(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        match self {
            Self::NotConstructed { process, retired_registry } => {
                anyhow::ensure!(!super::live_close::foreign_process(*process),
                    "foreign failed-start Store auxiliary receipt");
                let identity = registry.identity()?;
                if let Some(retired) = retired_registry {
                    return retired.require_same_registry(&identity);
                }
                *retired_registry = Some(identity);
                Ok(())
            }
        }
    }
}
impl StoreStartupCloseProgress {
    fn original(&self) -> Result<&Arc<LiveStateOpen>> {
        self.original.as_ref().context("actual failed Store parent facade already transferred")
    }

    /// Hold this progress under `kind`: settled exactly when final exhaustion
    /// would accept it without a registry.
    fn held(self, kind: &'static str) -> lillux::custody::Held<StoreStartupCloseProgress> {
        lillux::custody::Held::new(kind, self, store_startup_close_progress_exhausted)
    }

    /// The registry-free part of final exhaustion: every owner slot consumed,
    /// the destination receiver completed, the auxiliary receipt consumed,
    /// every packet, source and admission exhausted, and no refusal that
    /// retains an owner. A dropped progress settles on exactly these checks.
    fn require_exhausted_holders(&self) -> Result<()> {
        let p = self;
        anyhow::ensure!(p.original.is_none() && p.store_parent.is_none()
            && p.store_parent_parts.is_none() && p.store_parent_finalization.is_none()
            && p.store_parent_finalization_retired && p.physical_registry.is_none(),
            "failed Store retains a parent or registry source at exhaustion");
        anyhow::ensure!(p.runtime.is_none() && p.runtime_failure.is_none()
            && p.runtime_closed.is_none() && p.runtime_outside.is_none()
            && p.runtime_open_failure.is_none() && p.runtime_released.is_none()
            && p.runtime_released_outside.is_none()
            && p.runtime_schema_source_parts.is_none() && p.runtime_schema_unused_inputs.is_none(),
            "failed Store retains a Runtime owner at exhaustion");
        anyhow::ensure!(p.state.is_none() && p.state_failure.is_none() && p.state_closed.is_none()
            && p.state_open_failure.is_none() && p.state_recovery_failure.is_none()
            && p.state_released.is_none() && p.state_physical.is_none(),
            "failed Store retains a State owner at exhaustion");
        anyhow::ensure!(p.operational_open_failure.is_none()
            && p.operational_recovery_failure.is_none() && p.operational_released.is_none()
            && p.operational_physical.is_none()
            && p.schema_failure.is_none() && p.schema_released.is_none(),
            "failed Store retains an Operational or schema owner at exhaustion");
        p.store_destination_join.get()
            .context("failed Store destination receiver absent at exhaustion")?
            .require_parent_completed_holders()?;
        match &p.failed_auxiliary_sources {
            FailedStartupAuxiliaryReceipt::NotConstructed { process, retired_registry } => {
                anyhow::ensure!(!super::live_close::foreign_process(*process),
                    "foreign failed-start Store auxiliary receipt at exhaustion");
                anyhow::ensure!(retired_registry.is_some(),
                    "failed-start Store auxiliary receipt was not consumed");
            }
        }
        for packets in p.runtime_packet_parts.iter().chain(p.operational_packet_parts.iter())
            .chain(std::iter::once(&p.scheduler_packet_parts)) {
            super::live_outside::require_original_packets_exhausted(packets)?;
        }
        for packets in p.operational_sql_custody.iter().chain(p.runtime_sql_custody.iter()) {
            super::live_outside::require_sql_custody_exhausted(packets)?;
        }
        if let Some(database) = p.scheduler_unopened_database.as_ref() {
            database.require_exhausted()?;
        }
        for reference in [&p.scheduler_unopened_schema_reference, &p.runtime_unopened_schema_reference] {
            if let Some(reference) = reference.as_ref() {
                reference.require_exhausted()?;
            }
        }
        if let Some(admission) = p.scheduler_admission_sources.as_ref() {
            admission.require_exhausted()?;
        }
        ryeos_state::state_db::require_exhausted_refusals(
            p.runtime_schema_refusal.iter().chain(p.prior_refusal.iter()))
    }
}
/// Settled exactly when final exhaustion would accept the progress.
fn store_startup_close_progress_exhausted(progress: &StoreStartupCloseProgress) -> bool {
    progress.require_exhausted_holders().is_ok()
}
pub struct ReleasedLiveStateStoreStartup { progress: lillux::custody::Held<StoreStartupCloseProgress> }
impl ReleasedLiveStateStoreStartup {
    fn new(progress: StoreStartupCloseProgress) -> Self {
        Self { progress: progress.held(RELEASED_STORE_STARTUP_KIND) }
    }
    /// Hand the whole progress on, still held.
    fn into_progress(self) -> lillux::custody::Held<StoreStartupCloseProgress> {
        self.progress
    }
}
impl ReleasedLiveStateStoreStartup {
    pub(super) fn physical_registry_slot(&mut self)
        -> &mut Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry> {
        &mut self.progress.get_mut().physical_registry
    }

    pub(super) fn consume_retired_auxiliary_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        let progress = self.progress.get_mut();
        progress.failed_auxiliary_sources.consume_retired_registry(registry)
    }

    pub(super) fn consume_retired_parent_finalization_parts(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        let progress = self.progress.get_mut();
        if progress.store_parent_finalization_retired {
            anyhow::ensure!(progress.original.is_none()
                && progress.store_parent.is_none()
                && progress.store_parent_parts.is_none()
                && progress.store_parent_finalization.is_none(),
                "consumed failed Store parent regained a retained owner");
            return progress.store_destination_join.get()
                .context("retired failed Store destination receiver absent")?
                .require_parent_completed_in_retired_registry(registry);
        }

        anyhow::ensure!(progress.original.is_none()
            && progress.store_parent.is_none()
            && progress.store_parent_parts.is_none(),
            "retired failed Store parent frame retains an untransferred owner");
        let parts = progress.store_parent_finalization.as_ref()
            .context("retired failed Store parent finalization frame absent")?;
        let join = progress.store_destination_join.get()
            .context("retired failed Store destination receiver absent")?;
        anyhow::ensure!(Arc::ptr_eq(&parts.join, join),
            "retired failed Store parent frame changed its destination receiver");
        parts.require_retired(registry)?;

        progress.store_parent_finalization = None;
        progress.store_parent_finalization_retired = true;
        Ok(())
    }

    /// Retire the selected State frame's generated Bound metadata pins and end
    /// its remaining strong native owners before the region seal. A failed
    /// start without State physical custody has no State connection; its
    /// absence is checked against every State owner.
    pub(super) fn retire_state_native_sources(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_metadata_pin_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    ) -> Result<()> {
        let progress = self.progress.get_mut();
        if let Some(state) = progress.state_physical.as_mut() {
            let registry = progress.physical_registry.as_ref()
                .context("failed Store physical registry absent before State native-source retirement")?;
            return state.retire_native_sources(factory, terminal, registry, prepay, issue_metadata_pin_inputs);
        }
        Self::require_no_state_owner(progress)
    }

    pub(super) fn require_pre_seal_complete(&self) -> Result<()> {
        let p = &*self.progress;
        let registry = p.physical_registry.as_ref()
            .context("failed Store physical registry absent at pre-seal census")?;
        anyhow::ensure!(p.original.is_none() && p.store_parent.is_none()
            && p.store_parent_parts.is_none() && !p.store_parent_finalization_retired,
            "failed Store parent owner remains outside its finalization parts");
        let parts = p.store_parent_finalization.as_ref()
            .context("failed Store parent finalization parts absent at pre-seal census")?;
        anyhow::ensure!(parts.native_authorities_retired && parts.factory.is_none()
            && parts.lease_authority.is_none(),
            "failed Store parent retains native authorities before the region seal");
        let join = p.store_destination_join.get()
            .context("failed Store destination receiver absent at pre-seal census")?;
        anyhow::ensure!(Arc::ptr_eq(&parts.join, join),
            "failed Store parent frame changed its destination receiver");
        join.require_parent_completed_in_registry(registry)?;
        for packets in p.runtime_packet_parts.iter() {
            super::live_outside::require_original_packet_pre_seal_complete(packets,
                super::live_outside::OriginalPacketSourceKind::Runtime)?;
        }
        for packets in p.operational_packet_parts.iter() {
            super::live_outside::require_original_packet_pre_seal_complete(packets,
                super::live_outside::OriginalPacketSourceKind::Operational)?;
        }
        super::live_outside::require_original_packet_pre_seal_complete(&p.scheduler_packet_parts,
            super::live_outside::OriginalPacketSourceKind::Scheduler)?;
        anyhow::ensure!(p.scheduler_unopened_database.as_ref()
            .is_none_or(super::live_outside::UnopenedSchedulerDatabaseProgress::is_joined)
            && [&p.scheduler_unopened_schema_reference, &p.runtime_unopened_schema_reference].into_iter()
                .all(|reference| reference.as_ref()
                    .is_none_or(super::live_outside::UnopenedNativeTransientProgress::is_joined))
            && p.scheduler_admission_sources.as_ref()
                .is_none_or(LiveSchedulerNativeAdmissionOutsideSources::is_joined),
            "failed Store Scheduler/Runtime unopened, reference or admission source is not joined before the region seal");
        self.require_state_native_sources_retired()
    }

    pub(super) fn require_state_native_sources_retired(&self) -> Result<()> {
        if let Some(state) = self.progress.state_physical.as_ref() {
            anyhow::ensure!(state.native_sources_retired(),
                "failed Store seal precedes State native-source retirement");
            anyhow::ensure!(state.live_parent_completed()? && state.recovery_aliases_retired(),
                "failed Store seal precedes the State live-parent or recovery-alias join");
            return Ok(());
        }
        Self::require_no_state_owner(&self.progress)
    }

    fn require_no_state_owner(progress: &StoreStartupCloseProgress) -> Result<()> {
        anyhow::ensure!(progress.state.is_none() && progress.state_failure.is_none()
            && progress.state_closed.is_none() && progress.state_open_failure.is_none()
            && progress.state_recovery_failure.is_none() && progress.state_released.is_none()
            && progress.state_physical.is_none(),
            "State native-source census is absent while a State owner remains");
        Ok(())
    }

    pub(super) fn consume_retired_state_projection_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let progress = self.progress.get_mut();
        if let Some(state) = progress.state_physical.as_mut() {
            return state.consume_retired_projection_sources(registry, prepay);
        }
        anyhow::ensure!(progress.state.is_none() && progress.state_failure.is_none()
            && progress.state_closed.is_none() && progress.state_open_failure.is_none()
            && progress.state_recovery_failure.is_none() && progress.state_released.is_none()
            && progress.state_physical.is_none(),
            "State projection custody is absent while a State owner remains");
        let runtime_owner = progress.runtime_outside.is_some()
            ^ progress.runtime_released_outside.is_some();
        if runtime_owner {
            // Runtime opens before State. Its exact retained outside frame is
            // the selected failed-start witness that State was never acquired;
            // any distinct Operational owners remain independently retained.
            anyhow::ensure!(progress.runtime.is_none() && progress.runtime_failure.is_none()
                && progress.runtime_closed.is_none() && progress.runtime_open_failure.is_none()
                && progress.runtime_released.is_none(),
                "State absence conflicts with an untransferred Runtime owner");
            return Ok(());
        }
        self.require_pre_runtime_failure_without_database_owners()?;
        let _ = registry;
        let _ = prepay;
        Ok(())
    }

    pub(super) fn consume_retired_state_root_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let progress = self.progress.get_mut();
        anyhow::ensure!(progress.runtime_packet_parts.iter().all(|packets|
                super::live_outside::packet_source_parts_consumed(packets.as_ref()))
            && progress.operational_packet_parts.iter().all(|packets|
                super::live_outside::packet_source_parts_consumed(packets.as_ref()))
            && progress.scheduler_packet_parts.as_ref().is_none_or(|packets|
                packets.iter().all(|packet| packet.source_parts_consumed())),
            "failed-start State and Operational roots cannot retire before Store packet source receipts");
        anyhow::ensure!(progress.state_physical.is_none() || progress.operational_physical.is_none(),
            "failed Store has competing State and Operational physical root custodians");
        if let Some(state) = progress.state_physical.as_mut() {
            return state.consume_retired_root_sources(registry, prepay);
        }
        if let Some(operational) = progress.operational_physical.as_mut() {
            anyhow::ensure!(progress.state.is_none() && progress.state_failure.is_none()
                && progress.state_closed.is_none() && progress.state_open_failure.is_none()
                && progress.state_recovery_failure.is_none() && progress.state_released.is_none(),
                "Operational root custody conflicts with a retained State database owner");
            return operational.consume_retired_root_sources(registry, prepay);
        }
        anyhow::ensure!(progress.state.is_none() && progress.state_failure.is_none()
            && progress.state_closed.is_none() && progress.state_open_failure.is_none()
            && progress.state_recovery_failure.is_none() && progress.state_released.is_none()
            && progress.operational_open_failure.is_none()
            && progress.operational_recovery_failure.is_none()
            && progress.operational_released.is_none()
            && progress.operational_sql_custody.iter().all(Option::is_none)
            && progress.operational_packet_parts.iter().all(Option::is_none),
            "State/Operational root custody is absent while a database source remains");
        let runtime_owner = progress.runtime_outside.is_some()
            ^ progress.runtime_released_outside.is_some();
        if runtime_owner {
            anyhow::ensure!(progress.runtime.is_none() && progress.runtime_failure.is_none()
                && progress.runtime_closed.is_none() && progress.runtime_open_failure.is_none()
                && progress.runtime_released.is_none(),
                "State root absence conflicts with an untransferred Runtime owner");
            return Ok(());
        }
        self.require_pre_runtime_failure_without_database_owners()
    }

    pub(super) fn consume_retired_runtime_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let progress = self.progress.get_mut();
        anyhow::ensure!(progress.runtime_outside.is_none()
            || progress.runtime_released_outside.is_none(),
            "failed Store retains competing Runtime outside variants");
        if let Some(runtime) = progress.runtime_outside.as_mut() {
            return runtime.consume_retired_shared_roots(registry, prepay);
        }
        if let Some(runtime) = progress.runtime_released_outside.as_mut() {
            return runtime.consume_retired_shared_roots(registry, prepay);
        }
        self.require_pre_runtime_failure_without_database_owners()
    }

    /// Prove a failed-start path that has no Runtime database owner: Runtime
    /// acquisition refused before creating one. A typed Runtime schema
    /// refusal is accepted only after its terminal and unused-input owners
    /// have joined the Store's packet and exact-authority progress.
    /// StateStore opens Runtime before State, Operational, or Scheduler, and
    /// `LiveStateOpenFailure` preserves an unclassified pre-acquisition error
    /// in `prior_refusal`; a partial Runtime acquisition instead retains its
    /// typed failure and cannot satisfy this absence proof.
    fn require_pre_runtime_failure_without_database_owners(&self) -> Result<()> {
        let progress = &*self.progress;
        anyhow::ensure!(progress.prior_refusal.is_some() || progress.runtime_schema_refusal.is_some(),
            "pre-Runtime failed-start refusal evidence absent");
        anyhow::ensure!(progress.schema_released.is_none(),
            "pre-Runtime refusal retains an unjoined schema-reference owner");
        anyhow::ensure!(progress.runtime.is_none() && progress.runtime_failure.is_none()
            && progress.runtime_closed.is_none() && progress.runtime_outside.is_none()
            && progress.runtime_open_failure.is_none() && progress.runtime_released.is_none()
            && progress.runtime_released_outside.is_none()
            && progress.runtime_schema_source_parts.is_none()
            && progress.runtime_schema_unused_inputs.is_none()
            && progress.runtime_sql_custody[0].is_none()
            && progress.runtime_packet_parts[0].is_none(),
            "pre-Runtime refusal retains a Runtime owner or packet source");
        if progress.runtime_schema_refusal.is_some() {
            anyhow::ensure!(progress.runtime_sql_custody[1].as_ref().is_some_and(|raw| raw.is_empty())
                && progress.runtime_packet_parts[1].is_some(),
                "Runtime schema refusal lacks its typed packet-source marker");
            anyhow::ensure!(progress.runtime_unopened_schema_reference.as_ref()
                .is_none_or(super::live_outside::UnopenedNativeTransientProgress::is_joined),
                "Runtime schema refusal retains an unjoined transient alias");
            anyhow::ensure!(progress.store_parent_parts.is_some()
                || progress.store_parent_finalization.is_some(),
                "Runtime schema refusal was checked before the Store source join");
        } else {
            anyhow::ensure!(progress.runtime_sql_custody[1].is_none()
                && progress.runtime_packet_parts[1].is_none()
                && progress.runtime_unopened_schema_reference.is_none(),
                "untyped pre-Runtime refusal retains a Runtime schema source");
        }
        anyhow::ensure!(progress.state.is_none() && progress.state_failure.is_none()
            && progress.state_closed.is_none() && progress.state_open_failure.is_none()
            && progress.state_recovery_failure.is_none() && progress.state_released.is_none()
            && progress.state_physical.is_none(),
            "pre-Runtime refusal retains a State owner");
        anyhow::ensure!(progress.operational_open_failure.is_none()
            && progress.operational_recovery_failure.is_none()
            && progress.operational_released.is_none()
            && progress.operational_physical.is_none()
            && progress.operational_sql_custody.iter().all(Option::is_none)
            && progress.operational_packet_parts.iter().all(Option::is_none),
            "pre-Runtime refusal retains an Operational owner or packet source");
        anyhow::ensure!(progress.schema_failure.is_none()
            && progress.runtime_schema_unused_inputs.is_none()
            && progress.runtime_unopened_schema_reference.as_ref()
                .is_none_or(super::live_outside::UnopenedNativeTransientProgress::is_joined)
            && progress.scheduler_packet_parts.is_none()
            && progress.scheduler_unopened_database.is_none()
            && progress.scheduler_unopened_schema_reference.is_none()
            && progress.scheduler_admission_sources.is_none(),
            "pre-Runtime refusal retains a schema or Scheduler source");
        Ok(())
    }

    /// Consume the unopened Scheduler root registrations (direct and failed
    /// admission) with their exact close receipts from the retired registry.
    pub(super) fn consume_retired_scheduler_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        let progress = self.progress.get_mut();
        if let Some(database) = progress.scheduler_unopened_database.as_mut() {
            database.consume_retired(registry)?;
        }
        if let Some(admission) = progress.scheduler_admission_sources.as_mut() {
            admission.consume_retired(registry)?;
        }
        Ok(())
    }

    /// Consume the selected State, direct Operational and Runtime outside
    /// frames after their receipt stages. Each refuses intact and is restored.
    pub(super) fn consume_retired_owner_frames(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let progress = self.progress.get_mut();
        if let Some(state) = progress.state_physical.take()
            && let Err((state, error)) = state.into_exhausted(registry, prepay) {
                progress.state_physical = Some(state);
                return Err(error);
            }
        if let Some(operational) = progress.operational_physical.take()
            && let Err((operational, error)) = operational.into_exhausted(registry) {
                progress.operational_physical = Some(operational);
                return Err(error);
            }
        if let Some(runtime) = progress.runtime_outside.take()
            && let Err((runtime, error)) = runtime.into_exhausted() {
                progress.runtime_outside = Some(runtime);
                return Err(error);
            }
        if let Some(runtime) = progress.runtime_released_outside.take()
            && let Err((runtime, error)) = runtime.into_exhausted() {
                progress.runtime_released_outside = Some(runtime);
                return Err(error);
            }
        Ok(())
    }

    fn require_exhausted(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        let p = &*self.progress;
        p.require_exhausted_holders()?;
        p.store_destination_join.get()
            .context("failed Store destination receiver absent at exhaustion")?
            .require_parent_completed_in_retired_registry(registry)?;
        match &p.failed_auxiliary_sources {
            FailedStartupAuxiliaryReceipt::NotConstructed { process: _, retired_registry } => {
                registry.require_same_registry(retired_registry.as_ref()
                    .context("failed-start Store auxiliary receipt was not consumed")?)?;
            }
        }
        Ok(())
    }

    /// Final consumption of the failed-start Store graph. Every owner slot
    /// must already be consumed; only receipts, identities and diagnostics
    /// remain. Refusal returns the graph intact.
    pub(super) fn into_exhausted(
        self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> std::result::Result<super::live_outside::StoreExhausted, (Self, anyhow::Error)> {
        let exhausted = match self.require_exhausted(registry)
            .and_then(|()| super::live_outside::StoreExhausted::new(registry)) {
            Ok(exhausted) => exhausted,
            Err(error) => return Err((self, error)),
        };
        let StoreStartupCloseProgress { store_destination_join, runtime_packet_parts,
            operational_packet_parts, scheduler_packet_parts, scheduler_unopened_database,
            scheduler_unopened_schema_reference, runtime_schema_source_parts,
            runtime_schema_unused_inputs, runtime_unopened_schema_reference, runtime_schema_refusal,
            scheduler_admission_sources, failed_auxiliary_sources, operational_sql_custody,
            runtime_sql_custody, physical_registry, original, store_parent, store_parent_parts,
            store_parent_finalization, store_parent_finalization_retired: _, runtime,
            runtime_failure, runtime_closed, runtime_outside, runtime_open_failure,
            runtime_released, runtime_released_outside, state, state_failure, state_closed,
            state_open_failure, state_recovery_failure, state_released, state_physical,
            operational_open_failure, operational_recovery_failure, operational_released,
            operational_physical, schema_failure, schema_released, prior_refusal }
            = take_held(self.into_progress());
        drop(store_destination_join);
        for packets in runtime_packet_parts.into_iter().chain(operational_packet_parts)
            .chain(std::iter::once(scheduler_packet_parts)) {
            super::live_outside::exhaust_original_packets(packets);
        }
        if let Some(database) = scheduler_unopened_database {
            database.exhaust();
        }
        for reference in [scheduler_unopened_schema_reference, runtime_unopened_schema_reference].into_iter().flatten() {
            reference.exhaust();
        }
        drop(runtime_schema_source_parts);
        drop(runtime_schema_unused_inputs);
        drop(runtime_schema_refusal);
        if let Some(admission) = scheduler_admission_sources {
            admission.exhaust();
        }
        drop(failed_auxiliary_sources);
        for packets in operational_sql_custody.into_iter().chain(runtime_sql_custody) {
            super::live_outside::exhaust_sql_custody(packets);
        }
        drop(physical_registry);
        drop(original);
        drop(store_parent);
        drop(store_parent_parts);
        drop(store_parent_finalization);
        drop(runtime);
        drop(runtime_failure);
        drop(runtime_closed);
        drop(runtime_outside);
        drop(runtime_open_failure);
        drop(runtime_released);
        drop(runtime_released_outside);
        drop(state);
        drop(state_failure);
        drop(state_closed);
        drop(state_open_failure);
        drop(state_recovery_failure);
        drop(state_released);
        drop(state_physical);
        drop(operational_open_failure);
        drop(operational_recovery_failure);
        drop(operational_released);
        drop(operational_physical);
        drop(schema_failure);
        drop(schema_released);
        drop(prior_refusal);
        Ok(exhausted)
    }

    pub(super) fn scheduler_native_store_handoff_receipt<'store>(
        &'store self,
        handoff_id: &ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<ryeos_scheduler::db::SchedulerNativeStoreHandoffReceipt<'store>> {
        let p = &*self.progress;
        super::live_outside::issue_scheduler_store_handoff_receipt(
            self,
            p.scheduler_packet_parts.as_ref(),
            p.scheduler_unopened_database.as_ref(),
            p.scheduler_unopened_schema_reference.as_ref(),
            p.scheduler_admission_sources.as_ref(),
            p.physical_registry.as_ref(), handoff_id, factory, aggregate)
    }
}
impl ReleasedLiveStateStoreStartup {
    /// Transfer the complete actual failed Store body. This preserves custody;
    /// it does not retire descriptors, metadata, controls or account obligations.
    fn advance_store_parent(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if self.progress.store_parent.is_some() || self.progress.store_parent_parts.is_some() {
            anyhow::ensure!(self.progress.original.is_none(), "failed Store retains transferred body and duplicate facade");
            let p = self.progress.get_mut();
            register_store_parent_roots(&mut p.store_parent, &mut p.store_parent_parts,
                p.physical_registry.as_mut().context("actual failed Store physical registry absent")?,
                factory, aggregate, prepay)?;
            return Ok(());
        }
        let p = self.progress.get_mut();
        anyhow::ensure!(p.runtime.is_none() && p.runtime_failure.is_none() && p.runtime_closed.is_none()
            && p.runtime_open_failure.is_none() && p.runtime_released.is_none()
            && p.state.is_none() && p.state_failure.is_none() && p.state_closed.is_none()
            && p.state_open_failure.is_none() && p.state_recovery_failure.is_none() && p.state_released.is_none()
            && p.operational_open_failure.is_none() && p.operational_recovery_failure.is_none()
            && p.operational_released.is_none() && p.schema_failure.is_none() && p.schema_released.is_none()
            && p.runtime_schema_source_parts.is_none() && p.runtime_schema_unused_inputs.is_none()
            && p.runtime_unopened_schema_reference.as_ref()
                .is_none_or(super::live_outside::UnopenedNativeTransientProgress::is_joined),
            "failed Store retains untransferred native or startup source");
        anyhow::ensure!(p.runtime_outside.is_none() || p.runtime_released_outside.is_none(),
            "failed Store retains conflicting Runtime outside sources");
        if p.runtime_schema_refusal.is_some() {
            anyhow::ensure!(p.runtime_sql_custody[1].as_ref().is_some_and(|raw| raw.is_empty())
                && p.runtime_packet_parts[1].is_some(),
                "failed Store Runtime schema refusal lacks its joined packet phase");
        }
        anyhow::ensure!(p.state_physical.is_none() || p.operational_physical.is_none(),
            "failed Store retains conflicting State and direct Operational sources");
        let registry = p.physical_registry.as_ref().context("actual failed Store physical inventory absent")?;
        if let Some(outside) = p.runtime_outside.as_ref() { outside.require_shared_roots_registered(registry, prepay)?; }
        if let Some(outside) = p.runtime_released_outside.as_ref() { outside.require_shared_roots_registered(registry, prepay)?; }
        if let Some(state) = p.state_physical.as_ref() {
            state.require_original(factory, aggregate)?;
            state.require_operational_registered(registry, prepay)?;
        }
        if let Some(operational) = p.operational_physical.as_ref() {
            operational.require_original(factory, aggregate)?;
            operational.require_parent_registered(registry, prepay)?;
        }
        // Genuine pre-acquisition absence is retained as absence; it supplies
        // no native/physical terminal. Every actually selected vector must have
        // its complete original destination handoff before this body moves.
        for (raw, parts) in p.runtime_sql_custody.iter().zip(p.runtime_packet_parts.iter()) {
            match (raw, parts) {
                (None, None) => {},
                (Some(raw), Some(_)) => {
                    anyhow::ensure!(raw.is_empty(), "failed Store retains raw native packets");
                    super::live_outside::require_original_packet_sources_transferred(parts, factory, aggregate,
                        super::live_outside::OriginalPacketSourceKind::Runtime)?;
                },
                _ => anyhow::bail!("failed Store packet provenance lost its matching source"),
            }
        }
        for (raw, parts) in p.operational_sql_custody.iter().zip(p.operational_packet_parts.iter()) {
            match (raw, parts) {
                (None, None) => {},
                (Some(raw), Some(_)) => {
                    anyhow::ensure!(raw.is_empty(), "failed Store retains raw native packets");
                    super::live_outside::require_original_packet_sources_transferred(parts, factory, aggregate,
                        super::live_outside::OriginalPacketSourceKind::Operational)?;
                },
                _ => anyhow::bail!("failed Store packet provenance lost its matching source"),
            }
        }
        if p.scheduler_packet_parts.is_some() {
            super::live_outside::require_original_packet_sources_transferred(&p.scheduler_packet_parts,
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Scheduler)?;
        }
        anyhow::ensure!(p.scheduler_unopened_database.as_ref()
            .is_none_or(super::live_outside::UnopenedSchedulerDatabaseProgress::is_joined),
            "failed-start unopened Scheduler database is not joined to the Store inventory");
        anyhow::ensure!(p.scheduler_unopened_schema_reference.as_ref()
            .is_none_or(super::live_outside::UnopenedNativeTransientProgress::is_joined),
            "failed-start unused Scheduler schema input is not joined to the Store authority");
        anyhow::ensure!(p.scheduler_admission_sources.as_ref()
            .is_none_or(LiveSchedulerNativeAdmissionOutsideSources::is_joined),
            "failed-start Scheduler admission is not joined to the Store inventory");
        let join = p.store_destination_join.get().context("actual failed Store receiver absent")?;
        join.require_original_origin(factory, aggregate)?;
        if let Some(original) = p.original.as_ref() {
            original.require_close_factory(factory)?;
            join.require_original_parent(original, factory, aggregate)?;
            let quote = quote_store_parent_facade_retirement();
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            join.require_original_parent(original, factory, aggregate)?;
            // All child/evidence frames and canonical parent receiver remain
            // cached. End only this duplicate outer parent edge.
            drop(p.original.take());
        }
        join.transfer_original_parent(factory, aggregate, &mut p.store_parent, prepay)?;
        register_store_parent_roots(&mut p.store_parent, &mut p.store_parent_parts,
            p.physical_registry.as_mut().context("actual failed Store physical registry absent")?,
            factory, aggregate, prepay)?;
        Ok(())
    }

    pub(super) fn advance_scheduler_packet_parts(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        handoff: Option<(ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
            ryeos_scheduler::db::SchedulerNativeStoreSourceRole)>,
    ) -> Result<()> {
        let p = self.progress.get_mut();
        super::live_outside::prepare_scheduler_packet_parts(source, &mut p.scheduler_packet_parts,
            factory, aggregate, prepay, handoff)?;
        let registry = p.physical_registry.as_mut()
            .context("actual failed Store physical registry absent for Scheduler source")?;
        super::live_outside::register_original_packet_bound_roots(&mut p.scheduler_packet_parts,
            factory, aggregate, super::live_outside::OriginalPacketSourceKind::Scheduler,
            registry, prepay)
    }

    pub(super) fn advance_unopened_scheduler_database(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeBoundInputs>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        handoff_id: Option<ryeos_scheduler::db::SchedulerNativeStoreHandoffId>,
    ) -> Result<()> {
        let p = self.progress.get_mut();
        let expected = p.original.as_ref();
        let registry = p.physical_registry.as_mut()
            .context("actual failed Store physical registry absent for unopened Scheduler source")?;
        super::live_outside::advance_unopened_scheduler_database(source,
            &mut p.scheduler_unopened_database, expected, factory, aggregate, registry, prepay,
            handoff_id)
    }

    pub(super) fn advance_unopened_scheduler_transient(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeTransientInputs>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let p = self.progress.get_mut();
        let expected = p.original.as_ref();
        super::live_outside::advance_unopened_scheduler_transient(source,
            &mut p.scheduler_unopened_schema_reference, expected, factory, aggregate, prepay)
    }

    pub(super) fn advance_scheduler_admission_sources(
        &mut self,
        source: &mut Option<LiveSchedulerNativeAdmissionOutsideSources>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let p = self.progress.get_mut();
        if source.is_some() && p.scheduler_admission_sources.is_none() {
            let bytes = std::mem::size_of::<LiveSchedulerNativeAdmissionOutsideSources>();
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(u64::try_from(bytes)?, 8))?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            p.scheduler_admission_sources = source.take();
        }
        anyhow::ensure!(source.is_none(),
            "failed Scheduler admission has conflicting Store progress");
        if let Some(progress) = p.scheduler_admission_sources.as_mut() {
            let expected = p.original.as_ref();
            let registry = p.physical_registry.as_mut()
                .context("actual failed Store physical registry absent for Scheduler admission")?;
            progress.advance(expected, factory, aggregate, registry, prepay)?;
        }
        Ok(())
    }

    pub(super) fn consume_retired_packet_destinations(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let StoreStartupCloseProgress { runtime_packet_parts, operational_packet_parts, scheduler_packet_parts, .. } = self.progress.get_mut();
        for packets in runtime_packet_parts.iter_mut() {
            super::live_outside::consume_retired_original_packet_destinations(packets,
                registry, super::live_outside::OriginalPacketSourceKind::Runtime, prepay)?;
        }
        for packets in operational_packet_parts.iter_mut() {
            super::live_outside::consume_retired_original_packet_destinations(packets,
                registry, super::live_outside::OriginalPacketSourceKind::Operational, prepay)?;
        }
        super::live_outside::consume_retired_original_packet_destinations(scheduler_packet_parts,
            registry, super::live_outside::OriginalPacketSourceKind::Scheduler, prepay)?;
        Ok(())
    }

    pub(super) fn consume_retired_packet_bound_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let StoreStartupCloseProgress { runtime_packet_parts, operational_packet_parts, scheduler_packet_parts, .. } = self.progress.get_mut();
        for packets in runtime_packet_parts.iter_mut() {
            super::live_outside::consume_retired_original_packet_bound_roots(packets,
                registry, super::live_outside::OriginalPacketSourceKind::Runtime, prepay)?;
        }
        for packets in operational_packet_parts.iter_mut() {
            super::live_outside::consume_retired_original_packet_bound_roots(packets,
                registry, super::live_outside::OriginalPacketSourceKind::Operational, prepay)?;
        }
        super::live_outside::consume_retired_original_packet_bound_roots(scheduler_packet_parts,
            registry, super::live_outside::OriginalPacketSourceKind::Scheduler, prepay)
    }

    pub(super) fn consume_retired_packet_metadata_pins(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let StoreStartupCloseProgress { runtime_packet_parts, operational_packet_parts, scheduler_packet_parts, .. } = self.progress.get_mut();
        for packets in runtime_packet_parts.iter_mut() {
            super::live_outside::consume_retired_original_packet_metadata_pins(packets,
                super::live_outside::OriginalPacketSourceKind::Runtime, registry, prepay)?;
        }
        for packets in operational_packet_parts.iter_mut() {
            super::live_outside::consume_retired_original_packet_metadata_pins(packets,
                super::live_outside::OriginalPacketSourceKind::Operational, registry, prepay)?;
        }
        super::live_outside::consume_retired_original_packet_metadata_pins(scheduler_packet_parts,
            super::live_outside::OriginalPacketSourceKind::Scheduler, registry, prepay)
    }

    pub(super) fn consume_retired_packet_source_parts(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<u8> {
        let progress = self.progress.get_mut();
        let mut failures = 0u8;
        for packets in progress.runtime_packet_parts.iter_mut() {
            failures = failures.checked_add(
                super::live_outside::consume_retired_original_packet_source_parts(packets,
                    super::live_outside::OriginalPacketSourceKind::Runtime, registry, prepay)?,
            ).context("failed-start Runtime packet failure count overflow")?;
        }
        for packets in progress.operational_packet_parts.iter_mut() {
            failures = failures.checked_add(
                super::live_outside::consume_retired_original_packet_source_parts(packets,
                    super::live_outside::OriginalPacketSourceKind::Operational, registry, prepay)?,
            ).context("failed-start Operational packet failure count overflow")?;
        }
        failures = failures.checked_add(
            super::live_outside::consume_retired_original_packet_source_parts(
                &mut progress.scheduler_packet_parts,
                super::live_outside::OriginalPacketSourceKind::Scheduler, registry, prepay)?,
        ).context("failed-start Scheduler packet failure count overflow")?;
        Ok(failures)
    }

    pub(super) fn complete_store_parent_join(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_metadata_pin_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let p = self.progress.get_mut();
        if p.store_parent_finalization.is_none() {
            anyhow::ensure!(p.store_parent.is_none(),
                "actual failed Store parent body remains outside its physical frame");
            let parent = p.store_parent_parts.take()
                .context("actual failed Store physical parent parts absent at completion")?;
            match parent.into_finalization_parts(factory, aggregate, prepay) {
                Ok(parts) => p.store_parent_finalization = Some(parts),
                Err((parent, error)) => {
                    p.store_parent_parts = Some(parent);
                    return Err(error);
                }
            }
        }
        let StoreStartupCloseProgress {
            store_parent_finalization, physical_registry,
            runtime_packet_parts, operational_packet_parts, scheduler_packet_parts, ..
        } = p;
        let registry = physical_registry.as_mut()
            .context("actual failed Store physical registry absent at parent completion")?;
        let registry_identity = registry.identity()?;
        let parts = store_parent_finalization.as_ref()
            .context("actual failed Store finalization parts were not retained")?;
        if parts.native_authorities_retired {
            parts.join.require_parent_completed_in_registry(registry)?;
        } else {
            parts.join.complete_parent_outside(parts, registry, factory, aggregate, prepay)?;
        }

        for packets in runtime_packet_parts.iter_mut() {
            if packets.is_none() { continue; }
            super::live_outside::register_original_packet_destination_roots(
                packets, super::live_outside::OriginalPacketSourceKind::Runtime, registry, prepay)?;
            super::live_outside::require_original_packet_root_handoffs(packets, factory, aggregate,
                super::live_outside::OriginalPacketSourceKind::Runtime)?;
            super::live_outside::retire_original_packet_native_references(packets,
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Runtime, prepay)?;
            super::live_outside::retire_original_packet_metadata_pins(packets,
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Runtime,
                prepay, &registry_identity, issue_metadata_pin_inputs)?;
        }
        for packets in operational_packet_parts.iter_mut() {
            if packets.is_none() { continue; }
            super::live_outside::register_original_packet_destination_roots(
                packets, super::live_outside::OriginalPacketSourceKind::Operational, registry, prepay)?;
            super::live_outside::require_original_packet_root_handoffs(packets, factory, aggregate,
                super::live_outside::OriginalPacketSourceKind::Operational)?;
            super::live_outside::retire_original_packet_native_references(packets,
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Operational, prepay)?;
            super::live_outside::retire_original_packet_metadata_pins(packets,
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Operational,
                prepay, &registry_identity, issue_metadata_pin_inputs)?;
        }
        if scheduler_packet_parts.is_some() {
            super::live_outside::register_original_packet_destination_roots(
                scheduler_packet_parts, super::live_outside::OriginalPacketSourceKind::Scheduler,
                registry, prepay)?;
            super::live_outside::require_original_packet_root_handoffs(scheduler_packet_parts,
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Scheduler)?;
            super::live_outside::retire_original_packet_native_references(scheduler_packet_parts,
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Scheduler, prepay)?;
            super::live_outside::retire_original_packet_metadata_pins(scheduler_packet_parts,
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Scheduler,
                prepay, &registry_identity, issue_metadata_pin_inputs)?;
        }
        for packets in runtime_packet_parts.iter_mut() {
            if packets.is_none() { continue; }
            super::live_outside::prepare_original_packet_destination_retirements(
                packets, factory, aggregate, super::live_outside::OriginalPacketSourceKind::Runtime,
                registry, prepay)?;
        }
        for packets in operational_packet_parts.iter_mut() {
            if packets.is_none() { continue; }
            super::live_outside::prepare_original_packet_destination_retirements(
                packets, factory, aggregate, super::live_outside::OriginalPacketSourceKind::Operational,
                registry, prepay)?;
        }
        if scheduler_packet_parts.is_some() {
            super::live_outside::prepare_original_packet_destination_retirements(
                scheduler_packet_parts, factory, aggregate,
                super::live_outside::OriginalPacketSourceKind::Scheduler, registry, prepay)?;
        }
        store_parent_finalization.as_mut()
            .context("actual failed Store finalization parts absent before native authority retirement")?
            .retire_native_authorities(registry, factory, aggregate, prepay)
    }

    /// # Safety
    /// The original settled-capacity finalizer calls after durable account
    /// settlement and non-operator descriptor retirement. A genuinely
    /// unpublished Store has no parent alias; retained partial parent state
    /// must refuse instead of being treated as absent.
    pub(super) unsafe fn retire_settled_exclusion_alias(
        &mut self,
        admission_alias: Option<&Arc<ryeos_node_runtime::state_lock::StateLockLease>>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let progress = self.progress.get_mut();
        let Some(parts) = progress.store_parent_finalization.as_mut() else {
            anyhow::ensure!(progress.original.is_none()
                && progress.store_parent.is_none()
                && progress.store_parent_parts.is_none()
                && progress.store_destination_join.get().is_none(),
                "failed Store retains an incomplete parent exclusion source");
            anyhow::ensure!(admission_alias.is_none(),
                "Store lacks the operator membership needed to retire its admission exclusion alias");
            return Ok(());
        };
        let registry = progress.physical_registry.as_ref()
            .context("actual failed Store physical registry absent before exclusion-alias retirement")?;
        unsafe { parts.retire_settled_exclusion_alias(admission_alias, registry, prepay) }
    }

    #[cfg(target_os = "linux")]
    pub(super) unsafe fn register_operator_lock_alias(&mut self,
        owner: &Arc<lillux::ExactExclusiveFileLock>,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        owner.require_original_process()?;
        if let Some(original) = self.progress.original.as_ref() {
            anyhow::ensure!(original.factory.same_original(factory), "actual Store operator lock handoff changed factory");
        } else {
            self.progress.store_destination_join.get().context("actual failed Store receiver absent")?
                .require_original_origin(factory, aggregate)?;
        }
        super::live_outside::require_physical_registry(&self.progress.physical_registry,
            factory, aggregate)?;
        let p = self.progress.get_mut();
        let registry = p.physical_registry.as_mut().context("actual Store physical registry absent")?;
        p.store_parent_parts.as_mut().context("actual failed Store physical parent parts absent")?
            .register_exclusion(owner, factory, aggregate, registry, prepay)?;
        // register_exclusion records this exact canonical owner in the SAME
        // registry. Re-registering the same Arc here billed a duplicate scan
        // on every retry without adding an owner or retirement obligation.
        Ok(())
    }

    /// Register closed/released Runtime holders while the SAME actual Store
    /// parent stays cached. No native leaf or new admission is manufactured.
    pub(super) unsafe fn register_runtime_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if self.progress.original.is_none() {
            return self.advance_store_parent(factory, aggregate, prepay);
        }
        anyhow::ensure!(self.progress.original()?.factory.same_original(factory),
            "failed-start Runtime physical handoff changed actual Store factory");
        super::live_outside::require_physical_registry(&self.progress.physical_registry,
            factory, aggregate)?;
        // A failed acquisition and a completed database are distinct original
        // paths. Do not report a complete handoff while an earlier close frame
        // still awaits normalization, or combine conflicting source owners.
        anyhow::ensure!(self.progress.runtime_closed.is_none()
            && self.progress.runtime_released.is_none(),
            "failed-start Runtime retains an untransferred close frame");
        anyhow::ensure!(self.progress.runtime_outside.is_none()
            || self.progress.runtime_released_outside.is_none(),
            "failed-start Runtime retains conflicting outside sources");
        let StoreStartupCloseProgress { original, state_closed, state_released, state_physical,
            runtime_outside, runtime_released_outside, runtime_sql_custody, operational_sql_custody, operational_released, operational_physical, runtime_packet_parts, operational_packet_parts, physical_registry, .. } = self.progress.get_mut();
        let registry = physical_registry.as_mut().context("actual failed-start physical registry absent")?;
        anyhow::ensure!(runtime_sql_custody.iter().filter(|slot| slot.is_some()).count() <= 1
            && operational_sql_custody.iter().filter(|slot| slot.is_some()).count() <= 1,
            "failed Store packet provenance exceeds one Runtime and one Operational producer collection");
        let original = original.as_ref().context("actual failed Store parent facade absent")?;
        if let Some(outside) = runtime_outside.as_mut()
            && runtime_sql_custody[0].is_none() {
                let quote = quote_store_packet_vector_handoff();
                registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
                let packets = unsafe { outside.take_original_sql_custody() }?;
                runtime_sql_custody[0] = Some(packets);
            }
        if let Some(outside) = runtime_released_outside.as_mut()
            && runtime_sql_custody[1].is_none() {
                let quote = quote_store_packet_vector_handoff();
                registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
                let packets = unsafe { outside.take_original_sql_custody() }?;
                runtime_sql_custody[1] = Some(packets);
            }
        for (raw, parts) in runtime_sql_custody.iter_mut().zip(runtime_packet_parts.iter_mut()) {
            super::live_outside::prepare_original_packet_parts(raw, parts, factory, aggregate,
                super::live_outside::OriginalPacketSourceKind::Runtime, prepay)?;
            super::live_outside::register_original_packet_bound_roots(parts, factory, aggregate,
                super::live_outside::OriginalPacketSourceKind::Runtime, registry, prepay)?;
        }
        anyhow::ensure!(state_closed.is_none() || state_released.is_none(),
            "failed-start Store retains conflicting State source frames");
        anyhow::ensure!(state_physical.is_none() || (state_closed.is_none() && state_released.is_none()),
            "failed-start Store retains a physical and untransferred State frame");
        // This same allocation also binds the direct pre-State Operational
        // source; sharing a native factory alone cannot substitute for it.
        let quote = quote_store_parent_authority_handoff();
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        original.require_close_factory(factory)?;
        let parent: Arc<dyn OriginalNativeDestinationAuthority> = original.clone();
        anyhow::ensure!(operational_physical.is_none() || operational_released.is_none(),
            "direct Operational retains physical and untransferred source frames");
        anyhow::ensure!((operational_released.is_none() && operational_physical.is_none())
            || (state_closed.is_none() && state_released.is_none() && state_physical.is_none()),
            "direct Operational physical custody conflicts with State-owned custody");
        if let Some(operational) = operational_released.as_mut() {
            anyhow::ensure!(state_closed.is_none() && state_released.is_none() && state_physical.is_none(),
                "direct Operational startup source conflicts with State-owned custody");
            if operational_sql_custody[2].is_none() {
                let quote = quote_store_packet_vector_handoff();
                registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
                let packets = unsafe { operational.take_original_sql_custody(factory, aggregate, &parent) }?;
                operational_sql_custody[2] = Some(packets);
            }
            super::live_outside::prepare_original_packet_parts(&mut operational_sql_custody[2],
                &mut operational_packet_parts[2], factory, aggregate,
                super::live_outside::OriginalPacketSourceKind::Operational, prepay)?;
            super::live_outside::register_original_packet_bound_roots(&mut operational_packet_parts[2],
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Operational, registry, prepay)?;
        }
        if let Some(state) = state_physical.as_ref() {
            state.require_original(factory, aggregate)?;
            state.require_operational_registered(registry, prepay)?;
        }
        if let Some(state) = state_closed.as_mut() {
            if operational_sql_custody[0].is_none() {
                let quote = quote_store_packet_vector_handoff();
                registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
                let packets = unsafe { state.take_operational_sql_custody(factory, aggregate) }?;
                operational_sql_custody[0] = Some(packets);
            }
            super::live_outside::prepare_original_packet_parts(&mut operational_sql_custody[0],
                &mut operational_packet_parts[0], factory, aggregate,
                super::live_outside::OriginalPacketSourceKind::Operational, prepay)?;
            super::live_outside::register_original_packet_bound_roots(&mut operational_packet_parts[0],
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Operational, registry, prepay)?;
            unsafe { state.register_operational_shared_roots(factory, aggregate, registry, prepay) }?;
        }
        if let Some(state) = state_released.as_mut() {
            if operational_sql_custody[1].is_none() {
                let quote = quote_store_packet_vector_handoff();
                registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
                if let Some(packets) = unsafe { state.take_operational_sql_custody(factory, aggregate) }? {
                    operational_sql_custody[1] = Some(packets);
                }
            }
            super::live_outside::prepare_original_packet_parts(&mut operational_sql_custody[1],
                &mut operational_packet_parts[1], factory, aggregate,
                super::live_outside::OriginalPacketSourceKind::Operational, prepay)?;
            super::live_outside::register_original_packet_bound_roots(&mut operational_packet_parts[1],
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Operational, registry, prepay)?;
            unsafe { state.register_operational_shared_roots(factory, aggregate, registry, prepay) }?;
        }

        if let Some(outside) = runtime_outside {
            // SAFETY: real same-factory aggregate and cached original Store.
            unsafe { outside.register_shared_roots(factory, aggregate, registry, prepay) }?;
            outside.require_shared_roots_registered(registry, prepay)?;
        }
        if let Some(outside) = runtime_released_outside {
            // SAFETY: `original` retains the SAME authority allocation through
            // every alias/root transfer; partial outcomes remain in outside.
            unsafe { outside.retire_unused_bound_input_aliases(factory, aggregate, &parent, registry, prepay) }?;
            outside.require_shared_roots_registered(registry, prepay)?;
        }
        // Cache the entire selected State frame before ending its redundant
        // parent edges. A partial refusal restores the original source exactly.
        if state_physical.is_none() {
            if let Some(source) = state_closed.take() {
                match source.into_physical_custody(factory, aggregate, registry, prepay) {
                    Ok(frame) => *state_physical = Some(StoreStartupStatePhysical::Closed(frame)),
                    Err((source, error)) => { *state_closed = Some(source); return Err(error); }
                }
            } else if let Some(source) = state_released.take() {
                match source.into_physical_custody(factory, aggregate, registry, prepay) {
                    Ok(frame) => *state_physical = Some(StoreStartupStatePhysical::Failed(frame)),
                    Err((source, error)) => { *state_released = Some(source); return Err(error); }
                }
            }
        }
        if operational_physical.is_none() && operational_released.is_some() {
            super::live_outside::require_original_packet_sources_transferred(&operational_packet_parts[2],
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Operational)?;
            anyhow::ensure!(operational_sql_custody[2].as_ref().is_some_and(|packets| packets.is_empty()),
                "actual direct Operational packet vector not transferred");
            let source = operational_released.take().expect("checked actual Operational source");
            match source.into_physical_custody(factory, aggregate, &parent, prepay) {
                Ok(frame) => *operational_physical = Some(frame),
                Err((source, error)) => { *operational_released = Some(source); return Err(error); }
            }
        }
        if let Some(frame) = operational_physical.as_mut() {
            unsafe { frame.advance_parent(factory, aggregate, prepay) }?;
            unsafe { frame.register_parent_roots(factory, aggregate, registry, prepay) }?;
            let quote = quote_store_parent_alias_retirement();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            original.require_close_factory(factory)?;
            let lease: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> = original.clone();
            frame.retire_parent_host_aliases(factory, aggregate, &parent, &lease, registry, prepay)?;
        }
        if let Some(frame) = state_physical.as_mut() {
            // Complete selected frame remains cached after its upper aliases
            // end. Actual admitted source handoffs precede this continuation;
            // the same receiver validates leaf completion and parent uniqueness.
            unsafe { frame.advance_parent(factory, aggregate, prepay) }?;
            unsafe { frame.register_parent_roots(factory, aggregate, registry, prepay) }?;
            let quote = quote_store_parent_alias_retirement();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            original.require_close_factory(factory)?;
            let lease: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> = original.clone();
            frame.retire_parent_host_aliases(factory, aggregate, &parent, &lease, registry, prepay)?;
            frame.retire_recovery_aliases(registry, prepay)?;
        }
        drop(parent);
        self.advance_store_parent(factory, aggregate, prepay)
    }
    /// Convert a released Runtime schema refusal into this Store's Runtime
    /// packet and unused-input progress. The typed diagnostic remains in the
    /// frame; neither route becomes Runtime acquisition or schema admission.
    fn advance_runtime_schema_reference(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if self.progress.schema_released.is_some() {
            anyhow::ensure!(self.progress.runtime_schema_refusal.is_none()
                && self.progress.runtime_schema_source_parts.is_none()
                && self.progress.runtime_schema_unused_inputs.is_none(),
                "Runtime schema refusal source conflicts with an earlier transferred phase");
            anyhow::ensure!(self.progress.runtime.is_none()
                && self.progress.runtime_failure.is_none()
                && self.progress.runtime_closed.is_none()
                && self.progress.runtime_open_failure.is_none()
                && self.progress.runtime_released.is_none()
                && self.progress.runtime_outside.is_none()
                && self.progress.runtime_released_outside.is_none()
                && self.progress.runtime_sql_custody.iter().all(Option::is_none)
                && self.progress.runtime_packet_parts.iter().all(Option::is_none),
                "Runtime schema-reference failure conflicts with another Runtime source");
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| prepay(0, 4))?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            let source = self.progress.get_mut().schema_released.take()
                .expect("checked retained Runtime schema failure");
            match source.into_outside_parts(factory, aggregate) {
                Ok((parts, unused_inputs, refusal)) => {
                    self.progress.get_mut().runtime_schema_source_parts = parts;
                    self.progress.get_mut().runtime_schema_unused_inputs = unused_inputs
                        .map(ryeos_state::sqlite_schema::UnusedSchemaTransientInputs::take_or_abort);
                    self.progress.get_mut().runtime_schema_refusal = Some(refusal);
                }
                Err((source, error)) => {
                    self.progress.get_mut().schema_released = Some(source);
                    return Err(error);
                }
            }
        }

        if self.progress.runtime_schema_source_parts.is_some() {
            let StoreStartupCloseProgress { runtime_schema_source_parts, runtime_sql_custody,
                runtime_packet_parts, .. } = self.progress.get_mut();
            super::live_outside::prepare_original_packet_parts_from_owner(
                runtime_schema_source_parts, &mut runtime_sql_custody[1],
                &mut runtime_packet_parts[1], OriginalNativeDomain::NodeRuntimeSchemaReference,
                factory, aggregate, super::live_outside::OriginalPacketSourceKind::Runtime, prepay)?;
        }

        if self.progress.runtime_schema_refusal.is_some() {
            let StoreStartupCloseProgress { runtime_sql_custody, runtime_packet_parts, .. } =
                self.progress.get_mut();
            let (raw, parts) = (&mut runtime_sql_custody[1], &mut runtime_packet_parts[1]);
            anyhow::ensure!(raw.is_none() == parts.is_none(),
                "Runtime schema refusal packet marker lost its paired phase");
            if raw.is_none() {
                let quote = super::live_outside::quote_packet_parts(0, 2)?;
                lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                    prepay(quote.bytes, quote.steps))?;
                factory.require_original_process()?;
                aggregate.require_original(factory)?;
                *raw = Some(Vec::new());
                *parts = Some(Vec::new());
            }
            let StoreStartupCloseProgress { runtime_sql_custody, runtime_packet_parts, .. } = self.progress.get_mut();
            super::live_outside::prepare_original_packet_parts(&mut runtime_sql_custody[1],
                &mut runtime_packet_parts[1], factory, aggregate,
                super::live_outside::OriginalPacketSourceKind::Runtime, prepay)?;
            let StoreStartupCloseProgress { runtime_schema_unused_inputs,
                runtime_unopened_schema_reference, original, .. } = self.progress.get_mut();
            super::live_outside::advance_unopened_native_transient(
                runtime_schema_unused_inputs, runtime_unopened_schema_reference,
                OriginalNativeDomain::NodeRuntimeSchemaReference, original.as_ref(),
                factory, aggregate, prepay)?;
            anyhow::ensure!(runtime_schema_unused_inputs.is_none(),
                "Runtime schema failure retains an unused transient source");
        }
        Ok(())
    }

    /// Retain normal-close or released-acquisition Runtime custody on the
    /// actual unpublished Store. Absence before acquisition supplies no success.
    fn advance_runtime_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(self.progress.original()?.factory.same_original(factory),
            "failed-start Runtime handoff changed actual Store factory");
        self.advance_runtime_schema_reference(factory, aggregate, prepay)?;
        let StoreStartupCloseProgress { runtime_closed, runtime_outside,
            runtime_released, runtime_released_outside, .. } = self.progress.get_mut();
        anyhow::ensure!(!(runtime_closed.is_some() || runtime_outside.is_some())
            || !(runtime_released.is_some() || runtime_released_outside.is_some()),
            "failed-start Store retains conflicting original Runtime sources");
        if runtime_outside.is_none()
            && let Some(source) = runtime_closed.take() {
                match source.into_live_outside_custody(factory, aggregate, prepay) {
                    Ok(outside) => *runtime_outside = Some(outside),
                    Err((source, error)) => { *runtime_closed = Some(source); return Err(error); }
                }
            }
        if let Some(outside) = runtime_outside {
            outside.advance_native_sources(factory, aggregate, prepay)?;
        }
        if runtime_released_outside.is_none()
            && let Some(source) = runtime_released.take() {
                match source.into_live_outside_custody(factory, aggregate, prepay) {
                    Ok(outside) => *runtime_released_outside = Some(outside),
                    Err((source, error)) => { *runtime_released = Some(source); return Err(error); }
                }
            }
        if let Some(outside) = runtime_released_outside {
            outside.advance_native_sources(factory, aggregate, prepay)?;
        }
        Ok(())
    }
    pub(super) fn advance_state_projection_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        if self.progress.original.is_none() {
            return self.progress.store_destination_join.get().context("actual failed Store receiver absent")?
                .require_original_origin(factory, terminal);
        }
        anyhow::ensure!(self.progress.original()?.factory.same_original(factory),
            "failed-start State source handoff changed actual Store factory");
        // Check the whole original State selection BEFORE either path performs
        // native or physical transfers. A later registry check cannot undo a
        // partial transfer from two conflicting source frames.
        anyhow::ensure!(self.progress.state_closed.is_none()
            || self.progress.state_released.is_none(),
            "failed-start Store retains conflicting State source frames");
        if let Some(frame) = self.progress.operational_physical.as_ref() {
            anyhow::ensure!(self.progress.operational_released.is_none()
                && self.progress.state_closed.is_none() && self.progress.state_released.is_none()
                && self.progress.state_physical.is_none(),
                "direct Operational physical custody conflicts with another source");
            frame.require_original(factory, terminal)?;
            return self.advance_runtime_sources(factory, terminal, prepay);
        }
        if self.progress.operational_released.is_some() {
            anyhow::ensure!(self.progress.state_closed.is_none()
                && self.progress.state_released.is_none() && self.progress.state_physical.is_none(),
                "direct Operational startup source conflicts with State-owned custody");
            let quote = quote_store_direct_operational_handoff();
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            factory.require_original_process()?;
            terminal.require_original(factory)?;
            let parent: Arc<dyn OriginalNativeDestinationAuthority> = self.progress.original()?.clone();
            let operational = self.progress.get_mut().operational_released.as_mut().expect("checked actual Operational startup source");
            operational.advance_native_sources(factory, terminal, &parent, prepay)?;
        }
        if let Some(frame) = self.progress.state_physical.as_ref() {
            frame.require_original(factory, terminal)?;
            return self.advance_runtime_sources(factory, terminal, prepay);
        }
        // A failed Store may contain an actually closed State. An unpublished
        // State's released-startup graph remains separately retained here.
        if let Some(state) = &mut self.progress.get_mut().state_closed {
            state.advance_projection_sources(factory, terminal, prepay)?;
            state.advance_schema_sources(factory, terminal, prepay)?;
            state.advance_destination_input_sources(factory, terminal, prepay)?;
            state.advance_operational_outside(factory, terminal, prepay)?;
        }
        if let Some(state) = &mut self.progress.get_mut().state_released {
            state.advance_projection_sources(factory, terminal, prepay)?;
            state.advance_schema_sources(factory, terminal, prepay)?;
            state.advance_destination_input_sources(factory, terminal, prepay)?;
            state.advance_operational_outside(factory, terminal, prepay)?;
        }
        self.advance_runtime_sources(factory, terminal, prepay)
    }
    pub(super) fn prepare_destination_join(&self,
        factory: &Arc<OriginalNativeFactory>, terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        terminal.require_original(factory)?;
        if self.progress.original.is_none() {
            return self.progress.store_destination_join.get().context("actual failed Store receiver absent")?
                .require_original_origin(factory, terminal);
        }
        anyhow::ensure!(self.progress.original()?.factory.same_original(factory), "failed-start Store selected foreign factory");
        super::live_destination_join::prepare_store_destination_join(&self.progress.store_destination_join,
            self.progress.original()?, factory, terminal, prepay)?;
        anyhow::ensure!(self.progress.operational_released.is_none()
            || (self.progress.state_closed.is_none() && self.progress.state_released.is_none()
                && self.progress.state_physical.is_none()),
            "direct Operational parent receiver conflicts with State-owned custody");
        if let Some(frame) = self.progress.operational_physical.as_ref() {
            anyhow::ensure!(self.progress.operational_released.is_none()
                && self.progress.state_closed.is_none() && self.progress.state_released.is_none()
                && self.progress.state_physical.is_none(),
                "direct Operational receiver conflicts with another source");
            return frame.require_original(factory, terminal);
        }
        if let Some(frame) = self.progress.state_physical.as_ref() {
            return frame.require_original(factory, terminal);
        }
        let quote = quote_store_destination_join_preparation();
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        let parent: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> = self.progress.original()?.clone();
        if let Some(operational) = &self.progress.operational_released {
            operational.prepare_destination_join(factory, terminal, &parent, prepay)?;
        }
        if let Some(state) = &self.progress.state_closed {
            state.require_original_store_parent(&parent, factory)?;
            state.prepare_destination_join(factory, terminal, prepay)?;
        }
        if let Some(state) = &self.progress.state_released {
            state.require_original_store_parent(&parent, factory)?;
            state.prepare_destination_join(factory, terminal, prepay)?;
        }
        Ok(())
    }
    /// Transfer the SAME original live graph only after genuine same-factory
    /// native acknowledgement. All State, Runtime, authority and physical
    /// holders remain owned; this is not descriptor retirement or settlement.
    pub fn into_outside_custody(self, terminal: &lillux_sqlite::OriginalNativeTerminal)
        -> std::result::Result<super::LiveStateStoreOutsideCustody, Self> {
        let Some(original) = self.progress.original.as_ref() else { return Err(self); };
        if terminal.require_original(&original.factory).is_err() { return Err(self); }
        Ok(super::live_outside::LiveStateStoreOutsideCustody::failed_startup(self))
    }

    /// # Safety
    /// The original supervisor must prove no Store/service or generation
    /// consumer escaped this failed startup. All actual SQL owners must have
    /// completed cleanup; keep this holder on each refusal.
    pub unsafe fn generation_lease_terminals(&self, factory: &Arc<OriginalNativeFactory>)
        -> Result<Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>> {
        self.native_terminals(factory)?;
        let mut receipts = Vec::new();
        if let Some(closed) = &self.progress.state_closed {
            receipts.extend(closed.generation_lease_terminals()?);
        }
        if let Some(released) = &self.progress.state_released {
            // SAFETY: this is the same actual unpublished startup graph;
            // the supervisor retains the real quiescence obligation above.
            for receipt in unsafe { released.generation_lease_terminals(factory) }? {
                if !receipts.iter().any(|actual| Arc::ptr_eq(actual, &receipt)) { receipts.push(receipt); }
            }
        }
        Ok(receipts)
    }
    pub fn native_terminals(&self, factory: &Arc<OriginalNativeFactory>)
        -> Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>> {
        factory.require_original_process()?;
        anyhow::ensure!(self.progress.runtime_outside.is_none() && self.progress.runtime_released_outside.is_none(),
            "Runtime source already transferred after aggregate acknowledgement");
        self.progress.original()?.require_close_factory(factory)?;
        let mut terminals = Vec::new();
        if let Some(closed) = &self.progress.runtime_closed { terminals.extend(closed.native_terminals()?); }
        if let Some(released) = &self.progress.runtime_released { terminals.extend(released.native_terminals(factory)?); }
        if let Some(closed) = &self.progress.state_closed { terminals.extend(closed.native_terminals()?); }
        if let Some(released) = &self.progress.state_released { terminals.extend(released.native_terminals(factory)?); }
        if let Some(released) = &self.progress.operational_released { terminals.extend(released.native_terminals(factory)?); }
        if let Some(released) = &self.progress.schema_released
            && let Some(terminal) = released.terminal(factory)? { terminals.push(terminal.clone()); }
        for terminal in &terminals { terminal.require_original(factory)?; }
        Ok(terminals)
    }
    pub fn prior_refusal(&self) -> Option<&anyhow::Error> { self.progress.prior_refusal.as_ref() }

    /// End a released failed start whose sources never left it, in process,
    /// yielding its native census: every connection terminal and closed
    /// generation lease, for the in-process host to record before it finishes
    /// its backend. The opening roots end with this call. A start that began
    /// any outside handoff is refused whole; a refused part goes back into
    /// the retained close progress for [`LiveStateStoreStartupRecoveryFailure::retry`].
    pub fn into_census(self, factory: Arc<OriginalNativeFactory>)
        -> std::result::Result<super::StateStoreCensus, LiveStateStoreStartupRecoveryFailure> {
        let retain = |progress: lillux::custody::Held<StoreStartupCloseProgress>, refusal: anyhow::Error| {
            LiveStateStoreStartupRecoveryFailure::new(StoreStartupRecoveryCustody::Closing {
                factory: Arc::clone(&factory),
                progress: Mutex::new(progress),
                refusal,
            })
        };
        // A forked child holds only a copy of this progress; it is refused here,
        // before any `Held` exists that a later refusal would have to take back.
        if let Err(refusal) = factory.require_original_process() {
            return Err(LiveStateStoreStartupRecoveryFailure::new(StoreStartupRecoveryCustody::Closing {
                factory: Arc::clone(&factory),
                progress: Mutex::new(self.into_progress()),
                refusal,
            }));
        }
        let mut progress = self.into_progress();
        let checked = (|| -> Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>> {
            factory.require_original_process()?;
            progress.original()?.require_close_factory(&factory)?;
            let p = &*progress;
            anyhow::ensure!(p.store_destination_join.get().is_none()
                && p.runtime_packet_parts.iter().all(Option::is_none)
                && p.operational_packet_parts.iter().all(Option::is_none)
                && p.scheduler_packet_parts.is_none() && p.scheduler_unopened_database.is_none()
                && p.scheduler_unopened_schema_reference.is_none()
                && p.runtime_schema_source_parts.is_none() && p.runtime_schema_unused_inputs.is_none()
                && p.runtime_unopened_schema_reference.is_none() && p.scheduler_admission_sources.is_none()
                && p.operational_sql_custody.iter().all(Option::is_none)
                && p.runtime_sql_custody.iter().all(Option::is_none)
                && p.physical_registry.is_none() && p.store_parent.is_none() && p.store_parent_parts.is_none()
                && p.store_parent_finalization.is_none() && !p.store_parent_finalization_retired
                && p.runtime.is_none() && p.runtime_failure.is_none() && p.runtime_outside.is_none()
                && p.runtime_open_failure.is_none() && p.runtime_released_outside.is_none()
                && p.state.is_none() && p.state_failure.is_none() && p.state_open_failure.is_none()
                && p.state_recovery_failure.is_none() && p.state_physical.is_none()
                && p.operational_open_failure.is_none() && p.operational_recovery_failure.is_none()
                && p.operational_physical.is_none() && p.schema_failure.is_none(),
                "released Store startup began an outside source handoff");
            let mut terminals = Vec::new();
            if let Some(closed) = &p.runtime_closed { terminals.extend(closed.native_terminals()?); }
            if let Some(released) = &p.runtime_released { terminals.extend(released.native_terminals(&factory)?); }
            if let Some(released) = &p.schema_released
                && let Some(terminal) = released.terminal(&factory)? { terminals.push(terminal.clone()); }
            for terminal in &terminals { terminal.require_original(&factory)?; }
            Ok(terminals)
        })();
        let mut terminals = match checked {
            Ok(terminals) => terminals,
            Err(error) => return Err(retain(progress, error)),
        };
        let mut leases = Vec::new();
        let mut lease_retirements = Vec::new();
        let mut prior_refusals = Vec::new();
        if let Some(closed) = progress.get_mut().state_closed.take() {
            match closed.into_census(Arc::clone(&factory)) {
                Ok(census) => {
                    let ryeos_state::state_db::LiveStateCensus { terminals: state, leases: state_leases,
                        lease_retirement, prior_refusal } = census;
                    terminals.extend(state);
                    leases.extend(state_leases);
                    lease_retirements.push(lease_retirement);
                    prior_refusals.extend(prior_refusal);
                }
                Err(failure) => {
                    progress.get_mut().state_failure = Some(failure);
                    return Err(retain(progress, anyhow::anyhow!(
                        "closed State refused its census; its close remains pending")));
                }
            }
        }
        if let Some(released) = progress.get_mut().state_released.take() {
            match released.into_census(&factory) {
                Ok(census) => {
                    let ryeos_state::state_db::LiveStateCensus { terminals: state, leases: state_leases,
                        lease_retirement, prior_refusal } = census;
                    terminals.extend(state);
                    leases.extend(state_leases);
                    lease_retirements.push(lease_retirement);
                    prior_refusals.extend(prior_refusal);
                }
                Err((released, error)) => {
                    progress.get_mut().state_released = Some(released);
                    return Err(retain(progress, error));
                }
            }
        }
        if let Some(released) = progress.get_mut().operational_released.take() {
            match released.into_census(&factory) {
                Ok(operational) => terminals.extend(operational),
                Err((released, error)) => {
                    progress.get_mut().operational_released = Some(released);
                    return Err(retain(progress, error));
                }
            }
        }
        // A schema reference refused before any ticket holds only unused
        // one-shot inputs; they end in their issuing process here.
        if let Some(released) = progress.get_mut().schema_released.take() {
            let has_terminal = released.terminal(&factory).map(|terminal| terminal.is_some());
            match has_terminal {
                Ok(false) => match released.release_unused(&factory) {
                    Ok(prior) => prior_refusals.push(prior),
                    Err((released, error)) => {
                        progress.get_mut().schema_released = Some(released);
                        return Err(retain(progress, error));
                    }
                },
                Ok(true) => progress.get_mut().schema_released = Some(released),
                Err(error) => {
                    progress.get_mut().schema_released = Some(released);
                    return Err(retain(progress, error));
                }
            }
        }
        let StoreStartupCloseProgress { store_destination_join, runtime_packet_parts, operational_packet_parts,
            scheduler_packet_parts, scheduler_unopened_database, scheduler_unopened_schema_reference,
            runtime_schema_source_parts, runtime_schema_unused_inputs, runtime_unopened_schema_reference,
            runtime_schema_refusal, scheduler_admission_sources, failed_auxiliary_sources,
            operational_sql_custody, runtime_sql_custody, physical_registry, original, store_parent,
            store_parent_parts, store_parent_finalization, store_parent_finalization_retired: _,
            runtime, runtime_failure, runtime_closed, runtime_outside, runtime_open_failure,
            runtime_released, runtime_released_outside, state, state_failure, state_closed,
            state_open_failure, state_recovery_failure, state_released, state_physical,
            operational_open_failure, operational_recovery_failure, operational_released,
            operational_physical, schema_failure, schema_released, prior_refusal }
            = take_held(progress);
        // Every outside slot was checked empty above and every native owner's
        // terminal is in the census; the facades and opening roots end here.
        drop(store_parent);
        drop(store_parent_parts);
        drop((store_destination_join, runtime_packet_parts, operational_packet_parts, scheduler_packet_parts,
            scheduler_unopened_database, scheduler_unopened_schema_reference, runtime_schema_source_parts,
            runtime_schema_unused_inputs, runtime_unopened_schema_reference, scheduler_admission_sources,
            failed_auxiliary_sources, operational_sql_custody, runtime_sql_custody, physical_registry,
            store_parent_finalization));
        drop((runtime, runtime_failure, runtime_closed, runtime_outside, runtime_open_failure,
            runtime_released, runtime_released_outside));
        drop((state, state_failure, state_closed, state_open_failure, state_recovery_failure,
            state_released, state_physical, operational_open_failure, operational_recovery_failure,
            operational_released, operational_physical, schema_failure, schema_released));
        drop(original);
        let prior_refusal = prior_refusal.or(runtime_schema_refusal).or(prior_refusals.into_iter().next());
        Ok(super::StateStoreCensus { terminals, leases, lease_retirements, prior_refusal })
    }
}
enum StoreStartupRecoveryCustody {
    Opening(LiveStateStoreOpenFailure),
    /// A close in progress, with the refusal that stopped its latest attempt.
    Closing {
        factory: Arc<OriginalNativeFactory>,
        progress: Mutex<lillux::custody::Held<StoreStartupCloseProgress>>,
        refusal: anyhow::Error,
    },
}
pub struct LiveStateStoreStartupRecoveryFailure {
    inner: lillux::custody::Held<StoreStartupRecoveryParts>,
}
/// The refused recovery's custody. Constructible only in this module.
struct StoreStartupRecoveryParts {
    custody: StoreStartupRecoveryCustody,
}
/// An opening failure retains its own checkout through that checkout's own
/// end, so it settles; a closing progress stays retained whole, with its
/// factory alias and refusal, until the process exits.
fn store_startup_recovery_settled(parts: &StoreStartupRecoveryParts) -> bool {
    matches!(parts.custody, StoreStartupRecoveryCustody::Opening(_))
}
impl LiveStateStoreStartupRecoveryFailure {
    fn new(custody: StoreStartupRecoveryCustody) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                STORE_STARTUP_RECOVERY_KIND,
                StoreStartupRecoveryParts { custody },
                store_startup_recovery_settled,
            ),
        }
    }
    /// Why the recovery is refused: the opening refusal while no close has
    /// begun, otherwise the refusal that stopped the latest close attempt.
    pub fn refusal(&self) -> &anyhow::Error {
        match &self.inner.custody {
            StoreStartupRecoveryCustody::Opening(failure) => &failure.error,
            StoreStartupRecoveryCustody::Closing { factory: _, progress: _, refusal } => refusal,
        }
    }
    /// Move the whole custody out; a copy in another process is refused and
    /// comes back still held.
    fn into_custody(self) -> std::result::Result<StoreStartupRecoveryCustody, Self> {
        self.inner.take_whole().map(|parts| parts.custody).map_err(|inner| Self { inner })
    }
}
impl std::fmt::Debug for LiveStateStoreStartupRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStateStoreStartupRecoveryFailure").finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveStateStoreStartupRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "actual Store startup cleanup remains pending: {}", self.refusal())
    }
}
impl std::error::Error for LiveStateStoreStartupRecoveryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.refusal().as_ref()) }
}
impl LiveStateStoreOpenFailure {
    pub fn close_retained(self) -> std::result::Result<ReleasedLiveStateStoreStartup, LiveStateStoreStartupRecoveryFailure> {
        if self.factory.require_original_process().is_err() {
            return Err(LiveStateStoreStartupRecoveryFailure::new(StoreStartupRecoveryCustody::Opening(self)));
        }
        let Self { error, factory, _custody } = self;
        let mut custody = _custody.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
        // An opening owner already handed outside leaves the close nothing to
        // begin from; the close below refuses that plainly and retains the rest.
        let original = custody.take_original();
        let mut progress = StoreStartupCloseProgress { store_destination_join: std::sync::OnceLock::new(), runtime_packet_parts: std::array::from_fn(|_| None),
            operational_packet_parts: std::array::from_fn(|_| None), scheduler_packet_parts: None,
            scheduler_unopened_database: None, scheduler_unopened_schema_reference: None,
            runtime_schema_source_parts: None, runtime_schema_unused_inputs: None,
            runtime_unopened_schema_reference: None, runtime_schema_refusal: None,
            scheduler_admission_sources: None,
            failed_auxiliary_sources: FailedStartupAuxiliaryReceipt::not_constructed_at_close_retained(),
            operational_sql_custody: std::array::from_fn(|_| None), runtime_sql_custody: std::array::from_fn(|_| None), physical_registry: None, original, store_parent: None, store_parent_parts: None, store_parent_finalization: None, store_parent_finalization_retired: false, runtime: custody.take_runtime(),
            runtime_failure: None, runtime_closed: None, runtime_outside: None, runtime_open_failure: None, runtime_released: None, runtime_released_outside: None, state: custody.take_state(), state_failure: None,
            state_closed: None, state_open_failure: None, state_recovery_failure: None, state_released: None, state_physical: None,
            operational_open_failure: None, operational_recovery_failure: None, operational_released: None, operational_physical: None,
            schema_failure: None, schema_released: None, prior_refusal: None };
        match error.downcast::<ryeos_state::state_db::LiveStateDatabaseOpenFailure>() {
            Ok(failure) => progress.state_open_failure = Some(failure),
            Err(error) => match error.downcast::<ryeos_state::sqlite_schema::NativeSchemaSqlReferenceFailure>() {
                Ok(failure) => progress.schema_failure = Some(failure),
                Err(error) => match error.downcast::<runtime_db::OriginalRuntimeAcquisitionFailure>() {
                    Ok(failure) => progress.runtime_open_failure = Some(failure),
                    Err(error) => match error.downcast::<ryeos_state::operational::LiveOperationalOpenFailure>() {
                        Ok(failure) => progress.operational_open_failure = Some(failure),
                        Err(error) => progress.prior_refusal = Some(error),
                    },
                },
            },
        }
        advance_store_startup_close(factory, progress.held(STORE_STARTUP_CLOSE_PROGRESS_KIND))
    }
}
impl LiveStateStoreStartupRecoveryFailure {
    pub fn retry(self) -> std::result::Result<ReleasedLiveStateStoreStartup, Self> {
        match self.into_custody() {
            Err(this) => Err(this),
            Ok(StoreStartupRecoveryCustody::Opening(failure)) => failure.close_retained(),
            Ok(StoreStartupRecoveryCustody::Closing { factory, progress, refusal }) => {
                if factory.require_original_process().is_err() {
                    return Err(Self::new(StoreStartupRecoveryCustody::Closing { factory, progress, refusal }));
                }
                // A new attempt reports its own refusal.
                drop(refusal);
                advance_store_startup_close(factory, progress.into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner))
            }
        }
    }
}
/// Advance a failed start's close under the factory its failure retains. A
/// progress whose opening owner already left it (an outside handoff began),
/// or whose owner belongs to another factory, is refused plainly and
/// retained whole.
fn advance_store_startup_close(factory: Arc<OriginalNativeFactory>, progress: lillux::custody::Held<StoreStartupCloseProgress>)
    -> std::result::Result<ReleasedLiveStateStoreStartup, LiveStateStoreStartupRecoveryFailure> {
    // A forked child holds only a copy of this progress; it is refused here,
    // before any `Held` exists that a later refusal would have to take back.
    if let Err(refusal) = factory.require_original_process() {
        return Err(LiveStateStoreStartupRecoveryFailure::new(StoreStartupRecoveryCustody::Closing {
            factory: Arc::clone(&factory), progress: Mutex::new(progress), refusal }));
    }
    let mut progress = progress;
    let retain = |progress: lillux::custody::Held<StoreStartupCloseProgress>, refusal: anyhow::Error| {
        LiveStateStoreStartupRecoveryFailure::new(StoreStartupRecoveryCustody::Closing {
            factory: Arc::clone(&factory), progress: Mutex::new(progress), refusal })
    };
    if let Err(error) = progress.original().and_then(|original| original.require_close_factory(&factory)) {
        return Err(retain(progress, error));
    }
    if let Some(runtime) = progress.get_mut().runtime.take() {
        match runtime.close_original_runtime(Arc::clone(&factory)) {
            Ok(closed) => progress.get_mut().runtime_closed = Some(closed),
            Err(failure) => progress.get_mut().runtime_failure = Some(failure),
        }
    } else if let Some(failure) = progress.get_mut().runtime_failure.take() {
        match failure.retry() {
            Ok(closed) => progress.get_mut().runtime_closed = Some(closed),
            Err(failure) => progress.get_mut().runtime_failure = Some(failure),
        }
    }
    if progress.runtime_failure.is_some() {
        return Err(retain(progress, anyhow::anyhow!("failed Store Runtime close remains pending")));
    }
    if let Some(failure) = progress.get_mut().runtime_open_failure.take() {
        match failure.retry_close() {
            Ok(released) => progress.get_mut().runtime_released = Some(released),
            Err(failure) => {
                progress.get_mut().runtime_open_failure = Some(failure);
                return Err(retain(progress, anyhow::anyhow!("failed Store Runtime acquisition release remains pending")));
            }
        }
    }
    if progress.state.is_some() || progress.state_failure.is_some() {
        // Capture authority before transferring State into its consuming close.
        // Retry already owns its exact State context and accepts a matching
        // guard from the retained original namespace, never a recreated anchor.
        let guard = match progress.state.as_ref().map(StateDb::pinned_authority) {
            Some(Ok(authority)) => authority.acquire_exclusive_guard(false),
            Some(Err(error)) => Err(error),
            None => match progress.state_failure.as_ref().expect("actual State close retry").acquire_existing_guard() {
                Ok(guard) => Ok(guard), Err(error) => Err(error),
            },
        };
        let guard = match guard { Ok(guard) => guard, Err(error) => return Err(retain(progress, error)) };
        let closed = if let Some(state) = progress.get_mut().state.take() { state.close_live(Arc::clone(&factory), &guard) }
            else { progress.get_mut().state_failure.take().expect("actual State close failure").retry(&guard) };
        match closed { Ok(closed) => progress.get_mut().state_closed = Some(closed),
            Err(failure) => {
                progress.get_mut().state_failure = Some(failure);
                return Err(retain(progress, anyhow::anyhow!("failed Store State close remains pending")));
            } }
    }
    if let Some(failure) = progress.get_mut().state_open_failure.take() {
        match failure.close_retained() {
            Ok(released) => progress.get_mut().state_released = Some(released),
            Err(failure) => {
                progress.get_mut().state_recovery_failure = Some(failure);
                return Err(retain(progress, anyhow::anyhow!("failed Store State opening release remains pending")));
            }
        }
    } else if let Some(failure) = progress.get_mut().state_recovery_failure.take() {
        match failure.retry() {
            Ok(released) => progress.get_mut().state_released = Some(released),
            Err(failure) => {
                progress.get_mut().state_recovery_failure = Some(failure);
                return Err(retain(progress, anyhow::anyhow!("failed Store State opening release remains pending")));
            }
        }
    }
    if let Some(failure) = progress.get_mut().operational_open_failure.take() {
        match failure.close_retained() {
            Ok(released) => progress.get_mut().operational_released = Some(released),
            Err(failure) => {
                progress.get_mut().operational_recovery_failure = Some(failure);
                return Err(retain(progress, anyhow::anyhow!("failed Store Operational opening release remains pending")));
            }
        }
    } else if let Some(failure) = progress.get_mut().operational_recovery_failure.take() {
        match failure.retry() {
            Ok(released) => progress.get_mut().operational_released = Some(released),
            Err(failure) => {
                progress.get_mut().operational_recovery_failure = Some(failure);
                return Err(retain(progress, anyhow::anyhow!("failed Store Operational opening release remains pending")));
            }
        }
    }
    if let Some(failure) = progress.get_mut().schema_failure.take() {
        match failure.close_retained() {
            Ok(released) => progress.get_mut().schema_released = Some(released),
            Err(failure) => {
                progress.get_mut().schema_failure = Some(failure);
                return Err(retain(progress, anyhow::anyhow!("failed Store schema reference release remains pending")));
            }
        }
    }
    // The progress was held only after the original process was confirmed.
    let released = ReleasedLiveStateStoreStartup::new(take_held(progress));
    if let Err(error) = released.native_terminals(&factory) {
        let progress = released.into_progress();
        return Err(retain(progress, error));
    }
    Ok(released)
}

impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for LiveStateOpen {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>, cleanup: bool) -> Result<()> {
        factory.require_original_process()?;
        OriginalNativeDestinationAuthority::require_original(self, factory)?;
        self.lease_authority.require_original(factory, cleanup)
    }
    fn fork_deadline(&self, factory: &Arc<OriginalNativeFactory>, cleanup: bool)
        -> Result<lillux::time::MonotonicDeadline> {
        lillux_sqlite::OriginalNativeProjectionLeaseAuthority::require_original(self, factory, cleanup)?;
        // Delegate to the original issuer's captured endpoint. State custody
        // does not renew a service duration or turn it into cleanup authority.
        self.lease_authority.fork_deadline(factory, cleanup)
    }
}

#[cfg(test)]
mod outside_quote_tests {
    use super::*;
    use lillux_sqlite::original_physical_custody::OutsideWork;

    #[test]
    fn store_finalizer_quotes_match_the_retained_owners_and_handoffs() {
        assert_eq!(quote_store_parent_facade_retirement(), OutsideWork { bytes: 0, steps: 4 });
        assert_eq!(quote_store_packet_vector_handoff(), OutsideWork { bytes: 0, steps: 4 });
        assert_eq!(quote_store_parent_authority_handoff(), OutsideWork { bytes: 0, steps: 2 });
        assert_eq!(quote_store_parent_alias_retirement(), OutsideWork { bytes: 0, steps: 2 });
        assert_eq!(quote_store_child_parent_alias_retirement(), OutsideWork { bytes: 0, steps: 4 });
        assert_eq!(quote_store_direct_operational_handoff(), OutsideWork { bytes: 0, steps: 2 });
        assert_eq!(quote_store_destination_join_preparation(), OutsideWork { bytes: 0, steps: 2 });
        assert_eq!(quote_store_root_registration_handoff(), OutsideWork { bytes: 0, steps: 2 });
        assert_eq!(quote_store_operator_lock_registration(), OutsideWork { bytes: 0, steps: 8 });

        let parent_bytes = std::mem::size_of::<LiveStorePhysicalParent>()
            + std::mem::size_of::<lillux::PinnedDirectory>()
            + 2 * std::mem::size_of::<usize>();
        assert_eq!(quote_store_physical_parent().unwrap(), OutsideWork {
            bytes: u64::try_from(parent_bytes).unwrap(), steps: 8,
        });
        assert_eq!(quote_store_parent_finalization_parts().unwrap(), OutsideWork {
            bytes: u64::try_from(std::mem::size_of::<LiveStoreParentFinalizationParts>()).unwrap(),
            steps: 8,
        });
    }
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const ADMISSION_SOURCES: &str = SCHEDULER_ADMISSION_OUTSIDE_SOURCES_KIND;
    const ADMISSION_FAILURE: &str = SCHEDULER_ADMISSION_FAILURE_KIND;
    const PROGRESS: &str = STORE_STARTUP_CLOSE_PROGRESS_KIND;
    const RELEASED: &str = RELEASED_STORE_STARTUP_KIND;
    const RECOVERY: &str = "ryeos-app::store-startup-recovery-failure";

    fn abandoned(kind: &str) -> usize {
        lillux::custody::abandoned_custody().count(kind)
    }

    fn admission_sources(joined: bool) -> LiveSchedulerNativeAdmissionOutsideSources {
        LiveSchedulerNativeAdmissionOutsideSources {
            inner: lillux::custody::Held::new(
                SCHEDULER_ADMISSION_OUTSIDE_SOURCES_KIND,
                SchedulerAdmissionOutsideSourceSlots {
                    original: None, bound_inputs: None, transient_inputs: None,
                    unopened_database: None, unopened_reference: None, joined,
                },
                scheduler_admission_sources_exhausted,
            ),
        }
    }

    #[test]
    fn dropped_exhausted_admission_sources_release_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let sources = admission_sources(true);
        sources.require_exhausted().unwrap();
        let before = abandoned(ADMISSION_SOURCES);
        sources.exhaust();
        assert_eq!(abandoned(ADMISSION_SOURCES), before);
    }

    #[test]
    fn dropped_unjoined_admission_sources_are_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let sources = admission_sources(false);
        assert!(sources.require_exhausted().is_err());
        let before = abandoned(ADMISSION_SOURCES);
        drop(sources);
        assert_eq!(abandoned(ADMISSION_SOURCES), before + 1);
    }

    /// A startup close progress in the shape close_retained builds before any
    /// owner joins it: no receiver, no registry, every slot empty. It is not
    /// exhausted, since its receiver and auxiliary receipt never completed.
    fn empty_startup_progress() -> StoreStartupCloseProgress {
        StoreStartupCloseProgress { store_destination_join: std::sync::OnceLock::new(),
            runtime_packet_parts: std::array::from_fn(|_| None),
            operational_packet_parts: std::array::from_fn(|_| None), scheduler_packet_parts: None,
            scheduler_unopened_database: None, scheduler_unopened_schema_reference: None,
            runtime_schema_source_parts: None, runtime_schema_unused_inputs: None,
            runtime_unopened_schema_reference: None, runtime_schema_refusal: None,
            scheduler_admission_sources: None,
            failed_auxiliary_sources: FailedStartupAuxiliaryReceipt::not_constructed_at_close_retained(),
            operational_sql_custody: std::array::from_fn(|_| None),
            runtime_sql_custody: std::array::from_fn(|_| None), physical_registry: None,
            original: None, store_parent: None,
            store_parent_parts: None, store_parent_finalization: None,
            store_parent_finalization_retired: true, runtime: None, runtime_failure: None,
            runtime_closed: None, runtime_outside: None, runtime_open_failure: None,
            runtime_released: None, runtime_released_outside: None, state: None,
            state_failure: None, state_closed: None, state_open_failure: None,
            state_recovery_failure: None, state_released: None, state_physical: None,
            operational_open_failure: None, operational_recovery_failure: None,
            operational_released: None, operational_physical: None, schema_failure: None,
            schema_released: None, prior_refusal: None }
    }

    #[test]
    fn dropped_uncompleted_startup_progress_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let progress = empty_startup_progress().held(PROGRESS);
        assert!(progress.require_exhausted_holders().is_err());
        let before = abandoned(PROGRESS);
        drop(progress);
        assert_eq!(abandoned(PROGRESS), before + 1);
    }

    #[test]
    fn dropped_released_startup_is_retained_whole() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let released = ReleasedLiveStateStoreStartup::new(empty_startup_progress());
        let before = (abandoned(RELEASED), abandoned(PROGRESS));
        drop(released);
        // Held whole: the progress inside is never dropped on its own.
        assert_eq!((abandoned(RELEASED), abandoned(PROGRESS)), (before.0 + 1, before.1));
    }

    #[test]
    fn dropped_closing_recovery_failure_is_retained_whole() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_closing_recovery_failure_is_retained_whole");
        // The retained frame keeps its factory alias, which only the process
        // end releases, so the scenario runs in its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "recovery +1; progress +0", "{outcome}");
            return;
        }
        let host = ryeos_state::test_support::shared_test_host();
        let failure = LiveStateStoreStartupRecoveryFailure::new(StoreStartupRecoveryCustody::Closing {
            factory: Arc::clone(host.factory()),
            progress: Mutex::new(empty_startup_progress().held(PROGRESS)),
            refusal: anyhow::anyhow!("test close refused"),
        });
        let before = (abandoned(RECOVERY), abandoned(PROGRESS));
        drop(failure);
        let after = (abandoned(RECOVERY), abandoned(PROGRESS));
        lillux_sqlite::test_host::end_isolated_process(host,
            &format!("recovery +{}; progress +{}", after.0 - before.0, after.1 - before.1))
    }

    #[test]
    fn retry_after_the_opening_owner_left_is_a_plain_refusal() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::retry_after_the_opening_owner_left_is_a_plain_refusal");
        // The retained frame keeps its factory alias, which only the process
        // end releases, so the scenario runs in its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "refused twice; recovery +1", "{outcome}");
            return;
        }
        let host = ryeos_state::test_support::shared_test_host();
        let factory = Arc::clone(host.factory());
        // The whole startup progress moves by value through the close; a
        // debug build needs more than a test thread's default stack for it.
        let retained = std::thread::Builder::new().stack_size(64 << 20).spawn(move || {
            // A progress whose opening owner was already handed outside: the
            // retry refuses with its reason and keeps the progress, never
            // panics.
            let failure = LiveStateStoreStartupRecoveryFailure::new(StoreStartupRecoveryCustody::Closing {
                factory,
                progress: Mutex::new(empty_startup_progress().held(PROGRESS)),
                refusal: anyhow::anyhow!("outside handoff began"),
            });
            let Err(failure) = failure.retry() else { panic!("no opening owner to close from") };
            assert!(failure.refusal().to_string().contains("already transferred"), "{failure}");
            let Err(failure) = failure.retry() else { panic!("still no opening owner") };
            assert!(failure.to_string().contains("already transferred"), "{failure}");
            let before = abandoned(RECOVERY);
            drop(failure);
            abandoned(RECOVERY) - before
        }).unwrap().join().unwrap();
        lillux_sqlite::test_host::end_isolated_process(host,
            &format!("refused twice; recovery +{retained}"))
    }

    #[test]
    fn retained_owner_is_kept_on_unwind_and_returned_when_taken() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        const KIND: &str = "ryeos-app::test-retained-until-taken";
        // Taken back, the owner is returned whole and nothing is retained.
        let before = abandoned(KIND);
        let mut held = lillux::custody::Held::until_taken(KIND, vec![1u8]);
        held.get_mut().push(2);
        assert_eq!(take_held(held), vec![1, 2]);
        assert_eq!(abandoned(KIND), before);
        // A panic unwinding the stretch retains the owner, counted.
        let unwound = std::panic::catch_unwind(|| {
            let held = lillux::custody::Held::until_taken(KIND, vec![3u8]);
            assert!(held.is_empty(), "unwind while holding");
            take_held(held)
        });
        assert!(unwound.is_err());
        assert_eq!(abandoned(KIND), before + 1);
    }

    #[test]
    fn dropped_admission_failure_is_retained_whole() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_admission_failure_is_retained_whole");
        // The retained attempt aliases the live Store's opening owner, which
        // the Store's close would then refuse, so the scenario runs in its
        // own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "failure +1", "{outcome}");
            return;
        }
        let app_root = tempfile::tempdir().unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&app_root.path().join("node-key.pem"))
            .expect("test node identity");
        let signer: Arc<dyn Signer> = Arc::new(super::super::NodeIdentitySigner::from_identity(&identity));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(identity.fingerprint().to_string(), *identity.verifying_key());
        let store = super::super::test_native::TestStateStore::open(app_root.path(), signer, Arc::new(head_trust))
            .expect("test StateStore opens");
        // A claimed attempt refused after issuance keeps the opening owner's
        // alias; no prepared inputs are needed to retain it.
        let original = Arc::clone(store.live_native_open.as_ref().expect("live Store opening owner"));
        let failure = LiveSchedulerNativeAdmissionFailure {
            inner: lillux::custody::Held::until_taken(
                SCHEDULER_ADMISSION_FAILURE_KIND,
                LiveSchedulerNativeAdmissionFailureParts {
                    error: anyhow::anyhow!("scheduler admission issuance refused"),
                    attempt: LiveSchedulerAdmissionAttempt {
                        original: Arc::clone(&original), claimed: true, inputs: None, reference: None,
                    },
                },
            ),
        };
        assert!(failure.claimed());
        let before = abandoned(ADMISSION_FAILURE);
        drop(failure);
        let delta = abandoned(ADMISSION_FAILURE) - before;
        let host = super::super::test_native::take_shared_host();
        lillux_sqlite::test_host::end_isolated_process((original, store, app_root, host), &format!("failure +{delta}"))
    }
}
