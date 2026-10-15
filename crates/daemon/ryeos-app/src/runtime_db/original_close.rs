//! Consuming native Runtime close, with the original incarnation retained.
//! A closed frame permits descriptor-only acknowledgement, never more SQL.
use super::*;
use lillux_sqlite::{
    ClosedOriginalNativeConnection, OriginalNativeCloseFailure, OriginalNativeConnectionTerminal,
    OriginalNativeDomain, OriginalNativeFactory,
};
use lillux_sqlite::original_physical_custody::{OriginalPhysicalRegistry, OutsideWork};
use std::sync::{Arc, Mutex};

/// Quote one live or failed-start Runtime outside frame before it is retained.
pub fn quote_runtime_outside_frame() -> Result<OutsideWork> {
    Ok(OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<OriginalLiveRuntimeOutsideCustody>())?,
        steps: 16,
    })
}

pub fn quote_runtime_acquisition_outside_frame() -> Result<OutsideWork> {
    Ok(OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<OriginalRuntimeAcquisitionOutsideCustody>())?,
        steps: 16,
    })
}

/// Quote the physical host continuation allocated by either Runtime route.
pub fn quote_runtime_host_context() -> Result<OutsideWork> {
    Ok(OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<OriginalRuntimeOutsideHostContext>())?,
        steps: 8,
    })
}

/// Quote one original Runtime root/namespace membership census, including its
/// fixed role checks. The registry quote accounts each distinct invocation.
pub fn quote_runtime_membership(
    registry: &OriginalPhysicalRegistry,
    queries: u64,
) -> Result<OutsideWork> {
    registry.membership_work_bound(queries, 0)?.checked_add(OutsideWork {
        bytes: 0,
        steps: 8,
    })
}

/// Quote the fixed Runtime directory/namespace receipts and the single
/// optional unused-open root registration from the same retired registry.
/// Each exact receipt performs a bounded 18-step owner/role/identity join.
pub fn quote_runtime_retired_shared_roots(additional_unused_roots: usize) -> Result<OutsideWork> {
    ensure!(additional_unused_roots <= MAX_UNUSED_RUNTIME_ROOT_REGISTRATIONS,
        "Runtime retired-root quote exceeds its one unused-open source");
    let registrations = 2usize.checked_add(additional_unused_roots)
        .context("Runtime retired-root registration count overflow")?;
    let steps = u64::try_from(registrations)?.checked_mul(18)
        .context("Runtime retired-root receipt quote overflow")?;
    Ok(OutsideWork { bytes: 0, steps })
}

pub fn quote_runtime_directory_transfer() -> Result<OutsideWork> {
    Ok(OutsideWork {
        bytes: u64::try_from(
            std::mem::size_of::<lillux::PinnedDirectory>() + 2 * std::mem::size_of::<usize>(),
        )?,
        steps: 4,
    })
}

pub fn quote_runtime_packet_sources(current: usize, incoming: usize) -> Result<OutsideWork> {
    let prospective = current
        .checked_add(incoming)
        .context("live Runtime packet quote overflow")?;
    ensure!(
        prospective <= MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES,
        "live Runtime packet quote exceeds the fixed main/schema owner slots"
    );
    let bytes = prospective
        .checked_mul(std::mem::size_of::<lillux_sqlite::OriginalNativeOutsideConnectionCustody>())
        .and_then(|n| n.checked_add(std::mem::size_of::<Vec<()>>()))
        .context("live Runtime packet storage overflow")?;
    let steps = u64::try_from(incoming)?
        .checked_mul(8)
        .and_then(|n| n.checked_add(16))
        .context("live Runtime packet work overflow")?;
    Ok(OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps,
    })
}

pub fn quote_unused_runtime_bound_parts() -> Result<OutsideWork> {
    Ok(OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<OriginalUnusedRuntimeBoundParts>())?,
        steps: 8,
    })
}

struct CloseContext {
    factory: Arc<OriginalNativeFactory>,
    directory: lillux::PinnedDirectory,
    namespace: lillux::PinnedDirectoryLock,
    target: PathBuf,
    incarnation: RuntimeDbIncarnation,
    domain: OriginalNativeDomain,
    _schema_reference: Option<sqlite_schema::ClosedNativeSchemaSqlReference>,
}

pub(crate) struct OriginalRuntimeClosed {
    context: CloseContext,
    proof: Arc<ClosedOriginalNativeConnection>,
    prior_refusal: Option<anyhow::Error>,
}
impl OriginalRuntimeClosed {
    pub(crate) fn terminal(&self) -> OriginalNativeConnectionTerminal {
        OriginalNativeConnectionTerminal::Closed(Arc::clone(&self.proof))
    }
    pub(crate) fn native_terminals(&self) -> Result<Vec<OriginalNativeConnectionTerminal>> {
        self.require_binding()?;
        let mut terminals = vec![self.terminal()];
        if let Some(reference) = &self.context._schema_reference {
            reference.require_original_closed(&self.context.factory, OriginalNativeDomain::NodeRuntimeSchemaReference)?;
            terminals.push(reference.terminal().clone());
        }
        Ok(terminals)
    }

    pub(crate) fn require_reset_incarnation(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        token: &RuntimeDbIncarnation,
        target: &Path,
    ) -> Result<()> {
        ensure!(self.context.domain == OriginalNativeDomain::RuntimeMutation,
            "live Runtime close cannot authorize offline reset acknowledgement");
        ensure!(
            self.context.factory.same_original(factory),
            "closed Runtime factory changed"
        );
        ensure!(
            Arc::ptr_eq(&self.context.incarnation.0, &token.0) && self.context.target == target,
            "closed Runtime incarnation or target changed"
        );
        self.require_binding()
    }
    fn require_binding(&self) -> Result<()> {
        self.proof.require_original(&self.context.factory)?;
        ensure!(
            self.proof.domain() == self.context.domain,
            "closed Runtime proof belongs to another native domain"
        );
        self.context
            .namespace
            .ensure_protects(&self.context.directory)?;
        self.context.directory.ensure_path_binding()?;
        let name = self
            .context
            .target
            .file_name()
            .context("closed Runtime basename absent")?;
        let observed = self
            .context
            .directory
            .entry_no_follow(name)?
            .context("closed Runtime target absent")?;
        ensure!(
            observed.entry_type == lillux::secure_fs::PinnedEntryType::Regular
                && self.proof.bound_main_identity()
                    == Some((observed.containing_device, observed.inode)),
            "closed Runtime target differs from the SAME retired Bound keeper"
        );
        Ok(())
    }
}

/// Complete actual live Runtime frame after native closure. Every physical
/// owner and semantic component remains outside custody; Drop closes nothing.
pub(crate) struct OriginalLiveRuntimeOutsideCustody {
    inner: lillux::custody::Held<OriginalLiveRuntimeOutsideParts>,
}
const LIVE_RUNTIME_OUTSIDE_KIND: &str = "ryeos-app::live-runtime-outside-custody";

/// Everything the live Runtime frame holds, in the order its release drops
/// it. Constructible only in this module, so no caller can swap the whole out
/// from under its custody.
pub(crate) struct OriginalLiveRuntimeOutsideParts {
    context: Option<CloseContext>,
    host: Option<OriginalRuntimeOutsideHostContext>,
    prior_refusal: Option<anyhow::Error>,
    sources: LiveRuntimeNativeSources,
}
impl OriginalLiveRuntimeOutsideParts {
    /// Final exhaustion without the process check: no close context, sources
    /// and host frame exhausted, and a refusal that retains no owner.
    fn require_exhausted_holders(&self) -> Result<()> {
        ensure!(self.context.is_none(), "Runtime close context remains at exhaustion");
        self.sources.require_exhausted()?;
        self.host.as_ref().context("Runtime host frame absent at exhaustion")?
            .require_exhausted()?;
        ryeos_state::state_db::require_exhausted_refusals(self.prior_refusal.iter())
    }
}
/// Settled exactly when final exhaustion would accept the frame; otherwise the
/// close context, host frame, refusal and native sources stay retained
/// together until the process exits.
fn live_runtime_outside_exhausted(parts: &OriginalLiveRuntimeOutsideParts) -> bool {
    parts.require_exhausted_holders().is_ok()
}
impl std::ops::Deref for OriginalLiveRuntimeOutsideCustody {
    type Target = OriginalLiveRuntimeOutsideParts;
    fn deref(&self) -> &OriginalLiveRuntimeOutsideParts { &self.inner }
}
impl OriginalLiveRuntimeOutsideCustody {
    fn parts_mut(&mut self) -> &mut OriginalLiveRuntimeOutsideParts { self.inner.get_mut() }
}
impl std::fmt::Debug for OriginalLiveRuntimeOutsideCustody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalLiveRuntimeOutsideCustody").field("process", &self.inner.issuer()).finish_non_exhaustive()
    }
}
impl OriginalRuntimeClosed {
    pub(crate) fn into_live_outside_custody(self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<OriginalLiveRuntimeOutsideCustody, (Self, anyhow::Error)> {
        let checked = lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            factory.require_original_process()?;
            ensure!(Arc::ptr_eq(&self.context.factory, factory), "live Runtime outside selects another original factory");
            aggregate.require_original(factory)?;
            self.proof.require_original(factory)?;
            ensure!(self.context.domain == OriginalNativeDomain::NodeRuntime
                && self.proof.domain() == OriginalNativeDomain::NodeRuntime,
                "offline Runtime cannot become a live outside frame");
            self.context._schema_reference.as_ref().context("actual live Runtime schema receipt absent")?
                .require_original_closed(factory, OriginalNativeDomain::NodeRuntimeSchemaReference)?;
            // Keep native proof/Bound inputs (including the actual producer
            // authority) whole; no Ready/path rebinding or parent recreation.
            let quote = quote_runtime_outside_frame()?;
            prepay(quote.bytes, quote.steps)?;
            factory.require_original_process()
        });
        if let Err(error) = checked { return Err((self, error)); }
        let Self { context, proof, prior_refusal } = self;
        Ok(OriginalLiveRuntimeOutsideCustody {
            inner: lillux::custody::Held::new(
                LIVE_RUNTIME_OUTSIDE_KIND,
                OriginalLiveRuntimeOutsideParts {
                    context: Some(context),
                    host: None,
                    prior_refusal,
                    sources: LiveRuntimeNativeSources::new(Some(OriginalNativeConnectionTerminal::Closed(proof))),
                },
                live_runtime_outside_exhausted,
            ),
        })
    }
}
impl OriginalLiveRuntimeOutsideCustody {
    /// # Safety
    /// Same actual complete supervisor census and original prepaid registry.
    pub(crate) unsafe fn register_shared_roots(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original(factory, aggregate)?;
        if self.host.is_none() {
            let context = self.context.as_ref().context("actual live Runtime context absent")?;
            ensure!(self.sources.complete(&context._schema_reference), "live Runtime physical handoff precedes actual native sources");
            let quote = quote_runtime_host_context()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            ensure!(!self.inner.is_foreign(), "foreign live Runtime after host precharge");
            factory.require_original_process()?;
            let context = self.parts_mut().context.take().expect("whole actual live Runtime context checked");
            let CloseContext { factory: original_factory, directory, namespace, target, incarnation, domain, _schema_reference: _ } = context;
            self.parts_mut().host = Some(OriginalRuntimeOutsideHostContext::new(
                Arc::downgrade(&original_factory),
                Some(OriginalRuntimeOutsideDirectory::Owned(directory)),
                None,
                Some(namespace),
                OriginalRuntimeOutsideSemantics::Closed { _target: target, _incarnation: incarnation, _domain: domain },
            ));
            // SAME actual caller factory remains retained. No physical field
            // is dropped when this completed redundant facade alias ends.
            drop(original_factory);
        }
        unsafe { self.parts_mut().host.as_mut().unwrap().register_shared_roots(registry, prepay) }
    }
    pub(crate) fn require_shared_roots_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign Runtime source registration census");
        ensure!(self.context.is_none() && self.sources.complete(&None) && self.sources.packets.is_empty(),
            "Runtime retains original context or undelivered native sources");
        self.host.as_ref().context("actual Runtime host continuation absent")?
            .require_shared_roots_registered(registry, prepay)
    }
    pub(crate) fn consume_retired_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign live Runtime root finalization");
        ensure!(self.context.is_none() && self.sources.complete(&None)
            && self.sources.packets.is_empty(),
            "Runtime root retirement precedes native-source and packet joins");
        self.parts_mut().host.as_mut().context("actual Runtime host root frame absent")?
            .consume_retired_shared_roots(registry, prepay)
    }
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign original live Runtime outside frame");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if let Some(context) = self.context.as_ref() {
            ensure!(Arc::ptr_eq(&context.factory, factory) && context.domain == OriginalNativeDomain::NodeRuntime,
                "live Runtime outside frame has another actual factory/domain");
        } else {
            self.host.as_ref().context("actual live Runtime host context absent")?.require_original(factory)?;
        }
        Ok(())
    }
    pub(crate) fn advance_native_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original(factory, aggregate)?;
        let parts = self.parts_mut();
        if let Some(context) = parts.context.as_mut() {
            parts.sources.advance(factory, aggregate, &mut context._schema_reference, prepay)
        } else {
            ensure!(self.sources.complete(&None), "live Runtime host handoff has unresolved native sources");
            Ok(())
        }
    }
    /// # Safety
    /// Caller caches the WHOLE actual packet vector before future effects.
    /// This is a resource transfer, never physical/account completion.
    pub(crate) unsafe fn take_original_sql_custody(&mut self)
        -> Result<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>> {
        ensure!(!self.inner.is_foreign(), "foreign live Runtime packet delivery");
        ensure!(self.sources.main.is_none() && self.sources.schema.is_none() && self.sources.native_failure.is_none()
            && self.context.as_ref().is_none_or(|context| context._schema_reference.is_none()), "actual live Runtime native sources remain retained");
        ensure!(self.sources.packets.len() <= MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES,
            "Runtime packet vector exceeds its fixed main/schema owner slots");
        Ok(self.parts_mut().sources.take_packets())
    }
}

enum OriginalRuntimeOutsideDirectory {
    Owned(lillux::PinnedDirectory),
    Shared(Arc<lillux::PinnedDirectory>),
}
enum OriginalRuntimeOutsideSemantics {
    Closed { _target: PathBuf, _incarnation: RuntimeDbIncarnation, _domain: OriginalNativeDomain },
    Refused {
        // A refusal may wrap a native owner, so it stays with the frame:
        // retained whole with an unsettled frame, relinquished whole by a
        // forked copy.
        error: Option<anyhow::Error>,
        unused_inputs: UnusedRuntimeBoundInputs,
        // Its own end releases retired parts and retains unretired ones.
        unused_parts: Option<OriginalUnusedRuntimeBoundParts>,
    },
}
/// The refused acquisition's unused bound input. It leaves only by `take`,
/// to become unused bound parts; dropped while still holding it, wherever
/// its frame or semantics are dropped or replaced, it retains the input for
/// the life of the process, counted.
struct UnusedRuntimeBoundInputs {
    inner: lillux::custody::Held<UnusedRuntimeBoundInputsParts>,
}
const UNUSED_RUNTIME_BOUND_INPUTS_KIND: &str = "ryeos-app::unused-runtime-bound-inputs";

/// The one input slot. Constructible only in this module.
struct UnusedRuntimeBoundInputsParts {
    inputs: Option<lillux_sqlite::OriginalNativeBoundInputs>,
}
fn unused_runtime_bound_inputs_taken(parts: &UnusedRuntimeBoundInputsParts) -> bool {
    parts.inputs.is_none()
}
impl UnusedRuntimeBoundInputs {
    fn new(inputs: Option<lillux_sqlite::OriginalNativeBoundInputs>) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                UNUSED_RUNTIME_BOUND_INPUTS_KIND,
                UnusedRuntimeBoundInputsParts { inputs },
                unused_runtime_bound_inputs_taken,
            ),
        }
    }
    /// Hand the input on. A copy in another process holds none of it.
    fn take(&mut self) -> Option<lillux_sqlite::OriginalNativeBoundInputs> {
        if self.inner.is_foreign() { return None; }
        self.inner.get_mut().inputs.take()
    }
    /// Put back an input a refused extraction returned.
    fn restore(&mut self, inputs: lillux_sqlite::OriginalNativeBoundInputs) {
        self.inner.get_mut().inputs = Some(inputs);
    }
}
impl std::ops::Deref for UnusedRuntimeBoundInputs {
    type Target = Option<lillux_sqlite::OriginalNativeBoundInputs>;
    fn deref(&self) -> &Self::Target { &self.inner.inputs }
}
const UNUSED_RUNTIME_BOUND_PARTS_KIND: &str = "ryeos-app::unused-runtime-bound-parts";

/// Everything the unused bound root holds, in the order its release drops it.
/// Constructible only in this module.
struct OriginalUnusedRuntimeBoundFields {
    root: Option<Arc<lillux::PinnedDirectory>>,
    authority: Option<Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>>,
    inspection: Option<(lillux_sqlite::SqliteCopyLease, Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>)>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
    _name: lillux_sqlite::OriginalNativeOutsideBoundCoordinate,
    mode: lillux_sqlite::OriginalNativeBoundMode,
    domain: OriginalNativeDomain,
}
impl OriginalUnusedRuntimeBoundFields {
    fn aliases_transferred(&self) -> bool {
        self.registration.is_some() && self.root.is_none() && self.authority.is_none() && self.inspection.is_none()
    }
    /// Every alias transferred and the registration consumed: the retired
    /// root frame left nothing behind.
    fn retired(&self) -> bool {
        self.root.is_none() && self.authority.is_none() && self.inspection.is_none() && self.registration.is_none()
    }
}
/// Settled exactly when retired holds: no root, authority, inspection or
/// registration remains. Otherwise the unused bound root, its authority alias,
/// inspection lease and registration stay retained together until the process
/// exits.
struct OriginalUnusedRuntimeBoundParts {
    inner: lillux::custody::Held<OriginalUnusedRuntimeBoundFields>,
}
impl OriginalUnusedRuntimeBoundParts {
    fn new(fields: OriginalUnusedRuntimeBoundFields) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                UNUSED_RUNTIME_BOUND_PARTS_KIND,
                fields,
                OriginalUnusedRuntimeBoundFields::retired,
            ),
        }
    }
    fn aliases_transferred(&self) -> bool { self.inner.aliases_transferred() }
    fn retired(&self) -> bool { self.inner.retired() }
    fn fields_mut(&mut self) -> &mut OriginalUnusedRuntimeBoundFields { self.inner.get_mut() }
}
impl std::ops::Deref for OriginalUnusedRuntimeBoundParts {
    type Target = OriginalUnusedRuntimeBoundFields;
    fn deref(&self) -> &OriginalUnusedRuntimeBoundFields { &self.inner }
}
const RUNTIME_OUTSIDE_HOST_KIND: &str = "ryeos-app::runtime-outside-host-context";

/// Everything the Runtime host frame holds, in the order its release drops
/// it. Constructible only in this module.
struct OriginalRuntimeOutsideHostFields {
    directory: Option<OriginalRuntimeOutsideDirectory>,
    namespace: Option<lillux::PinnedDirectoryLock>,
    semantics: OriginalRuntimeOutsideSemantics,
    registrations: [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>; 2],
    factory: std::sync::Weak<OriginalNativeFactory>,
    shared_directory_identity: Option<std::sync::Weak<lillux::PinnedDirectory>>,
    retired_roots_consumed: bool,
}
impl OriginalRuntimeOutsideHostFields {
    fn unused_runtime_source_is_retired(&self) -> bool {
        match &self.semantics {
            OriginalRuntimeOutsideSemantics::Closed { .. } => true,
            OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. } =>
                unused_inputs.is_none() && unused_parts.as_ref().is_none_or(OriginalUnusedRuntimeBoundParts::retired),
        }
    }
    /// Exhaustion without the process check: retired roots consumed, no
    /// directory, namespace, registration or unused source left, and a refusal
    /// that retains no owner.
    fn require_exhausted(&self) -> Result<()> {
        ensure!(self.retired_roots_consumed && self.directory.is_none() && self.namespace.is_none()
            && self.registrations.iter().all(Option::is_none)
            && self.shared_directory_identity.is_none()
            && self.unused_runtime_source_is_retired(),
            "Runtime host frame retains a root, namespace or registration at exhaustion");
        if let OriginalRuntimeOutsideSemantics::Refused { error, unused_inputs: _, unused_parts: _ } = &self.semantics {
            ryeos_state::state_db::require_exhausted_refusals(error.iter())?;
        }
        Ok(())
    }
}
/// Settled exactly when exhaustion would accept the frame; otherwise the
/// directory, namespace, refusal, registrations and unused source stay
/// retained together until the process exits.
fn host_context_exhausted(fields: &OriginalRuntimeOutsideHostFields) -> bool {
    fields.require_exhausted().is_ok()
}
struct OriginalRuntimeOutsideHostContext {
    inner: lillux::custody::Held<OriginalRuntimeOutsideHostFields>,
}
impl std::ops::Deref for OriginalRuntimeOutsideHostContext {
    type Target = OriginalRuntimeOutsideHostFields;
    fn deref(&self) -> &OriginalRuntimeOutsideHostFields { &self.inner }
}
impl OriginalRuntimeOutsideHostContext {
    fn new(
        factory: std::sync::Weak<OriginalNativeFactory>,
        directory: Option<OriginalRuntimeOutsideDirectory>,
        shared_directory_identity: Option<std::sync::Weak<lillux::PinnedDirectory>>,
        namespace: Option<lillux::PinnedDirectoryLock>,
        semantics: OriginalRuntimeOutsideSemantics,
    ) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                RUNTIME_OUTSIDE_HOST_KIND,
                OriginalRuntimeOutsideHostFields {
                    directory, namespace, semantics,
                    registrations: std::array::from_fn(|_| None),
                    factory, shared_directory_identity,
                    retired_roots_consumed: false,
                },
                host_context_exhausted,
            ),
        }
    }
    fn fields_mut(&mut self) -> &mut OriginalRuntimeOutsideHostFields { self.inner.get_mut() }
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign original Runtime host context");
        factory.require_original_process()?;
        ensure!(self.factory.ptr_eq(&Arc::downgrade(factory)), "Runtime host context changes actual factory allocation");
        Ok(())
    }
    unsafe fn register_shared_roots(&mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign original Runtime physical sources");
        let parts = self.fields_mut();
        ensure!(!parts.retired_roots_consumed,
            "retired Runtime root frame cannot be registered again");
        let existing_checks = parts.registrations.iter().filter(|binding| binding.is_some()).count();
        if existing_checks > 0 {
            let quote = quote_runtime_membership(registry, u64::try_from(existing_checks)?)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        }
        if let Some(binding) = &parts.registrations[0] { registry.require_membership(binding)?; }
        else {
            // Concrete Arc control before wrapping the actual owned FD. No
            // reopened directory or inode/path matching becomes ownership.
            let quote = quote_runtime_directory_transfer()?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            let directory = parts.directory.take().context("actual Runtime directory absent without registration")?;
            let directory = match directory {
                OriginalRuntimeOutsideDirectory::Owned(directory) => Arc::new(directory),
                OriginalRuntimeOutsideDirectory::Shared(directory) => directory,
            };
            parts.shared_directory_identity = Some(Arc::downgrade(&directory));
            match unsafe { registry.register_directory(directory, prepay) } {
                Ok(binding) => parts.registrations[0] = Some(binding),
                Err((directory, error)) => { parts.directory = Some(OriginalRuntimeOutsideDirectory::Shared(directory)); return Err(error); }
            }
        }
        if let Some(binding) = &parts.registrations[1] { registry.require_membership(binding)?; }
        else {
            let namespace = parts.namespace.take().context("actual Runtime namespace absent without registration")?;
            match unsafe { registry.register_namespace(namespace, prepay) } {
                Ok(binding) => parts.registrations[1] = Some(binding),
                Err((namespace, error)) => { parts.namespace = Some(namespace); return Err(error); }
            }
        }
        Ok(())
    }
    fn require_shared_roots_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign Runtime registration census");
        ensure!(!self.retired_roots_consumed,
            "retired Runtime root frame cannot reenter registration census");
        ensure!(self.directory.is_none() && self.namespace.is_none(),
            "Runtime retains unregistered directory or namespace");
        ensure!(!matches!(&self.semantics, OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. }
            if unused_inputs.is_some() || unused_parts.as_ref().is_some_and(|parts| !parts.aliases_transferred())),
            "Runtime retains original unused source aliases");
        ensure!(self.registrations.iter().all(Option::is_some),
            "actual Runtime fixed source registration absent");
        let additional_unused_roots = match &self.semantics {
            OriginalRuntimeOutsideSemantics::Refused { unused_parts, .. }
                if unused_parts.is_some() => 1,
            _ => 0,
        };
        let registration_count = 2usize.checked_add(additional_unused_roots)
            .context("Runtime source registration count overflow")?;
        let quote = quote_runtime_membership(registry, u64::try_from(registration_count)?)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        for binding in &self.registrations {
            registry.require_membership(binding.as_ref().context("actual Runtime fixed source registration absent")?)?;
        }
        if let Some(binding) = self.unused_runtime_root_registration()? {
            registry.require_membership(binding)?;
        }
        Ok(())
    }
    fn unused_runtime_root_registration(
        &self,
    ) -> Result<Option<&lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>> {
        let OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. } = &self.semantics
            else { return Ok(None); };
        ensure!(unused_inputs.is_none(),
            "Runtime source still retains an unused bound input");
        let Some(parts) = unused_parts.as_ref() else { return Ok(None); };
        ensure!(parts.domain == OriginalNativeDomain::NodeRuntime
            && parts.mode == lillux_sqlite::OriginalNativeBoundMode::OpenOrCreate
            && parts.inspection.is_none() && parts.aliases_transferred(),
            "unused Runtime root registration has untransferred or foreign source fields");
        let registration = parts.registration.as_ref()
            .context("unused Runtime root registration absent after alias transfer")?;
        ensure!(matches!(registration.observed_source_identity()?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
            "unused Runtime root registration changed its Directory role");
        Ok(Some(registration))
    }
    fn consume_retired_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign retired Runtime root frame");
        if self.retired_roots_consumed {
            ensure!(self.registrations.iter().all(Option::is_none)
                && self.shared_directory_identity.is_none()
                && self.unused_runtime_source_is_retired(),
                "consumed Runtime root frame regained registration state");
            return Ok(());
        }
        ensure!(self.directory.is_none() && self.namespace.is_none(),
            "retired Runtime root frame still owns an open source");
        ensure!(!matches!(&self.semantics,
            OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. }
                if unused_inputs.is_some()
                    || unused_parts.as_ref().is_some_and(|parts| !parts.aliases_transferred())),
            "retired Runtime root frame retains unused bound-input aliases");
        ensure!(self.registrations.iter().all(Option::is_some)
            && self.shared_directory_identity.is_some(),
            "retired Runtime root registrations or directory identity are absent");
        let additional_unused_roots = match &self.semantics {
            OriginalRuntimeOutsideSemantics::Refused { unused_parts, .. }
                if unused_parts.is_some() => 1,
            _ => 0,
        };
        let quote = quote_runtime_retired_shared_roots(additional_unused_roots)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        ensure!(!self.inner.is_foreign(),
            "foreign Runtime root frame after retirement debit");

        let unused_root = self.unused_runtime_root_registration()?;
        for (index, registration) in self.registrations.iter().enumerate() {
            let registration = registration.as_ref()
                .context("retired Runtime root registration disappeared")?;
            let identity = registration.observed_source_identity()?;
            ensure!(matches!((index, identity),
                (0, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
                | (1, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })),
                "retired Runtime source registration changed role");
            drop(registration.receipt_in(registry)?);
        }
        if let Some(registration) = unused_root {
            drop(registration.receipt_in(registry)?);
        }

        let fields = self.fields_mut();
        fields.registrations = std::array::from_fn(|_| None);
        if let OriginalRuntimeOutsideSemantics::Refused { unused_parts, .. } = &mut fields.semantics {
            if let Some(parts) = unused_parts.as_mut() {
                parts.fields_mut().registration = None;
            }
        }
        fields.shared_directory_identity = None;
        fields.retired_roots_consumed = true;
        Ok(())
    }
}
const MAX_UNUSED_RUNTIME_ROOT_REGISTRATIONS: usize = 1;
pub const MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES: usize = 2;

const LIVE_RUNTIME_NATIVE_SOURCES_KIND: &str = "ryeos-app::live-runtime-native-sources";

/// Every native source slot, in the order their release drops them.
/// Constructible only in this module.
struct LiveRuntimeNativeSourceSlots {
    main: Option<OriginalNativeConnectionTerminal>,
    schema: Option<OriginalNativeConnectionTerminal>,
    schema_definition: Option<sqlite_schema::SchemaSqlDefinition>,
    native_failure: Option<(usize, lillux_sqlite::OriginalNativeOutsideCustodyFailure)>,
    packets: Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
}
impl LiveRuntimeNativeSourceSlots {
    fn complete(&self, reference: &Option<sqlite_schema::ClosedNativeSchemaSqlReference>) -> bool {
        self.main.is_none() && self.schema.is_none() && self.native_failure.is_none() && reference.is_none()
    }
    /// No main, schema or failed source and no undelivered packet.
    fn exhausted(&self) -> bool {
        self.complete(&None) && self.packets.is_empty()
    }
}
/// Settled exactly when exhaustion would accept the sources; otherwise the
/// terminals, definition, failure and packets stay retained together until
/// the process exits.
struct LiveRuntimeNativeSources {
    inner: lillux::custody::Held<LiveRuntimeNativeSourceSlots>,
}
impl std::ops::Deref for LiveRuntimeNativeSources {
    type Target = LiveRuntimeNativeSourceSlots;
    fn deref(&self) -> &LiveRuntimeNativeSourceSlots { &self.inner }
}
impl LiveRuntimeNativeSources {
    fn new(main: Option<OriginalNativeConnectionTerminal>) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                LIVE_RUNTIME_NATIVE_SOURCES_KIND,
                LiveRuntimeNativeSourceSlots {
                    main, schema: None, schema_definition: None, native_failure: None,
                    packets: Vec::new(),
                },
                LiveRuntimeNativeSourceSlots::exhausted,
            ),
        }
    }
    /// Hand the whole packet vector on. A copy in another process holds none.
    fn take_packets(&mut self) -> Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody> {
        if self.inner.is_foreign() { return Vec::new(); }
        std::mem::take(&mut self.inner.get_mut().packets)
    }
    fn advance(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        reference: &mut Option<sqlite_schema::ClosedNativeSchemaSqlReference>,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        factory.require_original_process()?;
        ensure!(!self.inner.is_foreign(), "foreign live Runtime native sources");
        let slots = self.inner.get_mut();
        aggregate.require_original(factory)?;
        let count = usize::from(slots.main.is_some()) + usize::from(slots.schema.is_some())
            + usize::from(reference.is_some()) + usize::from(slots.native_failure.is_some());
        if count == 0 { return Ok(()); }
        let quote = quote_runtime_packet_sources(slots.packets.len(), count)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        factory.require_original_process()?;
        ensure!(slots.packets.len().checked_add(count).is_some_and(|n|
            n <= MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES),
            "live Runtime native packet source bound changed after funding");
        slots.packets.try_reserve_exact(count)?;
        if let Some(reference) = reference.as_ref() {
            reference.require_original_closed(factory, OriginalNativeDomain::NodeRuntimeSchemaReference)?;
        }
        if let Some((index, failure)) = slots.native_failure.take() {
            if index >= 2 {
                slots.native_failure = Some((index, failure));
                bail!("actual live Runtime failure selects unknown source slot");
            }
            let slot = if index == 0 { &mut slots.main } else { &mut slots.schema };
            if slot.is_some() {
                slots.native_failure = Some((index, failure));
                bail!("live Runtime failure recovery conflicts with actual source slot");
            }
            match failure.into_original() {
                Ok(original) => *slot = Some(original),
                Err(failure) => { slots.native_failure = Some((index, failure)); bail!("live Runtime whole source recovery refused"); }
            }
        }
        // Validate every existing source before consuming any of this batch.
        for (index, source) in [&slots.main, &slots.schema].into_iter().enumerate() {
            if let Some(source) = source.as_ref() {
                source.require_original(factory)?;
                let expected = if index == 0 { OriginalNativeDomain::NodeRuntime } else { OriginalNativeDomain::NodeRuntimeSchemaReference };
                match source {
                    OriginalNativeConnectionTerminal::Closed(original) => ensure!(original.domain() == expected,
                        "live Runtime native source changes original purpose"),
                    OriginalNativeConnectionTerminal::Released(original) => {
                        ensure!(index == 0 && original.original_domain()?.is_none_or(|domain| domain == expected),
                            "live Runtime released source changes original purpose");
                    }
                }
            }
        }
        if reference.is_some() {
            ensure!(slots.schema.is_none() && slots.schema_definition.is_none(),
                "actual live Runtime schema source conflicts with retained original reference");
        }
        if let Some(reference) = reference.take() {
            let (definition, source) = reference.into_original_parts();
            slots.schema_definition = Some(definition);
            slots.schema = Some(source);
        }
        for index in 0..2 {
            let slot = if index == 0 { &mut slots.main } else { &mut slots.schema };
            if let Some(source) = slot.take() {
                match source.into_outside_custody(aggregate) {
                    Ok(packet) => slots.packets.push(packet),
                    Err(failure) => {
                        slots.native_failure = Some((index, failure));
                        bail!("live Runtime actual native source retained after outside handoff refusal");
                    }
                }
            }
        }
        Ok(())
    }
}
/// Actual refused live acquisition body, including error/root/namespace.
/// A no-main-source stage is not successful acquisition or physical disposal.
pub(crate) struct OriginalRuntimeAcquisitionOutsideCustody {
    inner: lillux::custody::Held<OriginalRuntimeAcquisitionOutsideParts>,
}
const RUNTIME_ACQUISITION_OUTSIDE_KIND: &str = "ryeos-app::runtime-acquisition-outside-custody";

/// Everything the refused-acquisition frame holds, in the order its release
/// drops it. Constructible only in this module.
pub(crate) struct OriginalRuntimeAcquisitionOutsideParts {
    body: Option<super::original_native::OriginalRuntimeAcquisitionReleased>,
    host: Option<OriginalRuntimeOutsideHostContext>,
    sources: LiveRuntimeNativeSources,
}
impl OriginalRuntimeAcquisitionOutsideParts {
    /// Final exhaustion without the process check: no refused body, sources
    /// and host frame exhausted.
    fn require_exhausted_holders(&self) -> Result<()> {
        ensure!(self.body.is_none(), "refused Runtime acquisition body remains at exhaustion");
        self.sources.require_exhausted()?;
        self.host.as_ref().context("refused Runtime host frame absent at exhaustion")?
            .require_exhausted()
    }
}
/// Settled exactly when final exhaustion would accept the frame; otherwise
/// the refused body, host frame and native sources stay retained together
/// until the process exits.
fn acquisition_outside_exhausted(parts: &OriginalRuntimeAcquisitionOutsideParts) -> bool {
    parts.require_exhausted_holders().is_ok()
}
impl std::ops::Deref for OriginalRuntimeAcquisitionOutsideCustody {
    type Target = OriginalRuntimeAcquisitionOutsideParts;
    fn deref(&self) -> &OriginalRuntimeAcquisitionOutsideParts { &self.inner }
}
impl OriginalRuntimeAcquisitionOutsideCustody {
    fn parts_mut(&mut self) -> &mut OriginalRuntimeAcquisitionOutsideParts { self.inner.get_mut() }
}
impl std::fmt::Debug for OriginalRuntimeAcquisitionOutsideCustody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalRuntimeAcquisitionOutsideCustody").field("process", &self.inner.issuer()).finish_non_exhaustive()
    }
}
impl super::original_native::OriginalRuntimeAcquisitionReleased {
    pub(crate) fn into_live_outside_custody(mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<OriginalRuntimeAcquisitionOutsideCustody, (Self, anyhow::Error)> {
        let checked = lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            factory.require_original_process()?;
            ensure!(Arc::ptr_eq(&self.factory, factory), "refused Runtime selects another original factory");
            aggregate.require_original(factory)?;
            if let Some(original) = &self.terminal {
                original.require_original(factory)?;
                match original {
                    OriginalNativeConnectionTerminal::Closed(original) => ensure!(original.domain() == OriginalNativeDomain::NodeRuntime,
                        "offline closed acquisition cannot become live outside custody"),
                    OriginalNativeConnectionTerminal::Released(original) => ensure!(original.original_domain()?.is_none_or(|domain| domain == OriginalNativeDomain::NodeRuntime),
                        "offline released acquisition cannot become live outside custody"),
                }
            }
            if let Some(reference) = &self._schema_reference {
                reference.require_original_closed(factory, OriginalNativeDomain::NodeRuntimeSchemaReference)?;
            }
            let quote = quote_runtime_acquisition_outside_frame()?;
            prepay(quote.bytes, quote.steps)?;
            factory.require_original_process()
        });
        if let Err(error) = checked { return Err((self, error)); }
        let sources = LiveRuntimeNativeSources::new(self.terminal.take());
        Ok(OriginalRuntimeAcquisitionOutsideCustody {
            inner: lillux::custody::Held::new(
                RUNTIME_ACQUISITION_OUTSIDE_KIND,
                OriginalRuntimeAcquisitionOutsideParts { body: Some(self), host: None, sources },
                acquisition_outside_exhausted,
            ),
        })
    }
}
impl OriginalRuntimeAcquisitionOutsideCustody {
    /// # Safety
    /// The Store retains the SAME actual canonical LiveStateOpen parent while
    /// this finite duplicate source joins its original prepaid registry.
    pub(crate) unsafe fn retire_unused_bound_input_aliases(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        canonical_parent: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign unused Runtime input handoff");
        factory.require_original_process()?; aggregate.require_original(factory)?;
        // Normalize/cache the complete actual refused context first. Its
        // native sources already closed genuinely; no new ticket is created.
        unsafe { self.register_shared_roots(factory, aggregate, registry, prepay) }?;
        let host = self.parts_mut().host.as_mut().context("actual original Runtime host context absent")?;
        host.require_original(factory)?;
        let host = host.fields_mut();
        let original_root = host.shared_directory_identity.as_ref()
            .context("actual Runtime source directory allocation identity absent")?;
        let OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. } = &mut host.semantics else {
            bail!("unused Runtime source selected a successful close frame");
        };
        if unused_inputs.is_some() {
            ensure!(unused_parts.is_none(), "unused Runtime source conflicts with original retained parts");
            let quote = quote_unused_runtime_bound_parts()?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            let original = unused_inputs.take().expect("actual unused original inputs checked");
            let original = match original.into_original_parts() {
                Ok(original) => original,
                Err(original) => {
                    unused_inputs.restore(original);
                    bail!("actual unused Runtime bound input extraction refused");
                }
            };
            *unused_parts = Some(OriginalUnusedRuntimeBoundParts::new(OriginalUnusedRuntimeBoundFields {
                root: Some(original.root),
                authority: Some(original.authority),
                inspection: original.inspection,
                registration: None,
                _name: original.name, mode: original.mode, domain: original.domain,
            }));
        }
        let Some(original) = unused_parts.as_mut() else { return Ok(()); };
        ensure!(!original.inner.is_foreign(), "foreign unused Runtime source after precharge");
        let original = original.fields_mut();
        ensure!(original.domain == OriginalNativeDomain::NodeRuntime
            && original.mode == lillux_sqlite::OriginalNativeBoundMode::OpenOrCreate
            && original.inspection.is_none(), "unused Runtime bound source has unexpected original purpose/mode/context");
        if original.aliases_transferred() {
            let quote = quote_runtime_membership(registry, 1)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            registry.require_membership(original.registration.as_ref().unwrap())?;
            return Ok(());
        }
        let authority = original.authority.as_ref().context("actual unused Runtime authority absent")?;
        ensure!(Arc::ptr_eq(authority, canonical_parent), "unused Runtime source selects different actual canonical parent");
        if original.registration.is_none() {
            let root = original.root.as_ref().context("actual unused Runtime root absent")?;
            ensure!(original_root.ptr_eq(&Arc::downgrade(root)),
                "unused Runtime bound root is not the SAME retained source allocation");
            let root = original.root.take().expect("actual unused root verified");
            match unsafe { registry.register_directory(root, prepay) } {
                Ok(binding) => original.registration = Some(binding),
                Err((root, error)) => { original.root = Some(root); return Err(error); }
            }
        } else {
            let quote = quote_runtime_membership(registry, 1)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            registry.require_membership(original.registration.as_ref().unwrap())?;
        }
        // SAME canonical parent remains borrowed/retained throughout. Actual
        // root already retained by the registry before this duplicate ends.
        original.authority.take();
        Ok(())
    }

    /// # Safety
    /// Same actual failed-start supervisor, sealed census and prepaid registry.
    pub(crate) unsafe fn register_shared_roots(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign refused Runtime physical handoff");
        factory.require_original_process()?; aggregate.require_original(factory)?;
        if self.host.is_none() {
            let body = self.body.as_ref().context("actual refused Runtime body absent")?;
            ensure!(Arc::ptr_eq(&body.factory, factory) && self.sources.complete(&body._schema_reference)
                && body.terminal.is_none(), "refused Runtime physical handoff precedes actual native sources");
            let quote = quote_runtime_host_context()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            ensure!(!self.inner.is_foreign(), "foreign refused Runtime after host precharge");
            factory.require_original_process()?;
            let body = self.parts_mut().body.take().expect("whole actual refused Runtime body checked");
            let super::original_native::OriginalRuntimeAcquisitionReleased {
                error, factory: original_factory, terminal: _, root, namespace, _schema_reference: _, _unused_inputs } = body;
            self.parts_mut().host = Some(OriginalRuntimeOutsideHostContext::new(
                Arc::downgrade(&original_factory),
                Some(OriginalRuntimeOutsideDirectory::Shared(Arc::clone(&root))),
                Some(Arc::downgrade(&root)),
                Some(namespace),
                OriginalRuntimeOutsideSemantics::Refused {
                    error: Some(error),
                    unused_inputs: UnusedRuntimeBoundInputs::new(_unused_inputs),
                    unused_parts: None,
                },
            ));
            drop(root);
            drop(original_factory);
        }
        let host = self.parts_mut().host.as_mut().unwrap();
        host.require_original(factory)?;
        unsafe { host.register_shared_roots(registry, prepay) }
    }
    pub(crate) fn require_shared_roots_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign Runtime source registration census");
        ensure!(self.body.is_none() && self.sources.complete(&None) && self.sources.packets.is_empty(),
            "Runtime retains original context or undelivered native sources");
        self.host.as_ref().context("actual Runtime host continuation absent")?
            .require_shared_roots_registered(registry, prepay)
    }
    pub(crate) fn consume_retired_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign refused Runtime root finalization");
        ensure!(self.body.is_none() && self.sources.complete(&None)
            && self.sources.packets.is_empty(),
            "refused Runtime root retirement precedes native-source and packet joins");
        self.parts_mut().host.as_mut().context("actual Runtime host root frame absent")?
            .consume_retired_shared_roots(registry, prepay)
    }
    pub(crate) fn advance_native_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign refused Runtime outside body");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let parts = self.parts_mut();
        if let Some(body) = parts.body.as_mut() {
            ensure!(Arc::ptr_eq(&body.factory, factory), "refused Runtime outside source changes factory");
            parts.sources.advance(factory, aggregate, &mut body._schema_reference, prepay)
        } else {
            self.host.as_ref().context("actual refused Runtime host context absent")?.require_original(factory)?;
            ensure!(self.sources.complete(&None), "refused Runtime host handoff has unresolved native sources");
            Ok(())
        }
    }
    /// # Safety
    /// Receiver caches this WHOLE original packet vector before further work.
    pub(crate) unsafe fn take_original_sql_custody(&mut self)
        -> Result<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>> {
        ensure!(!self.inner.is_foreign(), "foreign refused Runtime packet delivery");
        ensure!(self.sources.complete(&None) && self.body.as_ref().is_none_or(|body| body._schema_reference.is_none()),
            "refused Runtime retains unresolved native source");
        let unused = if let Some(body) = self.body.as_ref() { body._unused_inputs.is_some() }
            else { matches!(&self.host.as_ref().context("actual refused Runtime host body absent")?.semantics,
                OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. }
                if unused_inputs.is_some() || unused_parts.as_ref().is_some_and(|parts| !parts.aliases_transferred())) };
        ensure!(!unused, "actual unused Runtime bound parent/root custody has not transferred");
        ensure!(self.sources.packets.len() <= MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES,
            "Runtime packet vector exceeds its fixed main/schema owner slots");
        Ok(self.parts_mut().sources.take_packets())
    }
}

enum CloseCustody {
    Runtime(RuntimeDb),
    Pending(CloseContext, OriginalNativeCloseFailure),
    Closed(OriginalRuntimeClosed),
}

pub(crate) struct OriginalRuntimeCloseFailure {
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
    custody: Mutex<CloseCustody>,
}
impl std::fmt::Debug for OriginalRuntimeCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalRuntimeCloseFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalRuntimeCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for OriginalRuntimeCloseFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl OriginalRuntimeCloseFailure {
    /// The actual owner is removed from bookkeeping before close effects.
    /// No new connection, identity, claim or deadline is constructed on retry.
    pub(crate) fn retry(self) -> std::result::Result<OriginalRuntimeClosed, Self> {
        let Self {
            error,
            factory,
            custody,
        } = self;
        advance_close(
            factory,
            custody.into_inner().unwrap_or_else(|p| p.into_inner()),
            Some(error),
        )
    }
}

fn advance_close(
    factory: Arc<OriginalNativeFactory>,
    custody: CloseCustody,
    prior: Option<anyhow::Error>,
) -> std::result::Result<OriginalRuntimeClosed, OriginalRuntimeCloseFailure> {
    let retain = |error: anyhow::Error, custody| OriginalRuntimeCloseFailure {
        error,
        factory: Arc::clone(&factory),
        custody: Mutex::new(custody),
    };
    let (context, closing) = match custody {
        CloseCustody::Runtime(mut db) => {
            let checked = (|| -> Result<()> {
                match &db.conn {
                    RuntimeConnection::OriginalMutation { owner, schema_reference } => {
                        ensure!(db.open_mode == RuntimeDbOpenMode::ExplicitHistoryReset
                            && owner.domain() == OriginalNativeDomain::RuntimeMutation,
                            "offline Runtime close changed its original mutation purpose");
                        schema_reference.require_original_closed(
                            &factory,
                            OriginalNativeDomain::NodeRuntimeSchemaReference,
                        )?;
                    }
                    RuntimeConnection::Live(live) => {
                        ensure!(db.open_mode == RuntimeDbOpenMode::Current
                            && live.owner().domain() == OriginalNativeDomain::NodeRuntime,
                            "live Runtime close changed its original node purpose");
                        live.require_close_ready()?;
                    }
                    _ => anyhow::bail!("Runtime close requires its actual admitted native owner"),
                }
                let directory = db
                    ._directory
                    .as_ref()
                    .context("Runtime close directory absent")?;
                let namespace = db
                    ._directory_lock
                    .as_ref()
                    .context("Runtime close namespace absent")?;
                namespace.ensure_protects(directory)?;
                directory.ensure_path_binding()?;
                let target = db
                    .reset_target
                    .as_ref()
                    .context("Runtime close target absent")?;
                ensure!(
                    target.parent() == Some(directory.path()),
                    "Runtime close target namespace changed"
                );
                Ok(())
            })();
            if let Err(error) = checked {
                return Err(retain(error, CloseCustody::Runtime(db)));
            }
            let context = CloseContext {
                factory: Arc::clone(&factory),
                directory: db._directory.take().expect("checked retained directory"),
                namespace: db._directory_lock.take().expect("checked namespace"),
                target: db.reset_target.take().expect("checked target"),
                incarnation: db.incarnation,
                domain: match &db.conn {
                    RuntimeConnection::OriginalMutation { owner, schema_reference: _ } => owner.domain(),
                    RuntimeConnection::Live(live) => live.owner().domain(),
                    _ => unreachable!("checked native Runtime owner"),
                },
                _schema_reference: None,
            };
            let mut context = context;
            let owner = match db.conn {
                RuntimeConnection::OriginalMutation { owner, schema_reference } => {
                    context._schema_reference = Some(schema_reference);
                    owner
                }
                RuntimeConnection::Live(live) => {
                    let (owner, reference) = live.into_close_parts();
                    context._schema_reference = Some(reference);
                    owner
                }
                _ => unreachable!("checked native Runtime owner"),
            };
            (context, owner.close())
        }
        CloseCustody::Pending(context, owner) => (context, owner.retry()),
        CloseCustody::Closed(mut closed) => {
            if closed.prior_refusal.is_none() {
                closed.prior_refusal = prior;
            }
            if let Err(error) = closed.require_binding() {
                return Err(retain(error, CloseCustody::Closed(closed)));
            }
            return Ok(closed);
        }
    };
    let proof = match closing {
        Ok(proof) => proof,
        Err(owner) => {
            let error = anyhow!("original Runtime native close refused: {:?}", owner.error());
            return Err(retain(
                prior.unwrap_or(error),
                CloseCustody::Pending(context, owner),
            ));
        }
    };
    let closed = OriginalRuntimeClosed {
        context,
        proof,
        prior_refusal: prior,
    };
    if let Err(error) = closed.require_binding() {
        return Err(retain(error, CloseCustody::Closed(closed)));
    }
    Ok(closed)
}

impl RuntimeDb {
    pub(crate) fn close_original_runtime(
        self,
        factory: Arc<OriginalNativeFactory>,
    ) -> std::result::Result<OriginalRuntimeClosed, OriginalRuntimeCloseFailure> {
        advance_close(factory, CloseCustody::Runtime(self), None)
    }
}

// Final post-release exhaustion of the Runtime outside frames. Only cached
// receipts and copied identities are read; no factory or native state.
impl OriginalRuntimeOutsideHostContext {
    fn require_exhausted(&self) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign Runtime host frame at exhaustion");
        self.inner.require_exhausted()
    }

    /// End an exhausted host frame. It settles on the exhaustion check the
    /// caller already passed, so every slot is released normally.
    #[cfg(test)]
    fn exhaust(self) {
        drop(self);
    }
}

impl LiveRuntimeNativeSources {
    fn require_exhausted(&self) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign Runtime native sources at exhaustion");
        ensure!(self.inner.exhausted(),
            "Runtime native sources or packets remain at exhaustion");
        Ok(())
    }

    /// End exhausted sources. They settle on the exhaustion check the caller
    /// already passed, so every slot is released normally.
    #[cfg(test)]
    fn exhaust(self) {
        drop(self);
    }
}

impl OriginalLiveRuntimeOutsideCustody {
    /// Consume the closed Runtime frame after its shared-root receipts.
    /// Refusal returns the frame intact; success leaves no owner.
    pub(crate) fn into_exhausted(self) -> std::result::Result<(), (Self, anyhow::Error)> {
        if let Err(error) = self.require_exhausted_holders() {
            return Err((self, error));
        }
        // The check above is this frame's settled predicate, so its end
        // releases the context slot, host frame, sources and refusal normally.
        drop(self);
        Ok(())
    }

    /// Final exhaustion: this process, no close context, sources and host
    /// frame exhausted, and a refusal that retains no owner. A dropped frame
    /// settles on exactly these checks.
    fn require_exhausted_holders(&self) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign Runtime frame at exhaustion");
        self.inner.require_exhausted_holders()
    }
}

impl OriginalRuntimeAcquisitionOutsideCustody {
    /// Consume the refused-acquisition Runtime frame after its shared-root
    /// and unused-root receipts. Refusal returns the frame intact.
    pub(crate) fn into_exhausted(self) -> std::result::Result<(), (Self, anyhow::Error)> {
        if let Err(error) = self.require_exhausted_holders() {
            return Err((self, error));
        }
        // The check above is this frame's settled predicate, so its end
        // releases the body slot, host frame and sources normally.
        drop(self);
        Ok(())
    }

    /// Final exhaustion: this process, no refused body, sources and host
    /// frame exhausted. A dropped frame settles on exactly these checks.
    fn require_exhausted_holders(&self) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign refused Runtime frame at exhaustion");
        self.inner.require_exhausted_holders()
    }
}

#[cfg(test)]
mod outside_quote_tests {
    use super::{MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES, quote_runtime_packet_sources,
        quote_runtime_retired_shared_roots, MAX_UNUSED_RUNTIME_ROOT_REGISTRATIONS};
    use lillux_sqlite::OriginalNativeOutsideConnectionCustody;

    #[test]
    fn runtime_packet_quote_covers_retained_vector_and_bounds() {
        // The live packet has exactly the main and schema owner slots.
        let quote = quote_runtime_packet_sources(1, 1).unwrap();
        assert_eq!(quote.bytes,
            (2 * std::mem::size_of::<OriginalNativeOutsideConnectionCustody>()
                + std::mem::size_of::<Vec<()>>()) as u64);
        assert_eq!(quote.steps, 24);
        assert!(quote_runtime_packet_sources(1, 2).is_err());
        assert!(quote_runtime_packet_sources(MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES, 1).is_err());
        assert!(quote_runtime_packet_sources(usize::MAX, 1).is_err());
    }

    #[test]
    fn runtime_retired_root_receipt_quote_covers_both_fixed_roles() {
        assert_eq!(quote_runtime_retired_shared_roots(0).unwrap(),
            lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 36 });
        assert_eq!(quote_runtime_retired_shared_roots(1).unwrap(),
            lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 54 });
        assert!(quote_runtime_retired_shared_roots(MAX_UNUSED_RUNTIME_ROOT_REGISTRATIONS + 1).is_err());
    }
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const UNUSED_PARTS: &str = "ryeos-app::unused-runtime-bound-parts";
    const HOST: &str = "ryeos-app::runtime-outside-host-context";
    const SOURCES: &str = "ryeos-app::live-runtime-native-sources";
    const LIVE: &str = "ryeos-app::live-runtime-outside-custody";
    const ACQUISITION: &str = "ryeos-app::runtime-acquisition-outside-custody";

    fn abandoned(kind: &str) -> usize {
        lillux::custody::abandoned_custody().count(kind)
    }

    /// Unused bound parts of never-opened inputs on a fresh directory, with
    /// the root retained inside when `keep_root` holds. No ticket is issued,
    /// so the shared host releases its arena whatever becomes of the parts.
    fn unused_parts(
        host: &lillux_sqlite::test_host::TestNativeHost,
        root: Arc<lillux::PinnedDirectory>,
        keep_root: bool,
    ) -> OriginalUnusedRuntimeBoundParts {
        // SAFETY: test-only issuance under the shared host's own funded factory.
        let inputs = unsafe {
            lillux_sqlite::OriginalNativeBoundInputs::issued(
                root, std::ffi::OsStr::new("runtime.sqlite3"),
                lillux_sqlite::OriginalNativeBoundMode::OpenOrCreate,
                OriginalNativeDomain::NodeRuntime, host.destination(), None,
            )
        }
        .unwrap();
        let lillux_sqlite::OriginalNativeOutsideBoundParts { root, name, mode, domain, authority, inspection } =
            inputs.into_original_parts().ok().expect("fresh inputs split in their own process");
        drop((authority, inspection));
        OriginalUnusedRuntimeBoundParts::new(OriginalUnusedRuntimeBoundFields {
            root: keep_root.then_some(root),
            authority: None,
            inspection: None,
            registration: None,
            _name: name, mode, domain,
        })
    }

    #[test]
    fn dropped_retired_unused_parts_release_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        let parts = unused_parts(&host, Arc::clone(&root), false);
        assert!(parts.retired());
        let before = abandoned(UNUSED_PARTS);
        drop(parts);
        assert_eq!(abandoned(UNUSED_PARTS), before);
        assert_eq!(Arc::strong_count(&root), 1, "the root alias was released");
        ryeos_state::test_support::release_shared_test_host(host);
    }

    #[test]
    fn dropped_unused_parts_with_root_are_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        let parts = unused_parts(&host, Arc::clone(&root), true);
        assert!(!parts.retired());
        let before = abandoned(UNUSED_PARTS);
        drop(parts);
        assert_eq!(abandoned(UNUSED_PARTS), before + 1);
        assert_eq!(Arc::strong_count(&root), 2, "the root alias stays retained");
        ryeos_state::test_support::release_shared_test_host(host);
    }

    const UNUSED_INPUTS: &str = "ryeos-app::unused-runtime-bound-inputs";

    /// Refused semantics holding never-opened bound inputs on `root`, or no
    /// inputs when `with_inputs` is false. No ticket is issued.
    fn refused_semantics(
        host: &lillux_sqlite::test_host::TestNativeHost,
        root: Arc<lillux::PinnedDirectory>,
        with_inputs: bool,
    ) -> OriginalRuntimeOutsideSemantics {
        // SAFETY: test-only issuance under the shared host's own funded factory.
        let inputs = with_inputs.then(|| unsafe {
            lillux_sqlite::OriginalNativeBoundInputs::issued(
                root, std::ffi::OsStr::new("runtime.sqlite3"),
                lillux_sqlite::OriginalNativeBoundMode::OpenOrCreate,
                OriginalNativeDomain::NodeRuntime, host.destination(), None,
            )
        }.unwrap());
        OriginalRuntimeOutsideSemantics::Refused {
            error: Some(anyhow::anyhow!("runtime acquisition refused")),
            unused_inputs: UnusedRuntimeBoundInputs::new(inputs),
            unused_parts: None,
        }
    }

    #[test]
    fn dropped_refused_semantics_retain_their_unused_inputs() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        // Semantics dropped on their own, outside any host frame, still
        // retain unused inputs, counted.
        let semantics = refused_semantics(&host, Arc::clone(&root), true);
        let before = abandoned(UNUSED_INPUTS);
        drop(semantics);
        assert_eq!(abandoned(UNUSED_INPUTS), before + 1);
        assert_eq!(Arc::strong_count(&root), 2, "the root alias stays retained");
        // With no inputs left there is nothing to retain.
        let semantics = refused_semantics(&host, Arc::clone(&root), false);
        let before = abandoned(UNUSED_INPUTS);
        drop(semantics);
        assert_eq!(abandoned(UNUSED_INPUTS), before);
        ryeos_state::test_support::release_shared_test_host(host);
    }

    /// A refused host frame whose roots were consumed, holding `directory`.
    fn host_context(directory: Option<Arc<lillux::PinnedDirectory>>) -> OriginalRuntimeOutsideHostContext {
        let mut host = OriginalRuntimeOutsideHostContext::new(
            std::sync::Weak::new(),
            directory.map(OriginalRuntimeOutsideDirectory::Shared),
            None,
            None,
            OriginalRuntimeOutsideSemantics::Refused {
                error: Some(anyhow::anyhow!("runtime acquisition refused")),
                unused_inputs: UnusedRuntimeBoundInputs::new(None),
                unused_parts: None,
            },
        );
        host.fields_mut().retired_roots_consumed = true;
        host
    }

    #[test]
    fn dropped_exhausted_host_context_releases_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = host_context(None);
        host.require_exhausted().unwrap();
        let before = abandoned(HOST);
        host.exhaust();
        assert_eq!(abandoned(HOST), before);
    }

    #[test]
    fn dropped_host_context_with_directory_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        let host = host_context(Some(Arc::clone(&root)));
        assert!(host.require_exhausted().is_err());
        let before = abandoned(HOST);
        drop(host);
        assert_eq!(abandoned(HOST), before + 1);
        assert_eq!(Arc::strong_count(&root), 2, "the directory alias stays retained");
    }

    #[test]
    fn dropped_empty_native_sources_release_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let sources = LiveRuntimeNativeSources::new(None);
        sources.require_exhausted().unwrap();
        let before = abandoned(SOURCES);
        sources.exhaust();
        assert_eq!(abandoned(SOURCES), before);
    }

    #[test]
    fn dropped_native_sources_with_terminal_are_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_native_sources_with_terminal_are_retained");
        // The retained terminal aliases a closed native connection whose
        // host cannot finish while the alias survives, so the scenario runs
        // in its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "sources +1", "{outcome}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        let host = lillux_sqlite::test_host::TestNativeHost::acquire();
        let (ledger, db) = crate::accounting_db::AccountingDb::open_native_for_test(
            Arc::clone(host.factory()), &root, host.destination()).unwrap();
        drop(db.expect("fresh native ledger opens"));
        ledger.close().unwrap();
        let terminal = ledger.native_terminals(host.factory()).unwrap().remove(0);
        let sources = LiveRuntimeNativeSources::new(Some(terminal));
        assert!(sources.require_exhausted().is_err());
        let before = abandoned(SOURCES);
        drop(sources);
        let delta = abandoned(SOURCES) - before;
        lillux_sqlite::test_host::end_isolated_process((dir, root, ledger, host), &format!("sources +{delta}"))
    }

    fn live_outside_custody(host: Option<OriginalRuntimeOutsideHostContext>) -> OriginalLiveRuntimeOutsideCustody {
        OriginalLiveRuntimeOutsideCustody {
            inner: lillux::custody::Held::new(
                LIVE_RUNTIME_OUTSIDE_KIND,
                OriginalLiveRuntimeOutsideParts {
                    context: None,
                    host,
                    prior_refusal: None,
                    sources: LiveRuntimeNativeSources::new(None),
                },
                live_runtime_outside_exhausted,
            ),
        }
    }

    #[test]
    fn exhausted_live_outside_custody_releases_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let custody = live_outside_custody(Some(host_context(None)));
        let before = (abandoned(LIVE), abandoned(HOST));
        custody.into_exhausted().map_err(|(_, error)| error).unwrap();
        assert_eq!((abandoned(LIVE), abandoned(HOST)), before);
    }

    #[test]
    fn dropped_live_outside_custody_without_host_frame_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let custody = live_outside_custody(None);
        assert!(custody.require_exhausted_holders().is_err());
        let before = abandoned(LIVE);
        drop(custody);
        assert_eq!(abandoned(LIVE), before + 1);
    }

    fn acquisition_outside_custody(host: Option<OriginalRuntimeOutsideHostContext>)
        -> OriginalRuntimeAcquisitionOutsideCustody {
        OriginalRuntimeAcquisitionOutsideCustody {
            inner: lillux::custody::Held::new(
                RUNTIME_ACQUISITION_OUTSIDE_KIND,
                OriginalRuntimeAcquisitionOutsideParts { body: None, host, sources: LiveRuntimeNativeSources::new(None) },
                acquisition_outside_exhausted,
            ),
        }
    }

    #[test]
    fn exhausted_acquisition_outside_custody_releases_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let custody = acquisition_outside_custody(Some(host_context(None)));
        let before = (abandoned(ACQUISITION), abandoned(HOST));
        custody.into_exhausted().map_err(|(_, error)| error).unwrap();
        assert_eq!((abandoned(ACQUISITION), abandoned(HOST)), before);
    }

    #[test]
    fn dropped_acquisition_outside_custody_without_host_frame_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let custody = acquisition_outside_custody(None);
        assert!(custody.require_exhausted_holders().is_err());
        let before = abandoned(ACQUISITION);
        drop(custody);
        assert_eq!(abandoned(ACQUISITION), before + 1);
    }
}
