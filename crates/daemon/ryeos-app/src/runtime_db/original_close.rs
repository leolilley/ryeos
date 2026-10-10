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

pub fn quote_runtime_namespace_owner() -> OutsideWork {
    OutsideWork { bytes: 512, steps: 4 }
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

    pub(crate) fn prior_refusal(&self) -> Option<&anyhow::Error> {
        self.prior_refusal.as_ref()
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
    process: u32,
    context: std::mem::ManuallyDrop<Option<CloseContext>>,
    host: std::mem::ManuallyDrop<Option<OriginalRuntimeOutsideHostContext>>,
    sources: LiveRuntimeNativeSources,
    prior_refusal: std::mem::ManuallyDrop<Option<anyhow::Error>>,
}
impl std::fmt::Debug for OriginalLiveRuntimeOutsideCustody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalLiveRuntimeOutsideCustody").field("process", &self.process).finish_non_exhaustive()
    }
}
impl OriginalRuntimeClosed {
    pub(crate) fn into_live_outside_custody(self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<OriginalLiveRuntimeOutsideCustody, (Self, anyhow::Error)> {
        let process = std::process::id();
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
            process, context: std::mem::ManuallyDrop::new(Some(context)),
            host: std::mem::ManuallyDrop::new(None),
            sources: LiveRuntimeNativeSources::new(Some(OriginalNativeConnectionTerminal::Closed(proof))),
            prior_refusal: std::mem::ManuallyDrop::new(prior_refusal),
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
            ensure!(self.process == std::process::id(), "foreign live Runtime after host precharge");
            factory.require_original_process()?;
            let context = self.context.take().expect("whole actual live Runtime context checked");
            let CloseContext { factory: original_factory, directory, namespace, target, incarnation, domain, _schema_reference: _ } = context;
            *self.host = Some(OriginalRuntimeOutsideHostContext {
                process: self.process, factory: Arc::downgrade(&original_factory),
                directory: std::mem::ManuallyDrop::new(Some(OriginalRuntimeOutsideDirectory::Owned(directory))),
                shared_directory_identity: None,
                namespace: std::mem::ManuallyDrop::new(Some(namespace)),
                semantics: OriginalRuntimeOutsideSemantics::Closed { target, incarnation, domain },
                registrations: std::array::from_fn(|_| None),
                retired_roots_consumed: false,
            });
            // SAME actual caller factory remains retained. No physical field
            // is dropped when this completed redundant facade alias ends.
            drop(original_factory);
        }
        unsafe { self.host.as_mut().unwrap().register_shared_roots(registry, prepay) }
    }
    pub(crate) fn require_shared_roots_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign Runtime source registration census");
        ensure!(self.context.is_none() && self.sources.complete(&None) && self.sources.packets.is_empty(),
            "Runtime retains original context or undelivered native sources");
        self.host.as_ref().context("actual Runtime host continuation absent")?
            .require_shared_roots_registered(registry, prepay)
    }
    pub(crate) fn actual_descriptors_retired(&self) -> Result<bool> {
        ensure!(self.process == std::process::id(), "foreign live Runtime physical observation");
        let Some(host) = self.host.as_ref() else { return Ok(false); };
        host.actual_descriptors_retired()
    }
    pub(crate) fn consume_retired_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign live Runtime root finalization");
        ensure!(self.context.is_none() && self.sources.complete(&None)
            && self.sources.packets.is_empty(),
            "Runtime root retirement precedes native-source and packet joins");
        self.host.as_mut().context("actual Runtime host root frame absent")?
            .consume_retired_shared_roots(registry, prepay)
    }
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original live Runtime outside frame");
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
        if let Some(context) = self.context.as_mut() {
            self.sources.advance(factory, aggregate, &mut context._schema_reference, prepay)
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
        ensure!(self.process == std::process::id(), "foreign live Runtime packet delivery");
        ensure!(self.sources.main.is_none() && self.sources.schema.is_none() && self.sources.native_failure.is_none()
            && self.context.as_ref().is_none_or(|context| context._schema_reference.is_none()), "actual live Runtime native sources remain retained");
        ensure!(self.sources.packets.len() <= MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES,
            "Runtime packet vector exceeds its fixed main/schema owner slots");
        Ok(std::mem::take(&mut *self.sources.packets))
    }
}

enum OriginalRuntimeOutsideDirectory {
    Owned(lillux::PinnedDirectory),
    Shared(Arc<lillux::PinnedDirectory>),
}
enum OriginalRuntimeOutsideSemantics {
    Closed { target: PathBuf, incarnation: RuntimeDbIncarnation, domain: OriginalNativeDomain },
    Refused {
        error: std::mem::ManuallyDrop<anyhow::Error>,
        unused_inputs: std::mem::ManuallyDrop<Option<lillux_sqlite::OriginalNativeBoundInputs>>,
        unused_parts: std::mem::ManuallyDrop<Option<OriginalUnusedRuntimeBoundParts>>,
    },
}
struct OriginalUnusedRuntimeBoundParts {
    root: std::mem::ManuallyDrop<Option<Arc<lillux::PinnedDirectory>>>,
    authority: std::mem::ManuallyDrop<Option<Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>>>,
    inspection: std::mem::ManuallyDrop<Option<(lillux_sqlite::SqliteCopyLease, Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>)>>,
    name: lillux_sqlite::OriginalNativeOutsideBoundCoordinate,
    mode: lillux_sqlite::OriginalNativeBoundMode,
    domain: OriginalNativeDomain,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}
impl OriginalUnusedRuntimeBoundParts {
    fn aliases_transferred(&self) -> bool {
        self.registration.is_some() && self.root.is_none() && self.authority.is_none() && self.inspection.is_none()
    }
}
struct OriginalRuntimeOutsideHostContext {
    process: u32,
    factory: std::sync::Weak<OriginalNativeFactory>,
    directory: std::mem::ManuallyDrop<Option<OriginalRuntimeOutsideDirectory>>,
    shared_directory_identity: Option<std::sync::Weak<lillux::PinnedDirectory>>,
    namespace: std::mem::ManuallyDrop<Option<lillux::PinnedDirectoryLock>>,
    semantics: OriginalRuntimeOutsideSemantics,
    registrations: [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>; 2],
    retired_roots_consumed: bool,
}
impl OriginalRuntimeOutsideHostContext {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original Runtime host context");
        factory.require_original_process()?;
        ensure!(self.factory.ptr_eq(&Arc::downgrade(factory)), "Runtime host context changes actual factory allocation");
        Ok(())
    }
    unsafe fn register_shared_roots(&mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original Runtime physical sources");
        ensure!(!self.retired_roots_consumed,
            "retired Runtime root frame cannot be registered again");
        let existing_checks = self.registrations.iter().filter(|binding| binding.is_some()).count();
        if existing_checks > 0 {
            let quote = quote_runtime_membership(registry, u64::try_from(existing_checks)?)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        }
        if let Some(binding) = &self.registrations[0] { registry.require_membership(binding)?; }
        else {
            // Concrete Arc control before wrapping the actual owned FD. No
            // reopened directory or inode/path matching becomes ownership.
            let quote = quote_runtime_directory_transfer()?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            ensure!(self.process == std::process::id(), "foreign Runtime root after precharge");
            let directory = self.directory.take().context("actual Runtime directory absent without registration")?;
            let directory = match directory {
                OriginalRuntimeOutsideDirectory::Owned(directory) => Arc::new(directory),
                OriginalRuntimeOutsideDirectory::Shared(directory) => directory,
            };
            self.shared_directory_identity = Some(Arc::downgrade(&directory));
            match unsafe { registry.register_directory(directory, prepay) } {
                Ok(binding) => self.registrations[0] = Some(binding),
                Err((directory, error)) => { *self.directory = Some(OriginalRuntimeOutsideDirectory::Shared(directory)); return Err(error); }
            }
        }
        if let Some(binding) = &self.registrations[1] { registry.require_membership(binding)?; }
        else {
            let namespace = self.namespace.take().context("actual Runtime namespace absent without registration")?;
            match unsafe { registry.register_namespace(namespace, prepay) } {
                Ok(binding) => self.registrations[1] = Some(binding),
                Err((namespace, error)) => { *self.namespace = Some(namespace); return Err(error); }
            }
        }
        Ok(())
    }
    fn require_shared_roots_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign Runtime registration census");
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
    fn unused_runtime_source_is_retired(&self) -> bool {
        match &self.semantics {
            OriginalRuntimeOutsideSemantics::Closed { .. } => true,
            OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. } =>
                unused_inputs.is_none() && unused_parts.as_ref().is_none_or(|parts|
                    parts.root.is_none() && parts.authority.is_none()
                        && parts.inspection.is_none() && parts.registration.is_none()),
        }
    }
    fn actual_descriptors_retired(&self) -> Result<bool> {
        ensure!(self.process == std::process::id(), "foreign actual Runtime physical receipts");
        if self.retired_roots_consumed {
            ensure!(self.registrations.iter().all(Option::is_none)
                && self.shared_directory_identity.is_none()
                && self.unused_runtime_source_is_retired(),
                "consumed Runtime root frame regained registration state");
            return Ok(true);
        }
        if self.directory.is_some() || self.namespace.is_some() { return Ok(false); }
        if matches!(&self.semantics, OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. }
            if unused_inputs.is_some() || unused_parts.as_ref().is_some_and(|parts| !parts.aliases_transferred())) {
            // These are genuine never-attempted Bound inputs, not an empty
            // native slot's disposal proof. Their actual parents/roots remain.
            return Ok(false);
        }
        for binding in &self.registrations {
            let Some(binding) = binding else { return Ok(false); };
            if !binding.retired() { return Ok(false); }
        }
        if self.unused_runtime_root_registration()?.is_some_and(|binding| !binding.retired()) {
            return Ok(false);
        }
        Ok(true)
    }

    fn consume_retired_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign retired Runtime root frame");
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
        ensure!(self.process == std::process::id(),
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

        self.registrations = std::array::from_fn(|_| None);
        if let OriginalRuntimeOutsideSemantics::Refused { unused_parts, .. } = &mut self.semantics {
            if let Some(parts) = unused_parts.as_mut() {
                parts.registration = None;
            }
        }
        self.shared_directory_identity = None;
        self.retired_roots_consumed = true;
        Ok(())
    }
}
const MAX_UNUSED_RUNTIME_ROOT_REGISTRATIONS: usize = 1;
pub const MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES: usize = 2;

struct LiveRuntimeNativeSources {
    main: std::mem::ManuallyDrop<Option<OriginalNativeConnectionTerminal>>,
    schema: std::mem::ManuallyDrop<Option<OriginalNativeConnectionTerminal>>,
    schema_definition: std::mem::ManuallyDrop<Option<sqlite_schema::SchemaSqlDefinition>>,
    native_failure: std::mem::ManuallyDrop<Option<(usize, lillux_sqlite::OriginalNativeOutsideCustodyFailure)>>,
    packets: std::mem::ManuallyDrop<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
}
impl LiveRuntimeNativeSources {
    fn new(main: Option<OriginalNativeConnectionTerminal>) -> Self {
        Self { main: std::mem::ManuallyDrop::new(main), schema: std::mem::ManuallyDrop::new(None),
            schema_definition: std::mem::ManuallyDrop::new(None), native_failure: std::mem::ManuallyDrop::new(None),
            packets: std::mem::ManuallyDrop::new(Vec::new()) }
    }
    fn advance(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        reference: &mut Option<sqlite_schema::ClosedNativeSchemaSqlReference>,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let count = usize::from(self.main.is_some()) + usize::from(self.schema.is_some())
            + usize::from(reference.is_some()) + usize::from(self.native_failure.is_some());
        if count == 0 { return Ok(()); }
        let quote = quote_runtime_packet_sources(self.packets.len(), count)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        factory.require_original_process()?;
        ensure!(self.packets.len().checked_add(count).is_some_and(|n|
            n <= MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES),
            "live Runtime native packet source bound changed after funding");
        self.packets.try_reserve_exact(count)?;
        if let Some(reference) = reference.as_ref() {
            reference.require_original_closed(factory, OriginalNativeDomain::NodeRuntimeSchemaReference)?;
        }
        if let Some((index, failure)) = self.native_failure.take() {
            if index >= 2 {
                *self.native_failure = Some((index, failure));
                bail!("actual live Runtime failure selects unknown source slot");
            }
            let slot = if index == 0 { &mut self.main } else { &mut self.schema };
            if slot.is_some() {
                *self.native_failure = Some((index, failure));
                bail!("live Runtime failure recovery conflicts with actual source slot");
            }
            match failure.into_original() {
                Ok(original) => **slot = Some(original),
                Err(failure) => { *self.native_failure = Some((index, failure)); bail!("live Runtime whole source recovery refused"); }
            }
        }
        // Validate every existing source before consuming any of this batch.
        for (index, source) in [&self.main, &self.schema].into_iter().enumerate() {
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
            ensure!(self.schema.is_none() && self.schema_definition.is_none(),
                "actual live Runtime schema source conflicts with retained original reference");
        }
        if let Some(reference) = reference.take() {
            let (definition, source) = reference.into_original_parts();
            *self.schema_definition = Some(definition);
            *self.schema = Some(source);
        }
        for index in 0..2 {
            let slot = if index == 0 { &mut self.main } else { &mut self.schema };
            if let Some(source) = slot.take() {
                match source.into_outside_custody(aggregate) {
                    Ok(packet) => self.packets.push(packet),
                    Err(failure) => {
                        *self.native_failure = Some((index, failure));
                        bail!("live Runtime actual native source retained after outside handoff refusal");
                    }
                }
            }
        }
        Ok(())
    }
    fn complete(&self, reference: &Option<sqlite_schema::ClosedNativeSchemaSqlReference>) -> bool {
        self.main.is_none() && self.schema.is_none() && self.native_failure.is_none() && reference.is_none()
    }
}
/// Actual refused live acquisition body, including error/root/namespace.
/// A no-main-source stage is not successful acquisition or physical disposal.
pub(crate) struct OriginalRuntimeAcquisitionOutsideCustody {
    process: u32,
    body: std::mem::ManuallyDrop<Option<super::original_native::OriginalRuntimeAcquisitionReleased>>,
    host: std::mem::ManuallyDrop<Option<OriginalRuntimeOutsideHostContext>>,
    sources: LiveRuntimeNativeSources,
}
impl std::fmt::Debug for OriginalRuntimeAcquisitionOutsideCustody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalRuntimeAcquisitionOutsideCustody").field("process", &self.process).finish_non_exhaustive()
    }
}
impl super::original_native::OriginalRuntimeAcquisitionReleased {
    pub(crate) fn into_live_outside_custody(mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<OriginalRuntimeAcquisitionOutsideCustody, (Self, anyhow::Error)> {
        let process = std::process::id();
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
        Ok(OriginalRuntimeAcquisitionOutsideCustody { process,
            body: std::mem::ManuallyDrop::new(Some(self)), host: std::mem::ManuallyDrop::new(None), sources })
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
        ensure!(self.process == std::process::id(), "foreign unused Runtime input handoff");
        factory.require_original_process()?; aggregate.require_original(factory)?;
        // Normalize/cache the complete actual refused context first. Its
        // native sources already closed genuinely; no new ticket is created.
        unsafe { self.register_shared_roots(factory, aggregate, registry, prepay) }?;
        let host = self.host.as_mut().context("actual original Runtime host context absent")?;
        host.require_original(factory)?;
        let original_root = host.shared_directory_identity.as_ref()
            .context("actual Runtime source directory allocation identity absent")?;
        let OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. } = &mut host.semantics else {
            bail!("unused Runtime source selected a successful close frame");
        };
        if unused_inputs.is_some() {
            ensure!(unused_parts.is_none(), "unused Runtime source conflicts with original retained parts");
            let quote = quote_unused_runtime_bound_parts()?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            ensure!(self.process == std::process::id(), "foreign unused Runtime source after precharge");
            let original = unused_inputs.take().expect("actual unused original inputs checked");
            let original = match original.into_original_parts() {
                Ok(original) => original,
                Err(original) => {
                    **unused_inputs = Some(original);
                    bail!("actual unused Runtime bound input extraction refused");
                }
            };
            **unused_parts = Some(OriginalUnusedRuntimeBoundParts {
                root: std::mem::ManuallyDrop::new(Some(original.root)),
                authority: std::mem::ManuallyDrop::new(Some(original.authority)),
                inspection: std::mem::ManuallyDrop::new(original.inspection),
                name: original.name, mode: original.mode, domain: original.domain, registration: None,
            });
        }
        let Some(original) = unused_parts.as_mut() else { return Ok(()); };
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
                Err((root, error)) => { *original.root = Some(root); return Err(error); }
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
        ensure!(self.process == std::process::id(), "foreign refused Runtime physical handoff");
        factory.require_original_process()?; aggregate.require_original(factory)?;
        if self.host.is_none() {
            let body = self.body.as_ref().context("actual refused Runtime body absent")?;
            ensure!(Arc::ptr_eq(&body.factory, factory) && self.sources.complete(&body._schema_reference)
                && body.terminal.is_none(), "refused Runtime physical handoff precedes actual native sources");
            let quote = quote_runtime_host_context()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            ensure!(self.process == std::process::id(), "foreign refused Runtime after host precharge");
            factory.require_original_process()?;
            let body = self.body.take().expect("whole actual refused Runtime body checked");
            let super::original_native::OriginalRuntimeAcquisitionReleased {
                error, factory: original_factory, terminal: _, root, namespace, _schema_reference: _, _unused_inputs } = body;
            *self.host = Some(OriginalRuntimeOutsideHostContext {
                process: self.process, factory: Arc::downgrade(&original_factory),
                shared_directory_identity: Some(Arc::downgrade(&root)),
                directory: std::mem::ManuallyDrop::new(Some(OriginalRuntimeOutsideDirectory::Shared(root))),
                namespace: std::mem::ManuallyDrop::new(Some(namespace)),
                semantics: OriginalRuntimeOutsideSemantics::Refused { error: std::mem::ManuallyDrop::new(error), unused_inputs: std::mem::ManuallyDrop::new(_unused_inputs), unused_parts: std::mem::ManuallyDrop::new(None) },
                registrations: std::array::from_fn(|_| None),
                retired_roots_consumed: false,
            });
            drop(original_factory);
        }
        let host = self.host.as_mut().unwrap();
        host.require_original(factory)?;
        unsafe { host.register_shared_roots(registry, prepay) }
    }
    pub(crate) fn require_shared_roots_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign Runtime source registration census");
        ensure!(self.body.is_none() && self.sources.complete(&None) && self.sources.packets.is_empty(),
            "Runtime retains original context or undelivered native sources");
        self.host.as_ref().context("actual Runtime host continuation absent")?
            .require_shared_roots_registered(registry, prepay)
    }
    pub(crate) fn actual_descriptors_retired(&self) -> Result<bool> {
        ensure!(self.process == std::process::id(), "foreign refused Runtime physical observation");
        let Some(host) = self.host.as_ref() else { return Ok(false); };
        host.actual_descriptors_retired()
    }
    pub(crate) fn consume_retired_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign refused Runtime root finalization");
        ensure!(self.body.is_none() && self.sources.complete(&None)
            && self.sources.packets.is_empty(),
            "refused Runtime root retirement precedes native-source and packet joins");
        self.host.as_mut().context("actual Runtime host root frame absent")?
            .consume_retired_shared_roots(registry, prepay)
    }
    pub(crate) fn advance_native_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign refused Runtime outside body");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if let Some(body) = self.body.as_mut() {
            ensure!(Arc::ptr_eq(&body.factory, factory), "refused Runtime outside source changes factory");
            self.sources.advance(factory, aggregate, &mut body._schema_reference, prepay)
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
        ensure!(self.process == std::process::id(), "foreign refused Runtime packet delivery");
        ensure!(self.sources.complete(&None) && self.body.as_ref().is_none_or(|body| body._schema_reference.is_none()),
            "refused Runtime retains unresolved native source");
        let unused = if let Some(body) = self.body.as_ref() { body._unused_inputs.is_some() }
            else { matches!(&self.host.as_ref().context("actual refused Runtime host body absent")?.semantics,
                OriginalRuntimeOutsideSemantics::Refused { unused_inputs, unused_parts, .. }
                if unused_inputs.is_some() || unused_parts.as_ref().is_some_and(|parts| !parts.aliases_transferred())) };
        ensure!(!unused, "actual unused Runtime bound parent/root custody has not transferred");
        ensure!(self.sources.packets.len() <= MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES,
            "Runtime packet vector exceeds its fixed main/schema owner slots");
        Ok(std::mem::take(&mut *self.sources.packets))
    }
}

/// The actual consumed Runtime context after State ACK. Descriptor transfer
/// and physical close are distinct; this owner never infers disposal from Drop.
pub(crate) struct OriginalRuntimeFinalizer {
    factory: Arc<OriginalNativeFactory>,
    proof: Arc<ClosedOriginalNativeConnection>,
    schema_reference: Option<sqlite_schema::ClosedNativeSchemaSqlReference>,
    roots: Mutex<RuntimeFinalizerRoots>,
}
struct RuntimeFinalizerRoots {
    directory: Option<lillux::PinnedDirectory>,
    namespace: Option<lillux::PinnedDirectoryLock>,
    directory_owner: Option<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>>,
    namespace_owner: Option<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>>,
    directory_receipt: Option<Arc<lillux_sqlite::RetiredOriginalFinalizerFile>>,
    namespace_receipt: Option<Arc<lillux_sqlite::RetiredOriginalFinalizerFile>>,
}
impl OriginalRuntimeClosed {
    /// The caller must prepay this finite producer and descriptor-owner storage
    /// under the actual post-ACK original envelope before consuming the frame.
    pub(crate) fn into_finalizer(
        self,
    ) -> std::result::Result<Arc<OriginalRuntimeFinalizer>, (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            ensure!(self.context.domain == OriginalNativeDomain::RuntimeMutation,
                "live Runtime close cannot become an offline finalizer");
            ensure!(
                self.prior_refusal.is_none(),
                "failed Runtime cannot become a successful finalizer"
            );
            self.require_binding()
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }
        let Self { context, proof, .. } = self;
        Ok(Arc::new(OriginalRuntimeFinalizer {
            factory: context.factory,
            proof,
            schema_reference: context._schema_reference,
            roots: Mutex::new(RuntimeFinalizerRoots {
                directory: Some(context.directory),
                namespace: Some(context.namespace),
                directory_owner: None,
                namespace_owner: None,
                directory_receipt: None,
                namespace_receipt: None,
            }),
        }))
    }
}
impl OriginalRuntimeFinalizer {
    /// Exact private owner membership and actual native Runtime close. A role
    /// label, caller list or matching inode cannot substitute for this producer.
    pub(crate) fn require_original_file(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        owner: &Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    ) -> Result<()> {
        ensure!(
            self.factory.same_original(factory),
            "Runtime finalizer selected another factory"
        );
        self.proof.require_original(factory)?;
        ensure!(
            self.proof.domain() == OriginalNativeDomain::RuntimeMutation,
            "Runtime finalizer has another native domain"
        );
        let roots = self
            .roots
            .try_lock()
            .map_err(|_| anyhow!("Runtime finalizer custody busy or poisoned"))?;
        ensure!(
            roots
                .directory_owner
                .as_ref()
                .is_some_and(|actual| Arc::ptr_eq(actual, owner))
                || roots
                    .namespace_owner
                    .as_ref()
                    .is_some_and(|actual| Arc::ptr_eq(actual, owner)),
            "Runtime finalizer selected a foreign descriptor owner"
        );
        Ok(())
    }
    /// Transfer the same actual directory, never duplicate or reopen it. The
    /// funded producer must already be retained by the controller. No close or
    /// native effect occurs under the bookkeeping mutex.
    pub(crate) fn transfer_directory(
        &self,
    ) -> Result<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>> {
        let mut roots = self
            .roots
            .try_lock()
            .map_err(|_| anyhow!("Runtime finalizer custody busy or poisoned"))?;
        if let Some(owner) = &roots.directory_owner {
            return Ok(owner.clone());
        }
        let directory = roots
            .directory
            .take()
            .context("actual Runtime directory already transferred without owner")?;
        // SAFETY: private construction consumed the genuine closed Runtime,
        // and the precharged controller retains this producer before transfer.
        // This is its actual directory descriptor, not a duplicate stand-in.
        let owner = unsafe {
            lillux_sqlite::RetainedOriginalFinalizerFile::issued(
                directory.into_owned_close_descriptor(),
                lillux_sqlite::OriginalFinalizerFileKind::Directory,
            )
        };
        roots.directory_owner = Some(owner.clone());
        Ok(owner)
    }
    /// Keep the genuine same-owner close receipt in this already-retained
    /// producer. Directory closure does not discharge its namespace or any
    /// other finalizer, and no bookkeeping guard spans the actual close.
    pub(crate) fn retain_directory_retirement(
        &self,
        receipt: Arc<lillux_sqlite::RetiredOriginalFinalizerFile>,
    ) -> Result<()> {
        let owner = {
            let roots = self
                .roots
                .try_lock()
                .map_err(|_| anyhow!("Runtime finalizer receipt custody busy or poisoned"))?;
            roots
                .directory_owner
                .clone()
                .context("Runtime directory owner absent")?
        };
        receipt.require_original(&owner)?;
        let mut roots = self
            .roots
            .try_lock()
            .map_err(|_| anyhow!("Runtime finalizer receipt registration busy or poisoned"))?;
        roots.directory_receipt = Some(receipt);
        Ok(())
    }

    /// A shared namespace remains pending under the SAME actual lease. Owner
    /// extraction is a precondition; only the later real close is evidence.
    pub(crate) fn transfer_namespace(
        &self,
    ) -> Result<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>> {
        let mut roots = self
            .roots
            .try_lock()
            .map_err(|_| anyhow!("Runtime finalizer custody busy or poisoned"))?;
        if let Some(owner) = &roots.namespace_owner {
            return Ok(owner.clone());
        }
        let namespace = roots
            .namespace
            .take()
            .context("actual Runtime namespace already transferred without owner")?;
        let descriptor = match namespace.try_into_owned_close_descriptor() {
            Ok(descriptor) => descriptor,
            Err(namespace) => {
                roots.namespace = Some(namespace);
                bail!("Runtime namespace still has actual live finalizer leases");
            }
        };
        // SAFETY: actual unique locked descriptor is moved without LOCK_UN;
        // the precharged retained producer keeps it until real close receipts.
        let owner = unsafe {
            lillux_sqlite::RetainedOriginalFinalizerFile::issued(
                descriptor,
                lillux_sqlite::OriginalFinalizerFileKind::DirectoryLock,
            )
        };
        roots.namespace_owner = Some(owner.clone());
        Ok(owner)
    }
}

/// Actual Runtime continuation after its original directory retirement.
/// Native receipt transfer and physical namespace retirement remain separate.
/// Destruction cannot close these retained roots or stand in for settlement.
pub(crate) struct OriginalOutsideRuntimeFinalizer {
    process: u32,
    roots: std::mem::ManuallyDrop<RuntimeFinalizerRoots>,
    shared_registrations: [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>; 1],
    terminals: std::mem::ManuallyDrop<[Option<OriginalNativeConnectionTerminal>; 2]>,
    schema_reference: std::mem::ManuallyDrop<Option<sqlite_schema::ClosedNativeSchemaSqlReference>>,
    schema_definition: Option<sqlite_schema::SchemaSqlDefinition>,
    native_failure: Option<(usize, lillux_sqlite::OriginalNativeOutsideCustodyFailure)>,
    sql_custody: Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
}
impl std::fmt::Debug for OriginalOutsideRuntimeFinalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalOutsideRuntimeFinalizer")
            .field("process", &self.process).finish_non_exhaustive()
    }
}
impl OriginalRuntimeFinalizer {
    /// # Safety
    /// The actual controller has quiesced all Runtime consumers, prepaid this
    /// finite owner under the same post-ACK clock/envelope, and removed its
    /// redundant finalizer aliases. Cache this owner or whole refusal before
    /// any further effect. No native or physical disposal is inferred here.
    pub(crate) unsafe fn into_outside_finalizer(
        self: Arc<Self>,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
    ) -> std::result::Result<OriginalOutsideRuntimeFinalizer, (Arc<Self>, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.factory.require_original_process()?;
            terminal.require_original(&self.factory)?;
            self.proof.require_original(&self.factory)?;
            ensure!(self.proof.domain() == OriginalNativeDomain::RuntimeMutation,
                "outside Runtime handoff requires its actual offline close");
            let roots = self.roots.try_lock()
                .map_err(|_| anyhow!("Runtime outside handoff roots busy or poisoned"))?;
            ensure!(roots.directory.is_none(), "actual Runtime directory has not transferred");
            let owner = roots.directory_owner.as_ref()
                .context("actual Runtime directory owner absent")?;
            roots.directory_receipt.as_ref()
                .context("actual Runtime directory close receipt absent")?.require_original(owner)?;
            ensure!(Arc::strong_count(&self) == 1 && Arc::weak_count(&self) == 0,
                "actual Runtime finalizer still has other consumers");
            Ok(())
        })();
        if let Err(error) = checked { return Err((self, error)); }
        let actual = match Arc::try_unwrap(self) {
            Ok(actual) => actual,
            Err(actual) => return Err((actual, anyhow!("Runtime consuming handoff lost exclusive custody"))),
        };
        let Self { factory, proof, schema_reference, roots } = actual;
        // The real proof retains its native owner until the distinct lower
        // receipt split succeeds; removing this direct alias loses no resource.
        let process = std::process::id();
        let outside = OriginalOutsideRuntimeFinalizer {
            process,
            roots: std::mem::ManuallyDrop::new(roots.into_inner().unwrap_or_else(|p| p.into_inner())),
            shared_registrations: [None],
            terminals: std::mem::ManuallyDrop::new([Some(OriginalNativeConnectionTerminal::Closed(proof)), None]),
            schema_reference: std::mem::ManuallyDrop::new(schema_reference),
            schema_definition: None,
            native_failure: None,
            sql_custody: Vec::new(),
        };
        drop(factory);
        Ok(outside)
    }
}
impl OriginalOutsideRuntimeFinalizer {
    fn require_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "Runtime outside custody belongs to original process");
        Ok(())
    }
    pub(crate) fn retire_native_references(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
        retire_aliases: &dyn Fn(&OriginalNativeConnectionTerminal) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        terminal.require_original(factory)?;
        if let Some((index, failure)) = self.native_failure.take() {
            match failure.into_original() {
                Ok(original) => self.terminals[index] = Some(original),
                Err(failure) => {
                    self.native_failure = Some((index, failure));
                    bail!("actual Runtime native receipt recovery refused");
                }
            }
        }
        if let Some(reference) = self.schema_reference.take() {
            let (definition, original) = reference.into_original_parts();
            self.schema_definition = Some(definition);
            self.terminals[1] = Some(original);
        }
        for index in 0..self.terminals.len() {
            if let Some(canonical) = self.terminals[index].as_ref() {
                canonical.require_original(factory)?;
                lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| retire_aliases(canonical))?;
            }
            if let Some(original) = self.terminals[index].take() {
                match original.into_outside_custody(terminal) {
                    Ok(custody) => self.sql_custody.push(custody),
                    Err(failure) => {
                        let message = format!("actual Runtime native transfer retains original receipt: {failure}");
                        self.native_failure = Some((index, failure));
                        bail!(message);
                    }
                }
            }
        }
        Ok(())
    }
    pub(crate) fn native_receipts_transferred(&self) -> bool {
        self.terminals.iter().all(Option::is_none) && self.native_failure.is_none() && self.schema_reference.is_none()
    }
    /// # Safety
    /// The outside controller must cache the returned actual resource packets
    /// before any subsequent effect. Payloads can still retain native affinity;
    /// only the genuine zero-handle region seal permits physical unmapping.
    pub(crate) unsafe fn take_original_sql_custody(
        &mut self,
    ) -> Result<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>> {
        self.require_process()?;
        ensure!(self.native_receipts_transferred(), "Runtime SQL custody transfer remains incomplete");
        Ok(std::mem::take(&mut self.sql_custody))
    }
    /// # Safety
    /// This is the same original outside supervisor's finite registry. All
    /// shared producers register before its inventory is sealed; cache the
    /// returned memberships in this retained token before effects. Only exact
    /// same retained lock ownership may share a physical receipt.
    pub(crate) unsafe fn register_shared_roots(
        &mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        ensure!(self.native_receipts_transferred() && self.sql_custody.is_empty(),
            "actual Runtime native payload handoff remains incomplete");
        if self.roots.namespace.is_some() {
            let quote = quote_runtime_namespace_owner();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            let original = self.roots.namespace.take().expect("checked actual Runtime namespace");
            match unsafe { registry.register_namespace(original, prepay) } {
                Ok(registration) => self.shared_registrations[0] = Some(registration),
                Err((original, error)) => {
                    self.roots.namespace = Some(original);
                    return Err(error);
                }
            }
        }
        self.require_shared_roots_registered(registry, prepay)
    }
    pub(crate) fn require_shared_roots_registered(
        &self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        ensure!(self.native_receipts_transferred() && self.sql_custody.is_empty(),
            "Runtime native payload handoff remains incomplete");
        ensure!(self.roots.directory.is_none() && self.roots.namespace.is_none(),
            "Runtime retains an unregistered physical source");
        self.roots.directory_receipt.as_ref().context("actual Runtime directory close absent")?
            .require_original(self.roots.directory_owner.as_ref().context("actual Runtime directory owner absent")?)?;
        let namespace = self.shared_registrations[0].as_ref()
            .context("actual Runtime namespace registration absent")?;
        let quote = quote_runtime_membership(registry, 1)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        registry.require_membership(namespace)?;
        Ok(())
    }
    pub(crate) fn retain_namespace_retirement(
        &mut self,
        receipt: Arc<lillux_sqlite::RetiredOriginalFinalizerFile>,
    ) -> Result<()> {
        self.require_process()?;
        let owner = self.roots.namespace_owner.as_ref()
            .context("actual Runtime namespace owner absent")?;
        receipt.require_original(owner)?;
        self.roots.namespace_receipt = Some(receipt);
        Ok(())
    }
    pub(crate) fn physical_descriptors_retired(&self) -> Result<()> {
        self.require_process()?;
        ensure!(self.roots.directory.is_none() && self.roots.namespace.is_none(),
            "Runtime still holds original directory/namespace descriptors");
        self.roots.directory_receipt.as_ref().context("Runtime directory close pending")?
            .require_original(self.roots.directory_owner.as_ref().context("Runtime directory owner absent")?)?;
        if let Some(owner) = &self.roots.namespace_owner {
            self.roots.namespace_receipt.as_ref().context("Runtime namespace close pending")?
                .require_original(owner)?;
        } else {
            ensure!(self.shared_registrations[0].is_some(), "actual Runtime namespace membership absent");
        }
        for registration in self.shared_registrations.iter().flatten() { registration.receipt()?; }
        Ok(())
    }
    /// Move the actual unique namespace descriptor without unlocking it. The
    /// controller prepays owner storage before calling, and caches this token
    /// before any real close. Shared leases preserve the original namespace.
    pub(crate) fn prepare_namespace_owner(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>> {
        self.require_process()?;
        if let Some(owner) = &self.roots.namespace_owner { return Ok(owner.clone()); }
        let quote = quote_runtime_namespace_owner();
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        let namespace = self.roots.namespace.take().context("actual Runtime namespace owner absent")?;
        let descriptor = match namespace.try_into_owned_close_descriptor() {
            Ok(descriptor) => descriptor,
            Err(namespace) => {
                self.roots.namespace = Some(namespace);
                bail!("actual Runtime namespace still has shared original leases");
            }
        };
        // SAFETY: the prepaid retained outside producer moves the actual
        // namespace descriptor without LOCK_UN; no duplicate FD or new grant.
        let owner = unsafe { lillux_sqlite::RetainedOriginalFinalizerFile::issued(
            descriptor, lillux_sqlite::OriginalFinalizerFileKind::DirectoryLock,
        ) };
        self.roots.namespace_owner = Some(owner.clone());
        Ok(owner)
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
                    RuntimeConnection::OriginalMutation(owner) => {
                        ensure!(db.open_mode == RuntimeDbOpenMode::ExplicitHistoryReset
                            && owner.domain() == OriginalNativeDomain::RuntimeMutation,
                            "offline Runtime close changed its original mutation purpose");
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
                ensure!(
                    db._database_file.is_none() && db._wal_file.is_none() && db._shm_file.is_none(),
                    "original Runtime close has duplicate data descriptors"
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
                    RuntimeConnection::OriginalMutation(owner) => owner.domain(),
                    RuntimeConnection::Live(live) => live.owner().domain(),
                    _ => unreachable!("checked native Runtime owner"),
                },
                _schema_reference: None,
            };
            let mut context = context;
            let owner = match db.conn {
                RuntimeConnection::OriginalMutation(owner) => owner,
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
        ensure!(self.process == std::process::id(), "foreign Runtime host frame at exhaustion");
        ensure!(self.retired_roots_consumed && self.directory.is_none() && self.namespace.is_none()
            && self.registrations.iter().all(Option::is_none)
            && self.shared_directory_identity.is_none()
            && self.unused_runtime_source_is_retired(),
            "Runtime host frame retains a root, namespace or registration at exhaustion");
        if let OriginalRuntimeOutsideSemantics::Refused { error, .. } = &self.semantics {
            ryeos_state::state_db::require_exhausted_refusals(std::iter::once(&**error))?;
        }
        Ok(())
    }

    fn exhaust(self) {
        let Self { process: _, factory: _, directory, shared_directory_identity: _, namespace,
            semantics, registrations, retired_roots_consumed: _ } = self;
        drop(std::mem::ManuallyDrop::into_inner(directory));
        drop(std::mem::ManuallyDrop::into_inner(namespace));
        drop(registrations);
        match semantics {
            OriginalRuntimeOutsideSemantics::Closed { target, incarnation, domain: _ } => {
                drop(target);
                drop(incarnation);
            }
            OriginalRuntimeOutsideSemantics::Refused { error, unused_inputs, unused_parts } => {
                drop(std::mem::ManuallyDrop::into_inner(error));
                drop(std::mem::ManuallyDrop::into_inner(unused_inputs));
                if let Some(parts) = std::mem::ManuallyDrop::into_inner(unused_parts) {
                    let OriginalUnusedRuntimeBoundParts { root, authority, inspection, name,
                        mode: _, domain: _, registration } = parts;
                    drop(std::mem::ManuallyDrop::into_inner(root));
                    drop(std::mem::ManuallyDrop::into_inner(authority));
                    drop(std::mem::ManuallyDrop::into_inner(inspection));
                    drop(name);
                    drop(registration);
                }
            }
        }
    }
}

impl LiveRuntimeNativeSources {
    fn require_exhausted(&self) -> Result<()> {
        ensure!(self.complete(&None) && self.packets.is_empty(),
            "Runtime native sources or packets remain at exhaustion");
        Ok(())
    }

    fn exhaust(self) {
        let Self { main, schema, schema_definition, native_failure, packets } = self;
        drop(std::mem::ManuallyDrop::into_inner(main));
        drop(std::mem::ManuallyDrop::into_inner(schema));
        drop(std::mem::ManuallyDrop::into_inner(schema_definition));
        drop(std::mem::ManuallyDrop::into_inner(native_failure));
        drop(std::mem::ManuallyDrop::into_inner(packets));
    }
}

impl OriginalLiveRuntimeOutsideCustody {
    /// Consume the closed Runtime frame after its shared-root receipts.
    /// Refusal returns the frame intact; success leaves no owner.
    pub(crate) fn into_exhausted(self) -> std::result::Result<(), (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            ensure!(self.process == std::process::id(), "foreign Runtime frame at exhaustion");
            ensure!(self.context.is_none(), "Runtime close context remains at exhaustion");
            self.sources.require_exhausted()?;
            self.host.as_ref().context("Runtime host frame absent at exhaustion")?
                .require_exhausted()?;
            ryeos_state::state_db::require_exhausted_refusals(self.prior_refusal.iter())
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }
        let Self { process: _, context, host, sources, prior_refusal } = self;
        drop(std::mem::ManuallyDrop::into_inner(context));
        if let Some(host) = std::mem::ManuallyDrop::into_inner(host) {
            host.exhaust();
        }
        sources.exhaust();
        drop(std::mem::ManuallyDrop::into_inner(prior_refusal));
        Ok(())
    }
}

impl OriginalRuntimeAcquisitionOutsideCustody {
    /// Consume the refused-acquisition Runtime frame after its shared-root
    /// and unused-root receipts. Refusal returns the frame intact.
    pub(crate) fn into_exhausted(self) -> std::result::Result<(), (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            ensure!(self.process == std::process::id(), "foreign refused Runtime frame at exhaustion");
            ensure!(self.body.is_none(), "refused Runtime acquisition body remains at exhaustion");
            self.sources.require_exhausted()?;
            self.host.as_ref().context("refused Runtime host frame absent at exhaustion")?
                .require_exhausted()
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }
        let Self { process: _, body, host, sources } = self;
        drop(std::mem::ManuallyDrop::into_inner(body));
        if let Some(host) = std::mem::ManuallyDrop::into_inner(host) {
            host.exhaust();
        }
        sources.exhaust();
        Ok(())
    }
}

#[cfg(test)]
mod outside_quote_tests {
    use super::{MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES, quote_runtime_packet_sources,
        quote_runtime_retired_shared_roots, MAX_UNUSED_RUNTIME_ROOT_REGISTRATIONS};
    use lillux_sqlite::OriginalNativeOutsideConnectionCustody;

    #[test]
    fn runtime_packet_quote_covers_retained_vector_and_bounds() {
        let quote = quote_runtime_packet_sources(1, 2).unwrap();
        assert_eq!(quote.bytes,
            (3 * std::mem::size_of::<OriginalNativeOutsideConnectionCustody>()
                + std::mem::size_of::<Vec<()>>()) as u64);
        assert_eq!(quote.steps, 32);
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
