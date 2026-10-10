//! Consuming live State shutdown. Native terminals retain real roots and
//! recovery custody; closing SQL neither erases State nor settles its account.
use super::*;
use crate::operational::{ClosedOriginalOperational, OriginalOperationalFailure};
use crate::projection::{ClosedOriginalProjection, OriginalProjectionFailure, ReleasedOriginalProjectionAcquisition};
use lillux_sqlite::{OriginalNativeConnectionTerminal, OriginalNativeFactory};

struct LiveRetiredStateContext {
    runtime: lillux::PinnedDirectory,
    _offline_namespace: Option<lillux::PinnedDirectoryLock>,
    cas: lillux::PinnedDirectory,
    refs: lillux::PinnedDirectory,
    locators: lillux::PinnedDirectory,
    recovery_directory: lillux::PinnedDirectory,
    _paths: [PathBuf; 3],
    previous: Vec<Arc<ClosedOriginalProjection>>,
    generated: OriginalProjectionBuildContext,
    recovery: RecoveryStore,
    _trust: Arc<TrustStore>,
    _head_cache: Mutex<HeadCache>,
    _repair_sink: Arc<dyn ProjectionRepairSink>,
}
impl LiveRetiredStateContext {
    fn require_guard(&self, guard: &crate::recovery::CasMutationGuard) -> anyhow::Result<()> {
        self.generated.factory.require_original_process()?;
        anyhow::ensure!(guard.is_exclusive(), "live State shutdown requires exclusive CAS custody");
        guard.ensure_protects_pinned_runtime(&self.runtime)?;
        self.recovery.ensure_guard(guard)?;
        self.require_roots()
    }
    fn require_roots(&self) -> anyhow::Result<()> {
        self.generated.factory.require_original_process()?;
        let OriginalProjectionStateOwner::Live(owner) = &self.generated.authority else {
            anyhow::bail!("live State shutdown cannot adopt offline authority");
        };
        owner.namespace().ensure_protects(&self.runtime)?;
        anyhow::ensure!(self.runtime.is_same_directory(owner.runtime())?, "live State shutdown changed its runtime");
        for root in [&self.runtime, &self.cas, &self.refs, &self.locators, &self.recovery_directory] {
            root.ensure_path_binding()?;
        }
        Ok(())
    }
    fn previous_terminals(&self) -> anyhow::Result<Vec<OriginalNativeConnectionTerminal>> {
        let factory = &self.generated.factory;
        let mut terminals = Vec::new();
        for owner in self.previous.iter().chain(self.generated.retired_selected.iter()) {
            owner.require_original(factory)?;
            push_unique(&mut terminals, OriginalNativeConnectionTerminal::Closed(owner.native_proof()));
        }
        for proof in &self.generated.terminals {
            proof.require_original(factory)?;
            push_unique(&mut terminals, OriginalNativeConnectionTerminal::Closed(Arc::clone(proof)));
        }
        let references = {
            let retained = self.generated.schema_references.try_lock()
                .map_err(|_| anyhow::anyhow!("live projection schema receipt custody busy"))?;
            retained.clone()
        };
        // Snapshot only Rust retention under its mutex. Actual native
        // provenance checks run after releasing that bookkeeping gate.
        for reference in &references {
            reference.require_original_closed(factory, lillux_sqlite::OriginalNativeDomain::NodeProjectionSchemaReference)?;
            push_unique(&mut terminals, reference.terminal().clone());
        }
        Ok(terminals)
    }
}
fn push_unique(terminals: &mut Vec<OriginalNativeConnectionTerminal>, terminal: OriginalNativeConnectionTerminal) {
    if !terminals.iter().any(|existing| match (existing, &terminal) {
        (OriginalNativeConnectionTerminal::Closed(a), OriginalNativeConnectionTerminal::Closed(b)) => Arc::ptr_eq(a, b),
        (OriginalNativeConnectionTerminal::Released(a), OriginalNativeConnectionTerminal::Released(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }) { terminals.push(terminal); }
}

enum ProjectionTerminal {
    Closed(ClosedOriginalProjection),
    Recovered(ReleasedOriginalProjectionAcquisition),
}
impl ProjectionTerminal {
    fn terminal(&self, factory: &Arc<OriginalNativeFactory>) -> anyhow::Result<OriginalNativeConnectionTerminal> {
        match self {
            Self::Closed(owner) => {
                owner.require_original(factory)?;
                Ok(OriginalNativeConnectionTerminal::Closed(owner.native_proof()))
            }
            Self::Recovered(owner) => {
                owner.terminal().require_original(factory)?;
                Ok(owner.terminal().clone())
            }
        }
    }
}
struct LiveStateCloseProgress {
    context: LiveRetiredStateContext,
    operational: Option<OperationalDb>,
    operational_failure: Option<OriginalOperationalFailure>,
    operational_closed: Option<ClosedOriginalOperational>,
    operational_outside: Option<crate::operational::OutsideOriginalOperationalCustody>,
    projection: Option<ProjectionDb>,
    projection_failure: Option<OriginalProjectionFailure>,
    projection_closed: Option<ProjectionTerminal>,
}

/// Holds actual native terminals, generation leases and canonical roots after
/// service SQL closes. This is not an offline reset ACK or resource settlement.
pub struct ClosedLiveState {
    schema_definitions: std::mem::ManuallyDrop<Vec<crate::sqlite_schema::SchemaSqlDefinition>>,
    projection_files: std::mem::ManuallyDrop<Vec<crate::projection::OutsideOriginalProjectionCustody>>,
    projection_native: std::mem::ManuallyDrop<Vec<OriginalNativeConnectionTerminal>>,
    projection_refusals: std::mem::ManuallyDrop<Vec<anyhow::Error>>,
    projection_sources_started: bool,
    destination_join: std::sync::OnceLock<Arc<super::destination_join::OriginalStateDestinationJoin>>, progress: std::mem::ManuallyDrop<LiveStateCloseProgress>, prior_refusal: Option<anyhow::Error> }
// Whole normal-close frame after its redundant State-parent edge ends. All
// canonical/recovery, schema, lease, native and refusal fields remain accessible
// to the same outside supervisor; this is not a disposal receipt.
pub struct ClosedLiveStatePhysicalCustody {
    process: u32,
    projection_physical: std::mem::ManuallyDrop<super::live_native::LiveProjectionPhysicalCustody>,
    parent: std::mem::ManuallyDrop<Option<super::live_native::LiveStateNativeOwner>>,
    parent_parts: std::mem::ManuallyDrop<Option<super::live_native::LiveStatePhysicalParent>>,
    join: Arc<super::destination_join::OriginalStateDestinationJoin>,
    _roots: std::mem::ManuallyDrop<NormalStatePhysicalRoots>,
    _recovery: std::mem::ManuallyDrop<([PathBuf; 3], Option<RecoveryStore>)>,
    recovery_aliases_retired: bool,
    recovery_registrations: super::live_native::LiveRecoveryPhysicalRegistrations,
    _controls: std::mem::ManuallyDrop<(Arc<TrustStore>, Mutex<HeadCache>, Arc<dyn ProjectionRepairSink>)>,
    _factory: Option<Arc<OriginalNativeFactory>>,
    _native: std::mem::ManuallyDrop<(Vec<Arc<ClosedOriginalProjection>>,
        Vec<Arc<lillux_sqlite::ClosedOriginalNativeConnection>>,
        Vec<Arc<lillux_sqlite::DurableOriginalNativeBound>>,
        Vec<Arc<ClosedOriginalProjection>>)>,
    _schema: std::mem::ManuallyDrop<(Mutex<Vec<Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>>>,
        Vec<crate::sqlite_schema::SchemaSqlDefinition>)>,
    _leases: std::mem::ManuallyDrop<(Mutex<Vec<Arc<crate::projection::OriginalGenerationLease>>>,
        std::sync::atomic::AtomicU32)>,
    _projection: std::mem::ManuallyDrop<(Vec<crate::projection::OutsideOriginalProjectionCustody>,
        Vec<OriginalNativeConnectionTerminal>, Vec<anyhow::Error>)>,
    operational: std::mem::ManuallyDrop<Option<crate::operational::OutsideOriginalOperationalCustody>>,
    _refusal: std::mem::ManuallyDrop<Option<anyhow::Error>>,
    metadata_pins: super::live_native::StateMetadataPinRetirement,
    native_retired: Option<super::live_native::StateNativeSourcesRetired>,
}
struct NormalStatePhysicalRoots {
    process: u32,
    directories: [Option<Arc<lillux::PinnedDirectory>>; 5],
    namespace: Option<lillux::PinnedDirectoryLock>,
    namespace_selected: bool,
    registrations: [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>; 6],
    registry_identity: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    retired_sources_consumed: bool,
}
impl NormalStatePhysicalRoots {
    unsafe fn register(&mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign normal State physical roots");
        anyhow::ensure!(!self.retired_sources_consumed,
            "retired normal State roots cannot be reopened");
        super::live_native::bind_root_receipt_registry(&mut self.registry_identity, registry)?;
        let additional_sources = (0..5)
            .filter(|slot| self.registrations[*slot].is_none() && self.directories[*slot].is_some())
            .count()
            + usize::from(self.namespace_selected
                && self.registrations[5].is_none() && self.namespace.is_some());
        let quote = quote_normal_state_root_membership(registry, additional_sources)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        anyhow::ensure!(self.process == std::process::id(), "foreign normal roots after membership debit");
        for slot in 0..5 {
            if let Some(binding) = &self.registrations[slot] {
                registry.require_membership(binding)?;
                anyhow::ensure!(self.directories[slot].is_none(), "registered normal State root retains source");
                continue;
            }
            let quote = super::live_native::quote_live_state_root_registration_handoff();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            anyhow::ensure!(self.process == std::process::id(), "foreign normal State roots after prepay");
            let actual = self.directories[slot].as_ref().context("actual normal State root absent")?.clone();
            match unsafe { registry.register_directory(actual, prepay) } {
                Ok(binding) => { self.registrations[slot] = Some(binding); drop(self.directories[slot].take()); }
                Err((alias, error)) => { drop(alias); return Err(error); }
            }
        }
        if self.namespace_selected && self.registrations[5].is_none() {
            let quote = super::live_native::quote_live_state_root_registration_handoff();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            anyhow::ensure!(self.process == std::process::id(), "foreign normal State roots after prepay");
            let actual = self.namespace.as_ref().context("actual selected State namespace absent")?.clone();
            match unsafe { registry.register_namespace(actual, prepay) } {
                Ok(binding) => { self.registrations[5] = Some(binding); drop(self.namespace.take()); }
                Err((alias, error)) => { drop(alias); return Err(error); }
            }
        }
        anyhow::ensure!(self.directories.iter().all(Option::is_none) && self.namespace.is_none(),
            "normal State physical sources remain unregistered");
        for slot in 0..(5 + usize::from(self.namespace_selected)) {
            let binding = self.registrations[slot].as_ref().context("actual normal State membership absent")?;
            registry.require_membership(binding)?;
            let source = binding.observed_source_identity()?;
            anyhow::ensure!(if slot == 5 {
                matches!(source, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })
            } else {
                matches!(source, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
            }, "normal State source role changed");
        }
        Ok(())
    }

    fn consume_retired_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign retired normal State roots");
        registry.require_same_registry(self.registry_identity.as_ref()
            .context("normal State root registry identity absent")?)?;
        let quote = super::live_native::quote_state_root_receipt_owner(5, usize::from(self.namespace_selected))?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        registry.require_same_registry(self.registry_identity.as_ref()
            .context("normal State root registry identity disappeared after funding")?)?;
        if self.retired_sources_consumed {
            anyhow::ensure!(self.registrations.iter().all(Option::is_none)
                && self.directories.iter().all(Option::is_none) && self.namespace.is_none(),
                "consumed normal State root registrations reappeared");
            return Ok(());
        }
        anyhow::ensure!(self.directories.iter().all(Option::is_none) && self.namespace.is_none(),
            "retired normal State frame retains a physical root alias");
        for slot in 0..5 {
            let binding = self.registrations[slot].as_ref()
                .context("retired normal State directory registration absent")?;
            anyhow::ensure!(matches!(binding.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                "retired normal State directory registration changed role");
        }
        if self.namespace_selected {
            let binding = self.registrations[5].as_ref()
                .context("retired selected State namespace registration absent")?;
            anyhow::ensure!(matches!(binding.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. }),
                "retired normal State namespace registration changed role");
        } else {
            anyhow::ensure!(self.registrations[5].is_none(),
                "unselected normal State namespace retains a registration");
        }
        for binding in self.registrations.iter().take(5 + usize::from(self.namespace_selected)).flatten() {
            drop(binding.receipt_in(registry)?);
        }
        self.registrations = std::array::from_fn(|_| None);
        self.retired_sources_consumed = true;
        Ok(())
    }
}
impl ClosedLiveStatePhysicalCustody {
    /// Pre-seal native-source stage for this normal frame. First retire the
    /// generated-Bound metadata pins of its closed connections (generated
    /// terminals, durable proofs and closed projection terminals, de-duplicated
    /// by allocation). Then end every remaining strong factory holder into a
    /// non-owning `StateNativeSourcesRetired`. Any refusal retains the whole
    /// frame and every pin.
    pub fn retire_native_sources(&mut self, factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
        issue_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> anyhow::Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    ) -> anyhow::Result<()> {
        self.require_original(factory, terminal)?;
        let Self { process: _, projection_physical, parent, parent_parts, join, _roots: _,
            _recovery: _, recovery_aliases_retired: _, recovery_registrations: _, _controls: _,
            _factory, _native, _schema, _leases, _projection, operational: _, _refusal,
            metadata_pins, native_retired } = self;
        if let Some(retired) = native_retired.as_ref() {
            anyhow::ensure!(_factory.is_none(), "retired State frame regained its native factory");
            retired.require_original_factory(factory)?;
            return retired.require_retired(prepay);
        }
        {
            let (previous, terminals, durable, selected) = &**_native;
            anyhow::ensure!(previous.is_empty() && selected.is_empty(),
                "closed State retains a projection owner outside its metadata-pin census");
            let projection = &_projection.1;
            let count = terminals.len().checked_add(durable.len())
                .and_then(|count| count.checked_add(projection.len()))
                .context("closed State metadata-pin candidate count overflow")?;
            let mut candidates = super::live_native::state_metadata_pin_candidates(factory, count, prepay)?;
            candidates.extend(terminals.iter().cloned());
            candidates.extend(durable.iter().map(|proof| proof.closed()));
            candidates.extend(projection.iter().filter_map(|terminal| match terminal {
                OriginalNativeConnectionTerminal::Closed(closed) => Some(Arc::clone(closed)),
                OriginalNativeConnectionTerminal::Released(_) => None,
            }));
            super::live_native::require_census_without_bound_inputs(&candidates)?;
            metadata_pins.retire(candidates, factory, terminal, prepay, issue_inputs)?;
        }
        // Holders already ended by earlier joins: the parent body moved into
        // its parts, generation leases became metadata roots, and the receiver
        // retains no leaf source (including a strong `Live` State owner).
        anyhow::ensure!(parent.is_none() && parent_parts.is_some(),
            "closed State parent body was not transferred into its parts");
        projection_physical.require_leases_retired()?;
        join.require_generated_roots_registered_with_prepay(registry, prepay)?;
        anyhow::ensure!(_factory.as_ref().is_some_and(|original| Arc::ptr_eq(original, factory)),
            "closed State frame selects another original factory");
        anyhow::ensure!(_schema.0.try_lock()
            .map_err(|_| anyhow::anyhow!("closed State schema census busy or poisoned"))?.is_empty()
            && _leases.0.try_lock()
            .map_err(|_| anyhow::anyhow!("closed State lease census busy or poisoned"))?.is_empty()
            && _projection.0.is_empty(),
            "closed State retains a schema, generation-lease or unregistered projection source");
        let owners = _native.1.len().checked_add(_native.2.len())
            .and_then(|owners| owners.checked_add(_projection.1.len()))
            .and_then(|owners| owners.checked_add(_projection.2.len()))
            .context("closed State native owner count overflow")?;
        super::live_native::require_refusals_without_native_owners(
            _projection.2.iter().chain(_refusal.iter()))?;
        super::live_native::prepay_state_native_source_retirement(factory, owners, prepay)?;
        terminal.require_original(factory)?;
        let receipt = metadata_pins.into_sources_retired(factory, _projection.2.len(), _refusal.is_some())?;
        // Infallible from here: each owner is moved out and ended explicitly;
        // the retained fields keep only empty values.
        let (previous, terminals, durable, selected) = std::mem::take(&mut **_native);
        drop(durable);
        drop(terminals);
        drop(previous);
        drop(selected);
        let (files, native, refusals) = std::mem::take(&mut **_projection);
        drop(native);
        drop(refusals);
        drop(files);
        let (references, definitions) = std::mem::take(&mut **_schema);
        drop(references);
        drop(definitions);
        let (leases, attempts) = std::mem::take(&mut **_leases);
        drop(leases);
        drop(attempts);
        drop(std::mem::take(&mut **_refusal));
        drop(_factory.take());
        *native_retired = Some(receipt);
        Ok(())
    }

    pub fn native_sources_retired(&self) -> bool {
        self.native_retired.is_some()
    }

    /// Bounded failure diagnostics kept after the native-source stage ended
    /// the projection refusals and prior refusal: (refusal count, prior?).
    pub fn retired_native_source_diagnostics(&self) -> Option<(usize, bool)> {
        self.native_retired.as_ref().map(super::live_native::StateNativeSourcesRetired::diagnostics)
    }

    /// Worst-case State-local metadata-pin work for one finalizer attempt
    /// over `resource_tickets` leaves, including the receipt checks of the
    /// projection and owner-frame stages. Native close and the issued inputs
    /// are charged by their own owners and quoted separately.
    pub fn quote_maximum_metadata_pin_attempt(resource_tickets: u32)
        -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        super::live_native::quote_maximum_state_metadata_pin_attempt(resource_tickets)
    }

    /// Consume only projection-root registration receipts from the exact
    /// retired inventory. Projection evidence and the rest of State custody
    /// remain in this owner for their own typed finalization stages.
    pub fn consume_retired_projection_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign closed State projection custody");
        self.native_retired.as_ref()
            .context("State native sources were not retired before the region seal")?
            .require_retired(prepay)?;
        self.projection_physical.consume_retired_sources(registry, prepay)?;
        self.join.consume_retired_generated_roots(registry, prepay)
    }

    /// Consume the normal State, RecoveryStore, shared-parent and shared
    /// parent-RecoveryStore root registrations from this exact retired inventory.
    /// The caller first consumes the matching Store packet sources; then the
    /// live Operational startup-only metadata can be released under this same
    /// retired inventory. Other State schema, lease and terminal owners remain.
    pub fn consume_retired_root_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign closed State root custody");
        anyhow::ensure!(self.native_retired.is_some(),
            "State roots cannot retire before its native-source stage");
        anyhow::ensure!(self.recovery_aliases_retired && self._recovery.1.is_none(),
            "State recovery aliases remain before root receipt consumption");
        self._roots.consume_retired_sources(registry, prepay)?;
        self.recovery_registrations.consume_retired_sources(registry, prepay)?;
        self.parent_parts.as_mut().context("actual shared State parent parts absent")?
            .consume_retired_root_sources(registry, prepay)?;
        if let Some(operational) = self.operational.as_mut() {
            operational.consume_retired_shared_roots(registry, prepay)?;
            operational.consume_retired_live_metadata(registry, prepay)?;
        }
        Ok(())
    }

    /// End the two actual RecoveryStore aliases only after their exact roots
    /// have joined this inventory. The authority copy, when present, must be
    /// the same pair of Arc owners; all checks and prepaid work happen before
    /// either source is consumed.
    pub fn retire_recovery_aliases(
        &mut self,
        authority_recovery: &mut Option<RecoveryStore>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State recovery finalization");
        if self.recovery_aliases_retired {
            anyhow::ensure!(authority_recovery.is_none(),
                "State recovery authority alias reappeared after retirement");
            let quote = registry.membership_work_bound(2, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            return self.recovery_registrations.require_registered_sources(registry);
        }
        let original = self._recovery.1.as_ref()
            .context("actual State RecoveryStore was already consumed")?;
        let original_roots = original.borrow_original_directory_custody();
        self.recovery_registrations.require_registered(&original_roots, registry)?;
        if let Some(authority) = authority_recovery.as_ref() {
            let authority_roots = authority.borrow_original_directory_custody();
            for (slot, (expected, actual)) in original_roots.iter().zip(authority_roots.iter()).enumerate() {
                anyhow::ensure!(Arc::ptr_eq(expected, actual),
                    "PinnedStateAuthority recovery alias changed root owner at slot {slot}");
            }
            self.recovery_registrations.require_registered(&authority_roots, registry)?;
        }
        // Two original roots and their optional authority aliases are checked
        // both before and after funding: eight opaque-membership validations
        // in the worst case, plus the fixed retirement transition.
        let quote = registry.membership_work_bound(8, 0)?.checked_add(
            lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 4 },
        )?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;

        // Revalidate the same retained owners after the irreversible charge.
        let original = self._recovery.1.as_ref()
            .context("actual State RecoveryStore disappeared after funding")?;
        let original_roots = original.borrow_original_directory_custody();
        self.recovery_registrations.require_registered(&original_roots, registry)?;
        if let Some(authority) = authority_recovery.as_ref() {
            let authority_roots = authority.borrow_original_directory_custody();
            for (slot, (expected, actual)) in original_roots.iter().zip(authority_roots.iter()).enumerate() {
                anyhow::ensure!(Arc::ptr_eq(expected, actual),
                    "PinnedStateAuthority recovery alias changed after funding at slot {slot}");
            }
            self.recovery_registrations.require_registered(&authority_roots, registry)?;
        }

        // The registry now owns the exact registered roots. Consume both
        // semantic RecoveryStore frames through their typed owner transition;
        // their moved Arc aliases end here while the physical directory owners
        // remain in the registry until matching close receipts.
        let (paths, recovery) = unsafe { std::mem::ManuallyDrop::take(&mut self._recovery) };
        drop(paths);
        if let Some(recovery) = recovery {
            drop(recovery.into_original_directory_custody());
        }
        if let Some(authority) = authority_recovery.take() {
            drop(authority.into_original_directory_custody());
        }
        self.recovery_aliases_retired = true;
        Ok(())
    }

    pub fn retire_parent_host_aliases(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        lease: &Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        self.parent_parts.as_mut().context("actual State parent parts absent")?
            .retire_host_aliases(factory, aggregate, store, lease, registry, prepay)
    }

    /// # Safety
    /// Retain this whole original frame and the same original registry across
    /// every partial refusal; original admission funds all wrapper/scan work.
    pub unsafe fn register_parent_roots(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        if self.parent_parts.is_none() {
            anyhow::ensure!(self.parent.is_some(), "actual live State parent body not transferred");
            let quote = super::live_native::quote_live_state_physical_parent()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            self.require_original(factory, aggregate)?;
            let original = self.parent.take().expect("checked actual live State body");
            *self.parent_parts = Some(super::live_native::LiveStatePhysicalParent::from_original(original));
        }
        unsafe { self.parent_parts.as_mut().expect("cached actual live State parts")
            .register_roots(registry, prepay) }?;
        unsafe { self._roots.register(registry, prepay) }?;
        if self.recovery_aliases_retired {
            self.recovery_registrations.require_registered_sources(registry)?;
        } else {
            unsafe { self.recovery_registrations.register(
                self._recovery.1.as_ref().context("actual State RecoveryStore absent")?,
                registry, prepay,
            ) }?;
        }
        unsafe { self.projection_physical.register(&mut self._projection.0,
            Some(&self._leases.0), factory, registry, prepay) }?;
        unsafe { self.join.register_generated_roots(registry, prepay) }
    }

    /// # Safety
    /// The actual outside Store must retain this complete frame, have handed
    /// over every admitted native/destination source, and exclude new producers.
    /// A refusal retains the same receiver and all cached partial transfers.
    pub unsafe fn advance_parent(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        // Upper redundant parent edges ended only after this whole frame and
        // canonical receiver were retained by the consuming constructor.
        if self.parent.is_some() || self.parent_parts.is_some() { return Ok(()); }
        unsafe { self.join.advance_live_parent(factory, aggregate, prepay) }?;
        // Preserve every actual parent field in this whole cached upper frame
        // before generated roots or the Store backedge can advance separately.
        unsafe { self.join.transfer_live_parent(factory, aggregate, &mut self.parent, prepay) }
    }

    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>,
        terminal: &lillux_sqlite::OriginalNativeTerminal) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign closed live State physical frame");
        self.join.require_original(factory, terminal)
    }
    pub fn require_operational_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State physical registration");
        self.operational.as_ref().context("actual Operational physical frame absent")?
            .require_shared_roots_registered(registry, prepay)
    }
}
impl ClosedLiveState {
    /// Consume the complete actual frame before ending only its redundant parent
    /// alias. Refusal returns the original source; all native handoffs must have
    /// completed and Operational roots must already join the supplied registry.
    pub fn into_physical_custody(self, factory: &Arc<OriginalNativeFactory>,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>)
        -> std::result::Result<ClosedLiveStatePhysicalCustody, (Self, anyhow::Error)> {
        let checked = lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            factory.require_original_process()?;
            terminal.require_original(factory)?;
            self.destination_join.get().context("actual State receiver absent")?
                .require_original(factory, terminal)?;
            let p = &self.progress;
            anyhow::ensure!(p.operational.is_none() && p.operational_failure.is_none()
                && p.operational_closed.is_none() && p.projection.is_none()
                && p.projection_failure.is_none() && p.projection_closed.is_none()
                && p.context.previous.is_empty() && p.context.generated.retired_selected.is_empty(),
                "closed live State source handoff incomplete");
            anyhow::ensure!(p.context.generated.schema_references.try_lock()
                .map_err(|_| anyhow::anyhow!("State schema census busy or poisoned"))?.is_empty(),
                "actual State schema sources remain");
            p.operational_outside.as_ref().context("actual Operational outside absent")?
                .require_shared_roots_registered(registry, prepay)?;
            anyhow::ensure!(matches!(p.context.generated.authority, OriginalProjectionStateOwner::Live(_)),
                "live physical transfer cannot adopt offline authority");
            let quote = quote_closed_live_state_physical_custody()?;
            prepay(quote.bytes, quote.steps)?;
            factory.require_original_process()?;
            terminal.require_original(factory)?;
            self.destination_join.get().context("actual State receiver absent after funding")?
                .require_original(factory, terminal)
        });
        if let Err(error) = checked { return Err((self, error)); }
        let Self { schema_definitions, projection_files, projection_native, projection_refusals,
            projection_sources_started: _, destination_join, progress, prior_refusal } = self;
        let LiveStateCloseProgress { context, operational, operational_failure, operational_closed,
            operational_outside, projection, projection_failure, projection_closed } = std::mem::ManuallyDrop::into_inner(progress);
        // Checked-empty effect owners are consumed explicitly; every resource
        // field below is moved to the new whole frame before ending the alias.
        debug_assert!(operational.is_none() && operational_failure.is_none() && operational_closed.is_none()
            && projection.is_none() && projection_failure.is_none() && projection_closed.is_none());
        let LiveRetiredStateContext { runtime, _offline_namespace, cas, refs, locators,
            recovery_directory, _paths, previous, generated, recovery, _trust, _head_cache, _repair_sink } = context;
        let OriginalProjectionBuildContext { factory, authority, terminals, durable, retired_selected,
            schema_references, generation_leases, generation_lease_attempts } = generated;
        let frame = ClosedLiveStatePhysicalCustody {
            projection_physical: std::mem::ManuallyDrop::new(super::live_native::LiveProjectionPhysicalCustody::empty()),
            parent: std::mem::ManuallyDrop::new(None),
            parent_parts: std::mem::ManuallyDrop::new(None),
            process: std::process::id(), join: destination_join.into_inner().expect("checked actual receiver"),
            _roots: std::mem::ManuallyDrop::new(NormalStatePhysicalRoots {
                process: std::process::id(),
                directories: [runtime, cas, refs, locators, recovery_directory].map(|root| Some(Arc::new(root))),
                namespace_selected: _offline_namespace.is_some(), namespace: _offline_namespace,
                registrations: std::array::from_fn(|_| None),
                registry_identity: None, retired_sources_consumed: false,
            }),
            _recovery: std::mem::ManuallyDrop::new((_paths, Some(recovery))),
            recovery_aliases_retired: false,
            recovery_registrations: super::live_native::LiveRecoveryPhysicalRegistrations::empty(),
            _controls: std::mem::ManuallyDrop::new((_trust, _head_cache, _repair_sink)), _factory: Some(factory),
            _native: std::mem::ManuallyDrop::new((previous, terminals, durable, retired_selected)),
            _schema: std::mem::ManuallyDrop::new((schema_references, std::mem::ManuallyDrop::into_inner(schema_definitions))),
            _leases: std::mem::ManuallyDrop::new((generation_leases, generation_lease_attempts)),
            _projection: std::mem::ManuallyDrop::new((std::mem::ManuallyDrop::into_inner(projection_files),
                std::mem::ManuallyDrop::into_inner(projection_native), std::mem::ManuallyDrop::into_inner(projection_refusals))),
            operational: std::mem::ManuallyDrop::new(operational_outside),
            _refusal: std::mem::ManuallyDrop::new(prior_refusal),
            metadata_pins: super::live_native::StateMetadataPinRetirement::empty(),
            native_retired: None,
        };
        // The receiver retains the canonical SAME parent. This ends only the
        // upper frame's redundant edge; no root, receipt or recovery is dropped.
        drop(authority);
        Ok(frame)
    }

    /// Transfer the actual Operational native packet vector to the Store
    /// supervisor. Context, schema bodies and marker custody remain here.
    /// # Safety
    /// The same original outside controller must immediately cache the returned
    /// vector before another effect; extraction does not retire packet affinity.
    pub unsafe fn take_operational_sql_custody(&mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> anyhow::Result<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.destination_join.get().context("actual State destination receiver absent")?
            .require_original(factory, aggregate)?;
        anyhow::ensure!(self.progress.operational.is_none()
            && self.progress.operational_failure.is_none()
            && self.progress.operational_closed.is_none(),
            "State retains an untransferred Operational native frame");
        let outside = self.progress.operational_outside.as_mut()
            .context("actual closed State Operational outside source absent")?;
        unsafe { outside.take_original_sql_custody() }
    }

    /// # Safety
    /// The original outside supervisor retains this complete State frame and
    /// prepays transfers into the SAME unsealed Store registry. Native receipt
    /// handoff must already have completed; no new admission is issued here.
    pub unsafe fn register_operational_shared_roots(&mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.destination_join.get().context("actual State destination receiver absent")?
            .require_original(factory, aggregate)?;
        anyhow::ensure!(self.progress.operational.is_none()
            && self.progress.operational_failure.is_none()
            && self.progress.operational_closed.is_none(),
            "State retains an untransferred Operational native frame");
        let LiveStateCloseProgress { operational_outside, context, .. } = &mut *self.progress;
        let OriginalProjectionStateOwner::Live(parent) = &context.generated.authority else {
            anyhow::bail!("live Operational context normalization requires actual State parent");
        };
        let outside = operational_outside.as_mut()
            .context("actual closed State Operational outside source absent")?;
        outside.normalize_live_context(factory, aggregate, parent, prepay)?;
        unsafe { outside.register_shared_roots(registry, prepay) }?;
        outside.require_shared_roots_registered(registry, prepay)
    }

    /// Transfer the actual Operational context into accessible post-ACK custody.
    /// Cache the complete outcome before further native or physical effects.
    pub fn advance_operational_outside(
        &mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.destination_join.get().context("actual State destination receiver absent")?
            .require_original(factory, aggregate)?;
        let LiveStateCloseProgress { operational_closed, operational_outside, context, .. } = &mut *self.progress;
        let OriginalProjectionStateOwner::Live(parent) = &context.generated.authority else {
            anyhow::bail!("live Operational outside transfer requires actual live State parent");
        };
        if let Some(outside) = operational_outside {
            return outside.advance_live_native_receipt(factory, aggregate, prepay);
        }
        let Some(source) = operational_closed.take() else { anyhow::bail!("actual closed Operational source absent"); };
        match source.into_live_outside_custody(factory, aggregate, parent, prepay) {
            Ok(outside) => *operational_outside = Some(outside),
            Err((source, error)) => {
                *operational_closed = Some(source);
                return Err(error);
            }
        }
        operational_outside.as_mut().context("actual Operational outside custody absent")?
            .advance_live_native_receipt(factory, aggregate, prepay)
    }

    /// Consume actual closed projection schema sources under the retained
    /// State. Definitions and receipts stay cached through alias refusals.
    pub fn advance_schema_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        self.destination_join.get().context("actual live State receiver not prepared")?.require_original(factory, terminal)?;
        if let Some(operational) = &mut self.progress.operational_closed {
            operational.advance_live_schema_sources(factory, terminal, prepay)?;
        }
        let registry = &self.progress.context.generated.schema_references;
        let count = registry.try_lock().map_err(|_| anyhow::anyhow!("live schema source quote busy or poisoned"))?.len();
        if count == 0 { return Ok(()); }
        let quote = quote_live_schema_source_retirement(
            count, self.schema_definitions.len(), self.projection_native.len(),
        )?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        self.schema_definitions.try_reserve_exact(count)?;
        self.projection_native.try_reserve_exact(count)?;
        let mut references = registry.try_lock().map_err(|_| anyhow::anyhow!("live schema source retirement busy or poisoned"))?;
        anyhow::ensure!(references.len() <= count, "live schema source changed after original quote");
        // Rust identity/transfer only beneath the registry gate. All native
        // provenance and original authority checks already ran before it.
        for index in (0..references.len()).rev() {
            if references.iter().take(index).any(|reference| Arc::ptr_eq(reference, &references[index])) { drop(references.remove(index)); }
        }
        while let Some(reference) = references.pop() {
            match Arc::try_unwrap(reference) {
                Ok(reference) => {
                    let (definition, proof) = reference.into_original_parts();
                    self.schema_definitions.push(definition);
                    self.projection_native.push(proof);
                }
                Err(reference) => {
                    references.push(reference);
                    anyhow::bail!("actual closed live schema source still has an external alias");
                }
            }
        }
        Ok(())
    }
    /// Advance actual projection sources in place after genuine native ACK.
    /// Every file/lease/error and canonical receipt remains in this same closed
    /// State on refusal. This does not close the full State/Store source census.
    pub fn advance_projection_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        self.destination_join.get().context("actual live State receiver not prepared")?.require_original(factory, terminal)?;
        let LiveStateCloseProgress { context: retired, projection_closed, .. } = &mut *self.progress;
        let LiveRetiredStateContext { previous, generated: context, .. } = retired;
        let lease_count = context.generation_leases.try_lock().map_err(|_| anyhow::anyhow!("original live generation registry busy or poisoned"))?.len();
        let count = previous.len().checked_add(context.retired_selected.len())
            .and_then(|count| count.checked_add(usize::from(projection_closed.is_some())))
            .context("live projection source count overflow")?;
        if count == 0 && self.projection_sources_started { return Ok(()); }
        let quote = quote_live_projection_source_transfer(
            count, lease_count, self.projection_files.len(), self.projection_native.len(),
            self.projection_refusals.len(),
        )?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        self.projection_files.try_reserve_exact(count)?;
        self.projection_native.try_reserve_exact(count)?;
        self.projection_refusals.try_reserve_exact(count)?;
        previous.try_reserve_exact(context.retired_selected.len())?;
        let leases = {
            let retained = context.generation_leases.try_lock().map_err(|_| anyhow::anyhow!("original live generation registry busy or poisoned"))?;
            anyhow::ensure!(retained.len() <= lease_count, "live generation source changed after original quote");
            retained.clone()
        };
        // Genuine lease-body retirement precedes transferring generation files;
        // no native call runs under the generation registry retention mutex.
        for lease in leases { lease.retire_after_native_terminal(terminal)?; }
        if let Some(selected) = projection_closed.take() {
            match selected {
                ProjectionTerminal::Closed(owner) => {
                    let (files, proof) = owner.into_outside_custody();
                    self.projection_files.push(files);
                    self.projection_native.push(OriginalNativeConnectionTerminal::Closed(proof));
                }
                ProjectionTerminal::Recovered(owner) => {
                    let (files, proof, error) = owner.into_outside_custody();
                    self.projection_files.push(files);
                    self.projection_native.push(proof);
                    self.projection_refusals.push(error);
                }
            }
            self.projection_sources_started = true;
        }
        previous.append(&mut context.retired_selected);
        // Remove only duplicate outer aliases while another canonical whole
        // projection is cached. Matching coordinates are never sufficient.
        for index in (0..previous.len()).rev() {
            if previous.iter().take(index).any(|owner| Arc::ptr_eq(owner, &previous[index])) {
                drop(previous.remove(index));
            }
        }
        while let Some(owner) = previous.pop() {
            match Arc::try_unwrap(owner) {
                Ok(owner) => {
                    let (files, proof) = owner.into_outside_custody();
                    self.projection_files.push(files);
                    self.projection_native.push(OriginalNativeConnectionTerminal::Closed(proof));
                }
                Err(owner) => {
                    previous.push(owner);
                    anyhow::bail!("original live prior projection still has an external alias");
                }
            }
        }
        Ok(())
    }
    /// Exact actual host-owner identity for the consuming Store graph; matching
    /// roots, policy or factory alone cannot join two different Store owners.
    pub fn require_original_store_parent(&self,
        store: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>, factory: &Arc<OriginalNativeFactory>) -> anyhow::Result<()> {
        factory.require_original_process()?;
        let OriginalProjectionStateOwner::Live(parent) = &self.progress.context.generated.authority else {
            anyhow::bail!("closed live State cannot select offline Store parent");
        };
        parent.require_original_store_parent(store, factory)
    }
    /// Prepare and retain this actual closed State's shared destination
    /// receiver after genuine aggregate acknowledgement. No runtime readiness
    /// or fresh database authority is re-entered during cleanup.
    pub fn prepare_destination_join(&self,
        factory: &Arc<OriginalNativeFactory>, terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        if let Some(join) = self.destination_join.get() { return join.require_original(factory, terminal); }
        let context = &self.progress.context.generated;
        anyhow::ensure!(context.factory.same_original(factory), "closed live State selected foreign factory");
        let join = context.authority.original_destination_join(factory, terminal, prepay)?;
        let _ = self.destination_join.set(join);
        self.destination_join.get().context("closed live State receiver publication absent")?.require_original(factory, terminal)
    }
    pub fn generation_lease_terminals(&self) -> anyhow::Result<Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>> {
        // Actual consuming State close finished every current/retired SQL
        // owner before this point; original roots and CAS recovery remain held.
        self.native_terminals()?;
        let context = &self.progress.context.generated;
        context.factory.require_original_process()?;
        let leases = context.generation_leases.try_lock()
            .map_err(|_| anyhow::anyhow!("actual generation lease registry busy"))?.clone();
        let mut terminals = Vec::new();
        for lease in leases {
            // SAFETY: these are the registry of the consumed State whose real
            // SQL terminals were checked above. The outer Store supervisor
            // drains execution before transferring State; no active facade is
            // handed out by this closed owner. Effects run outside the mutex.
            if let Some(terminal) = unsafe { lease.close_after_consumers(&context.factory) }? {
                if !terminals.iter().any(|prior| Arc::ptr_eq(prior, &terminal)) { terminals.push(terminal); }
            }
        }
        Ok(terminals)
    }
    pub fn prior_refusal(&self) -> Option<&anyhow::Error> { self.prior_refusal.as_ref() }
    pub fn native_terminals(&self) -> anyhow::Result<Vec<OriginalNativeConnectionTerminal>> {
        self.progress.context.generated.factory.require_original_process()?;
        anyhow::ensure!(self.progress.operational_outside.is_none(), "Operational source already transferred after aggregate acknowledgement");
        self.progress.context.require_roots()?;
        let factory = &self.progress.context.generated.factory;
        let mut terminals = self.progress.context.previous_terminals()?;
        for terminal in self.progress.operational_closed.as_ref().context("actual live Operational terminal absent")?.native_terminals()? {
            push_unique(&mut terminals, terminal);
        }
        if let Some(selected) = &self.progress.projection_closed { push_unique(&mut terminals, selected.terminal(factory)?); }
        else { anyhow::ensure!(self.projection_sources_started, "actual live projection terminal absent without consuming source transfer"); }
        for proof in self.projection_native.iter() { proof.require_original(factory)?; push_unique(&mut terminals, proof.clone()); }
        Ok(terminals)
    }
}

enum LiveStateCloseCustody { State(StateDb), Progress(LiveStateCloseProgress), Closed(ClosedLiveState) }
pub struct LiveStateCloseFailure {
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
    custody: std::mem::ManuallyDrop<Mutex<LiveStateCloseCustody>>,
}
impl std::fmt::Debug for LiveStateCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStateCloseFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveStateCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "live State close custody retained: {}", self.error) }
}
impl std::error::Error for LiveStateCloseFailure {}
impl LiveStateCloseFailure {
    /// Reacquire only the original existing CAS anchor for a retained close
    /// retry. No retention lock spans acquisition of the mutation guard.
    pub fn acquire_existing_guard(&self) -> anyhow::Result<crate::recovery::CasMutationGuard> {
        self.factory.require_original_process()?;
        let runtime = {
            let custody = self.custody.try_lock().map_err(|_| anyhow::anyhow!("State close custody busy"))?;
            match &*custody {
                LiveStateCloseCustody::State(state) => state._runtime_state_directory.try_clone()?,
                LiveStateCloseCustody::Progress(progress) => progress.context.runtime.try_clone()?,
                LiveStateCloseCustody::Closed(closed) => closed.progress.context.runtime.try_clone()?,
            }
        };
        crate::recovery::CasMutationGuard::acquire_existing_exclusive_in_pinned_runtime(&runtime)
    }
    pub fn retry(self, guard: &crate::recovery::CasMutationGuard) -> std::result::Result<ClosedLiveState, Self> {
        if self.factory.require_original_process().is_err() { return Err(self); }
        let Self { error, factory, custody } = self;
        advance_close(factory, std::mem::ManuallyDrop::into_inner(custody).into_inner().unwrap_or_else(|p| p.into_inner()), guard, Some(error))
    }
}

fn advance_close(factory: Arc<OriginalNativeFactory>, custody: LiveStateCloseCustody,
    guard: &crate::recovery::CasMutationGuard, prior: Option<anyhow::Error>) -> std::result::Result<ClosedLiveState, LiveStateCloseFailure> {
    let retain = |error, custody| LiveStateCloseFailure { error, factory: Arc::clone(&factory), custody: std::mem::ManuallyDrop::new(Mutex::new(custody)) };
    if let Err(error) = factory.require_original_process() { return Err(retain(error, custody)); }
    let mut progress = match custody {
        LiveStateCloseCustody::State(state) => {
            let checked = (|| -> anyhow::Result<()> {
                state.ensure_cas_mutation_guard(guard)?;
                anyhow::ensure!(guard.is_exclusive(), "live State close requires exclusive CAS guard");
                let original = state.original_projection_context.as_ref().context("live State native context absent")?;
                anyhow::ensure!(original.factory.same_original(&factory)
                    && matches!(&original.authority, OriginalProjectionStateOwner::Live(_))
                    && state.projection.original_factory().is_some_and(|owner| owner.same_original(&factory))
                    && matches!(&state.operational, OperationalAccess::Available(_)),
                    "live State close changed factory or capability ownership");
                original.authority.require_selected_runtime(&state._runtime_state_directory)?;
                Ok(())
            })();
            if let Err(error) = checked { return Err(retain(prior.unwrap_or(error), LiveStateCloseCustody::State(state))); }
            let StateDb { _runtime_state_directory: runtime, _offline_namespace_lock: namespace,
                _cas_directory: cas, _refs_directory: refs, _locators_directory: locators,
                _recovery_directory: recovery_directory, cas_root, refs_root, locators_root,
                projection, original_projection_terminals: previous, original_projection_context: generated,
                operational, recovery, trust_store, head_cache, projection_repair_sink } = state;
            let OperationalAccess::Available(operational) = operational else { unreachable!("validated live Operational owner"); };
            LiveStateCloseProgress {
                context: LiveRetiredStateContext { runtime, _offline_namespace: namespace, cas, refs, locators,
                    recovery_directory, _paths: [cas_root, refs_root, locators_root], previous,
                    generated: generated.expect("validated actual native context"), recovery,
                    _trust: trust_store, _head_cache: head_cache, _repair_sink: projection_repair_sink },
                operational: Some(operational), operational_failure: None, operational_closed: None, operational_outside: None,
                projection: Some(projection), projection_failure: None, projection_closed: None,
            }
        }
        LiveStateCloseCustody::Progress(progress) => progress,
        LiveStateCloseCustody::Closed(closed) => {
            if let Err(error) = closed.progress.context.require_guard(guard).and_then(|()| closed.native_terminals().map(|_| ())) {
                return Err(retain(error, LiveStateCloseCustody::Closed(closed)));
            }
            return Ok(closed);
        }
    };
    let mut progress = std::mem::ManuallyDrop::new(progress);
    if let Err(error) = progress.context.require_guard(guard) {
        return Err(retain(prior.unwrap_or(error), LiveStateCloseCustody::Progress(std::mem::ManuallyDrop::into_inner(progress))));
    }
    if let Some(owner) = progress.operational.take() {
        match owner.close_live(Arc::clone(&factory)) {
            Ok(closed) => progress.operational_closed = Some(closed),
            Err(failure) => progress.operational_failure = Some(failure),
        }
    } else if let Some(failure) = progress.operational_failure.take() {
        match failure.retry_close() {
            Ok(closed) => progress.operational_closed = Some(closed),
            Err(failure) => progress.operational_failure = Some(failure),
        }
    }
    if progress.operational_failure.is_some() {
        return Err(retain(prior.unwrap_or_else(|| anyhow::anyhow!("live Operational close remains pending")), LiveStateCloseCustody::Progress(std::mem::ManuallyDrop::into_inner(progress))));
    }
    if let Some(owner) = progress.projection.take() {
        match owner.close_original(&factory) {
            Ok(closed) => progress.projection_closed = Some(ProjectionTerminal::Closed(closed)),
            Err(failure) => progress.projection_failure = Some(failure),
        }
    } else if let Some(failure) = progress.projection_failure.take() {
        match failure.retry_close(&factory) {
            Ok(closed) => progress.projection_closed = Some(ProjectionTerminal::Recovered(closed)),
            Err(failure) => progress.projection_failure = Some(failure),
        }
    }
    if progress.projection_failure.is_some() {
        return Err(retain(prior.unwrap_or_else(|| anyhow::anyhow!("live projection close remains pending")), LiveStateCloseCustody::Progress(std::mem::ManuallyDrop::into_inner(progress))));
    }
    let closed = ClosedLiveState {
        schema_definitions: std::mem::ManuallyDrop::new(Vec::new()),
        projection_files: std::mem::ManuallyDrop::new(Vec::new()),
        projection_native: std::mem::ManuallyDrop::new(Vec::new()),
        projection_refusals: std::mem::ManuallyDrop::new(Vec::new()),
        projection_sources_started: false,
        destination_join: std::sync::OnceLock::new(), progress, prior_refusal: prior };
    if let Err(error) = closed.native_terminals() { return Err(retain(error, LiveStateCloseCustody::Closed(closed))); }
    Ok(closed)
}
impl StateDb {
    pub fn close_live(self, factory: Arc<OriginalNativeFactory>, guard: &crate::recovery::CasMutationGuard)
        -> std::result::Result<ClosedLiveState, LiveStateCloseFailure> {
        advance_close(factory, LiveStateCloseCustody::State(self), guard, None)
    }
}

/// Quote projection source storage, de-duplication and lease retirement.
/// Charge each full prospective allocation, not just added elements; earlier
/// cursor debits retain the old buffers during a possible relocating growth.
pub fn quote_live_projection_source_transfer(count: usize, lease_count: usize,
    retained_files: usize, retained_native: usize, retained_refusals: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    quote_projection_source_transfer(count, lease_count, retained_files, retained_native,
        retained_refusals, count)
}

/// Startup variant: identical storage and step formula, but the failed-start
/// transfer builds no live `previous` projection vector.
pub(super) fn quote_startup_projection_source_transfer(count: usize, lease_count: usize,
    retained_files: usize, retained_native: usize, retained_refusals: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    quote_projection_source_transfer(count, lease_count, retained_files, retained_native,
        retained_refusals, 0)
}

fn quote_projection_source_transfer(count: usize, lease_count: usize,
    retained_files: usize, retained_native: usize, retained_refusals: usize, previous: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let memory = retained_files.checked_add(count)
        .and_then(|count| count.checked_mul(std::mem::size_of::<crate::projection::OutsideOriginalProjectionCustody>()))
        .and_then(|bytes| bytes.checked_add(retained_native.checked_add(count)?.checked_mul(std::mem::size_of::<OriginalNativeConnectionTerminal>())?))
        .and_then(|bytes| bytes.checked_add(retained_refusals.checked_add(count)?.checked_mul(std::mem::size_of::<anyhow::Error>())?))
        .and_then(|bytes| bytes.checked_add(previous.checked_mul(std::mem::size_of::<Arc<ClosedOriginalProjection>>())?))
        .and_then(|bytes| bytes.checked_add(lease_count.checked_mul(std::mem::size_of::<Arc<crate::projection::OriginalGenerationLease>>())?))
        .and_then(|bytes| bytes.checked_add(5 * std::mem::size_of::<Vec<()>>()))
        .context("live projection full allocation quote overflow")?;
    let steps = u64::try_from(count)?.checked_mul(u64::try_from(count)?)
        .and_then(|work| work.checked_add(u64::try_from(lease_count).ok()?))
        .and_then(|work| work.checked_add(8))
        .context("live projection source work overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(memory)?, steps,
    })
}

pub fn quote_live_schema_source_retirement(
    incoming: usize,
    retained_definitions: usize,
    retained_native: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let definitions = retained_definitions.checked_add(incoming)
        .context("live schema definition quote overflow")?;
    let native = retained_native.checked_add(incoming)
        .context("live schema terminal quote overflow")?;
    let bytes = definitions.checked_mul(std::mem::size_of::<crate::sqlite_schema::SchemaSqlDefinition>())
        .and_then(|bytes| bytes.checked_add(native.checked_mul(std::mem::size_of::<OriginalNativeConnectionTerminal>())?))
        .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<Vec<()>>()))
        .context("live schema full allocation quote overflow")?;
    let incoming = u64::try_from(incoming)?;
    let steps = incoming.checked_mul(incoming).and_then(|work| work.checked_add(4))
        .context("live schema scan work overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?, steps,
    })
}

pub fn quote_closed_live_state_physical_custody()
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let bytes = u64::try_from(std::mem::size_of::<ClosedLiveStatePhysicalCustody>())?
        .checked_add(super::live_native::directory_wrapper_storage_bound(5)?)
        .context("normal State physical storage quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes, steps: 32 })
}

pub fn quote_normal_state_root_membership(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    additional_sources: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(additional_sources <= 6,
        "normal State root quote exceeds five directories and its namespace");
    registry.membership_work_bound(12, additional_sources)
}

#[cfg(test)]
mod outside_quote_tests {
    use super::*;
    use lillux_sqlite::original_physical_custody::OutsideWork;
    use std::mem::size_of;

    #[test]
    fn live_state_retirement_quotes_cover_retained_storage_and_scans() {
        let schema = quote_live_schema_source_retirement(2, 3, 4).unwrap();
        let schema_bytes = 5 * size_of::<crate::sqlite_schema::SchemaSqlDefinition>()
            + 6 * size_of::<OriginalNativeConnectionTerminal>()
            + 2 * size_of::<Vec<()>>();
        assert_eq!(schema, OutsideWork { bytes: schema_bytes as u64, steps: 8 });
        assert!(quote_live_schema_source_retirement(usize::MAX, 0, 0).is_err());

        let projection = quote_live_projection_source_transfer(2, 1, 3, 4, 5).unwrap();
        let projection_bytes = 5 * size_of::<crate::projection::OutsideOriginalProjectionCustody>()
            + 6 * size_of::<OriginalNativeConnectionTerminal>()
            + 7 * size_of::<anyhow::Error>()
            + 2 * size_of::<Arc<ClosedOriginalProjection>>()
            + size_of::<Arc<crate::projection::OriginalGenerationLease>>()
            + 5 * size_of::<Vec<()>>();
        assert_eq!(projection, OutsideWork { bytes: projection_bytes as u64, steps: 13 });
        let startup = quote_startup_projection_source_transfer(2, 1, 3, 4, 5).unwrap();
        let startup_bytes = projection_bytes - 2 * size_of::<Arc<ClosedOriginalProjection>>();
        assert_eq!(startup, OutsideWork { bytes: startup_bytes as u64, steps: 13 });

        let physical = quote_closed_live_state_physical_custody().unwrap();
        let physical_bytes = size_of::<ClosedLiveStatePhysicalCustody>() as u64
            + super::super::live_native::directory_wrapper_storage_bound(5).unwrap();
        assert_eq!(physical, OutsideWork { bytes: physical_bytes, steps: 32 });
    }

    #[test]
    fn live_state_root_receipt_quote_covers_normal_and_shared_parent_owners() {
        let normal = super::super::live_native::quote_state_root_receipt_owner(5, 1).unwrap();
        let recovery = super::super::live_native::quote_state_root_receipt_owner(2, 0).unwrap();
        let operational = crate::operational::quote_operational_retired_shared_roots().unwrap();
        assert_eq!(normal, OutsideWork { bytes: 0, steps: 100 });
        assert_eq!(recovery, OutsideWork { bytes: 0, steps: 36 });
        assert_eq!(operational, OutsideWork { bytes: 0, steps: 36 });
        assert_eq!(crate::operational::quote_operational_marker_receipt(),
            OutsideWork { bytes: 0, steps: 16 });
        assert_eq!(super::super::live_native::quote_live_state_finalization_root_receipts().unwrap(),
            OutsideWork { bytes: 0, steps: 328 });
    }
}

impl NormalStatePhysicalRoots {
    fn require_exhausted(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id() && self.retired_sources_consumed
            && self.directories.iter().all(Option::is_none) && self.namespace.is_none()
            && self.registrations.iter().all(Option::is_none),
            "normal State roots retain a source or registration at final exhaustion");
        registry.require_same_registry(self.registry_identity.as_ref()
            .context("normal State root registry identity absent at exhaustion")?)
    }
}

impl ClosedLiveStatePhysicalCustody {
    /// Consume the whole normal State frame after its native-source,
    /// projection-root and root receipt stages. Only cached receipts and
    /// copied identities are read. Refusal returns the frame intact.
    pub fn into_exhausted(
        self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> std::result::Result<(), (Self, anyhow::Error)> {
        let checked = (|| -> anyhow::Result<()> {
            anyhow::ensure!(self.process == std::process::id(), "foreign closed State frame at exhaustion");
            self.native_retired.as_ref()
                .context("closed State native-source receipt absent at exhaustion")?
                .require_retired(prepay)?;
            self.metadata_pins.require_exhausted()?;
            self.projection_physical.require_exhausted()?;
            anyhow::ensure!(self.parent.is_none(), "closed State parent body remains at exhaustion");
            (&*self.parent_parts).as_ref()
                .context("closed State parent parts absent at exhaustion")?
                .require_exhausted(registry)?;
            self.join.require_exhausted(registry)?;
            self._roots.require_exhausted(registry)?;
            anyhow::ensure!(self.recovery_aliases_retired && self._recovery.1.is_none(),
                "closed State retains a RecoveryStore alias at exhaustion");
            self.recovery_registrations.require_exhausted(registry)?;
            anyhow::ensure!(self._factory.is_none()
                && self._native.0.is_empty() && self._native.1.is_empty()
                && self._native.2.is_empty() && self._native.3.is_empty()
                && self._schema.0.try_lock()
                    .map_err(|_| anyhow::anyhow!("closed State schema census busy or poisoned"))?.is_empty()
                && self._schema.1.is_empty()
                && self._leases.0.try_lock()
                    .map_err(|_| anyhow::anyhow!("closed State lease census busy or poisoned"))?.is_empty()
                && self._projection.0.is_empty() && self._projection.1.is_empty()
                && self._projection.2.is_empty() && self._refusal.is_none(),
                "closed State frame retains a factory, native, schema, lease or projection owner");
            if let Some(operational) = (&*self.operational).as_ref() {
                operational.require_exhausted(registry)?;
            }
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }
        let Self { process: _, projection_physical, parent, parent_parts, join, _roots, _recovery,
            recovery_aliases_retired: _, recovery_registrations, _controls, _factory, _native,
            _schema, _leases, _projection, operational, _refusal, metadata_pins, native_retired } = self;
        drop(std::mem::ManuallyDrop::into_inner(projection_physical));
        drop(std::mem::ManuallyDrop::into_inner(parent));
        if let Some(parts) = std::mem::ManuallyDrop::into_inner(parent_parts) {
            parts.exhaust();
        }
        drop(join);
        drop(std::mem::ManuallyDrop::into_inner(_roots));
        drop(std::mem::ManuallyDrop::into_inner(_recovery));
        drop(recovery_registrations);
        // Trust map, head cache and repair sink are in-memory controls only.
        drop(std::mem::ManuallyDrop::into_inner(_controls));
        drop(_factory);
        drop(std::mem::ManuallyDrop::into_inner(_native));
        drop(std::mem::ManuallyDrop::into_inner(_schema));
        drop(std::mem::ManuallyDrop::into_inner(_leases));
        drop(std::mem::ManuallyDrop::into_inner(_projection));
        if let Some(operational) = std::mem::ManuallyDrop::into_inner(operational) {
            operational.exhaust();
        }
        drop(std::mem::ManuallyDrop::into_inner(_refusal));
        drop(metadata_pins);
        drop(native_retired);
        Ok(())
    }
}
