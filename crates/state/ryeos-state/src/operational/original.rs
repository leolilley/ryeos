//! Same-original Operational acquisition and consuming native retirement.
//! These owners retain actual partial effects; no close/reopen phase transfer.
use super::*;
use lillux::custody::Held;
use lillux_sqlite::original_physical_custody::{OriginalPhysicalRegistry, OutsideWork};
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeCloseFailure, OriginalNativeConnection,
    OriginalNativeConnectionTerminal, OriginalNativeDomain, OriginalNativeFactory,
    OriginalNativeOpenFailure,
};
use std::sync::{Arc, Mutex};

pub fn quote_operational_schema_sources(current: usize, incoming: usize) -> Result<OutsideWork> {
    let total = current
        .checked_add(incoming)
        .context("live Operational schema source quote overflow")?;
    anyhow::ensure!(
        total <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
        "Operational schema source quote exceeds its one fixed DDL owner"
    );
    let bytes = total
        .checked_mul(std::mem::size_of::<LiveOperationalSchemaSource>())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<()>>()))
        .context("live Operational schema source storage quote overflow")?;
    let steps = u64::try_from(incoming)?
        .checked_mul(4)
        .and_then(|steps| steps.checked_add(4))
        .context("live Operational schema source work quote overflow")?;
    Ok(OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps,
    })
}

pub fn quote_operational_live_context() -> Result<OutsideWork> {
    Ok(OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<OutsideOriginalOperationalCustody>())?,
        steps: 32,
    })
}

/// Maximum post-disposal receipt work for the actual Operational root and
/// namespace registrations in one retained State frame.
pub fn quote_operational_retired_shared_roots() -> Result<OutsideWork> {
    let slots = 2u64;
    let receipts = 2u64;
    let steps = slots
        .checked_mul(4)
        .and_then(|steps| receipts.checked_mul(12).and_then(|n| steps.checked_add(n)))
        .and_then(|steps| steps.checked_add(4))
        .context("Operational root receipt quote overflow")?;
    Ok(OutsideWork { bytes: 0, steps })
}

/// One actual retained marker descriptor owner and its Arc control block.
pub fn quote_operational_marker_owner() -> Result<OutsideWork> {
    let bytes = std::mem::size_of::<lillux_sqlite::RetainedOriginalFinalizerFile>()
        .checked_add(2 * std::mem::size_of::<usize>())
        .context("Operational marker owner storage quote overflow")?;
    Ok(OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps: 4,
    })
}

/// One fixed marker membership check and exact retired close receipt.
pub fn quote_operational_marker_receipt() -> OutsideWork {
    OutsideWork {
        bytes: 0,
        steps: 16,
    }
}

/// Bounded work to release the startup-only live Operational metadata after
/// its packet sources and exact physical receipts have been consumed.
pub fn quote_operational_live_metadata_retirement(
    schema_capacity: usize,
    reference_capacity: usize,
    has_initialization: bool,
) -> Result<OutsideWork> {
    anyhow::ensure!(
        schema_capacity <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES
            && reference_capacity <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
        "Operational metadata retirement exceeds its one fixed schema owner"
    );
    let initialization_steps = if has_initialization { 3 } else { 0 };
    let steps = u64::try_from(schema_capacity)?
        .checked_mul(4)
        .and_then(|steps| steps.checked_add(u64::try_from(reference_capacity).ok()?))
        .and_then(|steps| steps.checked_add(12 + initialization_steps))
        .context("Operational metadata retirement quote overflow")?;
    Ok(OutsideWork { bytes: 0, steps })
}

/// One pre-seal transition from the strong initialization owner to a fixed
/// weak allocation/ticket witness (or producer-qualified no-ticket evidence).
pub fn quote_operational_initialization_retirement() -> Result<OutsideWork> {
    Ok(OutsideWork { bytes: 0, steps: 8 })
}

pub fn quote_maximum_operational_live_metadata_retirement() -> Result<OutsideWork> {
    quote_operational_live_metadata_retirement(
        super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
        super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
        true,
    )
}

fn require_retired_operational_metadata_registry(
    identity: Option<&lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
) -> Result<()> {
    registry.require_same_registry(
        identity.context("Operational registry identity absent at metadata retirement")?,
    )
}

enum OperationalMarkerState {
    Unobserved,
    Pending(Arc<lillux_sqlite::RetainedOriginalFinalizerFile>),
    Registered {
        owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
        registration: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    },
    Absent,
    Consumed {
        had_descriptor: bool,
    },
}

/// Owns the exact live Operational marker descriptor through the same
/// original physical registry as the database roots. Failed-start owners may
/// instead record that no descriptor was retained at acquisition time.
pub(super) struct OperationalMarkerSource {
    process: u32,
    registry_identity:
        Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    state: OperationalMarkerState,
}

impl OperationalMarkerSource {
    pub(super) fn new() -> Self {
        Self {
            process: std::process::id(),
            registry_identity: None,
            state: OperationalMarkerState::Unobserved,
        }
    }
    /// A source whose marker receipt is already consumed, for frames built
    /// directly in tests.
    #[cfg(test)]
    pub(super) fn consumed_for_test(had_descriptor: bool) -> Self {
        Self {
            process: std::process::id(),
            registry_identity: None,
            state: OperationalMarkerState::Consumed { had_descriptor },
        }
    }

    fn bind_registry(
        &mut self,
        identity: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity,
    ) -> Result<()> {
        if let Some(expected) = &self.registry_identity {
            expected.require_same_registry(&identity)
        } else {
            self.registry_identity = Some(identity);
            Ok(())
        }
    }

    pub(super) fn register(
        &mut self,
        marker: Option<&mut Option<std::fs::File>>,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        absence_allowed: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign Operational marker source"
        );
        let identity = registry.identity()?;
        self.bind_registry(identity.clone())?;
        match &self.state {
            OperationalMarkerState::Registered { registration, .. } => {
                anyhow::ensure!(
                    marker.as_ref().is_none_or(|slot| slot.is_none()),
                    "registered Operational marker retains its source descriptor"
                );
                registry.require_membership(registration)?;
                anyhow::ensure!(matches!(registration.observed_source_identity()?,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::ControlRecord { .. }),
                    "Operational marker registration changed role");
                return Ok(());
            }
            OperationalMarkerState::Absent => {
                anyhow::ensure!(
                    absence_allowed && marker.as_ref().is_none_or(|slot| slot.is_none()),
                    "Operational marker absence proof changed"
                );
                return Ok(());
            }
            OperationalMarkerState::Consumed { .. } => {
                anyhow::bail!("retired Operational marker cannot reenter the registry")
            }
            OperationalMarkerState::Unobserved | OperationalMarkerState::Pending(_) => {}
        }

        if matches!(&self.state, OperationalMarkerState::Unobserved) {
            let Some(file) = marker.as_ref().and_then(|slot| slot.as_ref()) else {
                anyhow::ensure!(
                    absence_allowed,
                    "successful live Operational startup lost its original marker descriptor"
                );
                self.state = OperationalMarkerState::Absent;
                return Ok(());
            };
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.is_file(),
                "Operational marker descriptor is not a regular file"
            );
            let quote = quote_operational_marker_owner()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(quote.bytes, quote.steps)
            })?;
            anyhow::ensure!(
                !crate::foreign_process(self.process),
                "foreign Operational marker after funding"
            );
            self.registry_identity
                .as_ref()
                .context("Operational marker registry identity disappeared after funding")?
                .require_same_registry(&registry.identity()?)?;
            let file = marker
                .and_then(|slot| slot.take())
                .context("original Operational marker disappeared after funding")?;
            // SAFETY: this is the exact descriptor captured from the admitted
            // Operational marker path, and this owner registers and retires it
            // under the same original outside funding and registry. The marker
            // is pinned metadata-only, so it retires as a metadata pin.
            let owner = unsafe {
                lillux_sqlite::RetainedOriginalFinalizerFile::issued(
                    file,
                    lillux_sqlite::OriginalFinalizerFileKind::MetadataPin,
                )
            };
            self.state = OperationalMarkerState::Pending(owner);
        }

        let owner = match &self.state {
            OperationalMarkerState::Pending(owner) => Arc::clone(owner),
            _ => anyhow::bail!("Operational marker registration phase changed"),
        };
        match unsafe { registry.register_control_record(Arc::clone(&owner), prepay) } {
            Ok(registration) => {
                self.state = OperationalMarkerState::Registered {
                    owner,
                    registration,
                };
                Ok(())
            }
            Err((owner, error)) => {
                self.state = OperationalMarkerState::Pending(owner);
                Err(error)
            }
        }
    }

    pub(super) fn require_registered(
        &self,
        marker: Option<&Option<std::fs::File>>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        absence_allowed: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign Operational marker census"
        );
        let identity = registry.identity()?;
        self.registry_identity
            .as_ref()
            .context("Operational marker registry identity absent")?
            .require_same_registry(&identity)?;
        anyhow::ensure!(
            marker.is_none_or(|slot| slot.is_none()),
            "Operational marker source descriptor remains outside the registry"
        );
        match &self.state {
            OperationalMarkerState::Registered {
                registration,
                owner,
            } => {
                registry
                    .require_control_record_source(registration, owner)
                    .context("Operational marker registration changed its actual owner")?;
                anyhow::ensure!(matches!(registration.observed_source_identity()?,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::ControlRecord { .. }),
                    "Operational marker registration changed role");
                Ok(())
            }
            OperationalMarkerState::Absent if absence_allowed => Ok(()),
            _ => anyhow::bail!("Operational marker is neither registered nor typed absent"),
        }
    }

    pub(super) fn consume_retired(
        &mut self,
        marker: Option<&mut Option<std::fs::File>>,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        absence_allowed: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign retired Operational marker"
        );
        let identity = self
            .registry_identity
            .as_ref()
            .context("Operational marker registry identity absent before retirement")?;
        registry.require_same_registry(identity)?;
        let quote = quote_operational_marker_receipt();
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign retired Operational marker after funding"
        );
        registry.require_same_registry(
            self.registry_identity
                .as_ref()
                .context("Operational marker registry identity disappeared after funding")?,
        )?;
        anyhow::ensure!(
            marker.as_ref().is_none_or(|slot| slot.is_none()),
            "retired Operational marker retains its original File descriptor"
        );
        match &self.state {
            OperationalMarkerState::Registered {
                owner,
                registration,
            } => {
                anyhow::ensure!(matches!(registration.observed_source_identity()?,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::ControlRecord { .. }),
                    "retired Operational marker registration changed role");
                let receipt = registration.receipt_in(registry)?;
                receipt.require_original(owner)?;
                drop(receipt);
                self.state = OperationalMarkerState::Consumed {
                    had_descriptor: true,
                };
            }
            OperationalMarkerState::Absent if absence_allowed => {
                self.state = OperationalMarkerState::Consumed {
                    had_descriptor: false,
                };
            }
            OperationalMarkerState::Consumed { had_descriptor } => {
                anyhow::ensure!(
                    *had_descriptor || absence_allowed,
                    "retired Operational marker absence proof changed"
                );
            }
            _ => anyhow::bail!(
                "Operational marker lacks a registered or typed-absent retirement source"
            ),
        }
        Ok(())
    }

    pub(super) fn retired_or_absent(&self, absence_allowed: bool) -> bool {
        if crate::foreign_process(self.process) {
            return false;
        }
        match &self.state {
            OperationalMarkerState::Consumed {
                had_descriptor: true,
            } => true,
            OperationalMarkerState::Consumed {
                had_descriptor: false,
            } => absence_allowed,
            _ => false,
        }
    }
}

pub fn quote_operational_metadata() -> Result<OutsideWork> {
    Ok(OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<LiveOperationalMetadataCustody>())?,
        steps: 8,
    })
}

pub fn quote_operational_packet_sources(current: usize, incoming: usize) -> Result<OutsideWork> {
    let total = current
        .checked_add(incoming)
        .context("live Operational packet source quote overflow")?;
    anyhow::ensure!(
        total <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
        "Operational packet quote exceeds one schema source plus one main source"
    );
    let bytes = total
        .checked_mul(std::mem::size_of::<
            lillux_sqlite::OriginalNativeOutsideConnectionCustody,
        >())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<()>>()))
        .context("live Operational packet storage quote overflow")?;
    let steps = u64::try_from(incoming)?
        .checked_mul(8)
        .and_then(|steps| steps.checked_add(4))
        .context("live Operational packet work quote overflow")?;
    Ok(OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps,
    })
}

pub fn quote_operational_main_packet(current: usize) -> Result<OutsideWork> {
    let total = current
        .checked_add(1)
        .context("live Operational main packet source quote overflow")?;
    anyhow::ensure!(
        total <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
        "Operational main packet quote exceeds one schema source plus one main source"
    );
    let bytes = total
        .checked_mul(std::mem::size_of::<
            lillux_sqlite::OriginalNativeOutsideConnectionCustody,
        >())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<()>>()))
        .context("live Operational main packet storage quote overflow")?;
    Ok(OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps: 8,
    })
}

pub fn quote_operational_root_transfer() -> OutsideWork {
    OutsideWork {
        bytes: 512,
        steps: 4,
    }
}

pub fn quote_operational_membership(
    registry: &OriginalPhysicalRegistry,
    queries: u64,
) -> Result<OutsideWork> {
    registry
        .membership_work_bound(queries, 0)?
        .checked_add(OutsideWork { bytes: 0, steps: 8 })
}

enum Custody {
    BeforeOpen,
    Open(OriginalNativeOpenFailure),
    Live(OriginalNativeConnection),
    Operational(OperationalDb),
    Close(OriginalNativeCloseFailure),
    Terminal(OriginalNativeConnectionTerminal),
}
struct ContextOwner {
    factory: Arc<OriginalNativeFactory>,
    root: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
    read_only: bool,
    live: Option<LiveClosedOperationalContext>,
    live_domain: bool,
}
struct LiveOperationalSchemaSource {
    _ddl: String,
    _definition: crate::sqlite_schema::SchemaSqlDefinition,
    terminal: Option<OriginalNativeConnectionTerminal>,
}
const LIVE_CONTEXT_KIND: &str = "ryeos-state::operational-live-context";

/// Every owned field of a live closed Operational context, in the order its
/// release drops them. Constructible only in this module, so no caller can
/// swap the whole out from under its custody.
struct LiveClosedOperationalContextParts {
    schema_sources: Vec<LiveOperationalSchemaSource>,
    schema_native_failure: Option<(usize, lillux_sqlite::OriginalNativeOutsideCustodyFailure)>,
    original: Arc<crate::state_db::live_native::LiveStateNativeOwner>,
    references: super::live_native::LiveOperationalSchemaReferences,
    _initialization: Option<lillux_sqlite::DurableLiveOperationalInitialization>,
    initialization_retirement: Option<super::live_native::LiveOperationalInitializationRetirement>,
    _marker: Option<File>,
}
/// Settled when no schema source or schema transfer refusal still holds a
/// native receipt; the context then releases normally, its parent alias,
/// references, initialization and marker through their own drops. Otherwise
/// the whole context stays retained until the process exits. No schema source
/// keeps its native receipt and no schema transfer refusal holds one: only the
/// outside native transfer may end either.
fn live_context_schema_sources_settled(parts: &LiveClosedOperationalContextParts) -> bool {
    parts.schema_native_failure.is_none()
        && parts
            .schema_sources
            .iter()
            .all(|source| source.terminal.is_none())
}
struct LiveClosedOperationalContext {
    inner: Held<LiveClosedOperationalContextParts>,
    // A plain copied fact; it carries no custody.
    initialization_phase: super::live_native::LiveOperationalInitializationPhase,
}
impl LiveClosedOperationalContext {
    fn new(
        parts: LiveClosedOperationalContextParts,
        initialization_phase: super::live_native::LiveOperationalInitializationPhase,
    ) -> Self {
        Self {
            inner: Held::new(
                LIVE_CONTEXT_KIND,
                parts,
                live_context_schema_sources_settled,
            ),
            initialization_phase,
        }
    }
    /// Move every owned field out. A copy in another process is refused and
    /// comes back still held.
    fn into_parts(self) -> std::result::Result<LiveClosedOperationalContextParts, Self> {
        let Self {
            inner,
            initialization_phase,
        } = self;
        inner.take_whole().map_err(|inner| Self {
            inner,
            initialization_phase,
        })
    }
}
// Full live semantic/descriptor custody after the duplicate State-parent
// edge ends. None of these retained bodies is a disposal or settlement receipt.
const LIVE_METADATA_KIND: &str = "ryeos-state::operational-live-metadata";

/// Everything the metadata custody owns, in the order its release drops it.
/// Constructible only in this module.
struct LiveOperationalMetadataParts {
    parent: std::sync::Weak<crate::state_db::live_native::LiveStateNativeOwner>,
    factory: std::sync::Weak<OriginalNativeFactory>,
    schema_sources: Vec<LiveOperationalSchemaSource>,
    schema_native_failure: Option<(usize, lillux_sqlite::OriginalNativeOutsideCustodyFailure)>,
    references: super::live_native::LiveOperationalSchemaReferences,
    _initialization_retirement: Option<super::live_native::LiveOperationalInitializationRetirement>,
    _marker: Option<File>,
}
/// Settled exactly when live metadata retirement would accept it: no schema
/// native failure, no schema receipt, no original schema reference and no
/// marker descriptor remain.
fn live_metadata_settled(parts: &LiveOperationalMetadataParts) -> bool {
    parts.schema_native_failure.is_none()
        && parts
            .schema_sources
            .iter()
            .all(|source| source.terminal.is_none())
        && parts.references.original_source_count() == 0
        && parts._marker.is_none()
}
struct LiveOperationalMetadataCustody {
    inner: Held<LiveOperationalMetadataParts>,
}
impl LiveOperationalMetadataCustody {
    fn new(parts: LiveOperationalMetadataParts) -> Self {
        Self {
            inner: Held::new(LIVE_METADATA_KIND, parts, live_metadata_settled),
        }
    }
}
impl ContextOwner {
    fn domain(&self) -> OriginalNativeDomain {
        if self.live_domain {
            OriginalNativeDomain::NodeOperational
        } else if self.read_only {
            OriginalNativeDomain::OperationalInspection
        } else {
            OriginalNativeDomain::OperationalReplayReset
        }
    }
    fn require_roots(&self) -> Result<()> {
        // Reject a foreign incarnation before observing retained roots or
        // consulting the live owner graph. Descriptor identity is process-local.
        self.factory.require_original_process()?;
        if let Some(live) = &self.live {
            anyhow::ensure!(
                Arc::ptr_eq(live.inner.original.factory(), &self.factory),
                "live Operational close selects foreign factory"
            );
        }
        self.root.ensure_path_binding()?;
        self.namespace.ensure_protects(&self.root)
    }
}

pub struct OriginalOperationalFailure {
    error: anyhow::Error,
    context: ContextOwner,
    custody: Mutex<Custody>,
}
impl std::fmt::Debug for OriginalOperationalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalOperationalFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalOperationalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for OriginalOperationalFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Actual native terminal plus original namespace/factory and prior refusal.
/// A terminal after a refused operation never turns that operation successful.
pub struct ClosedOriginalOperational {
    context: ContextOwner,
    terminal: OriginalNativeConnectionTerminal,
    prior_refusal: Option<anyhow::Error>,
}
impl ClosedOriginalOperational {
    /// Consume actual live Operational schema sources into the same retained
    /// live context. Root, State-parent, initialization and marker remain owned.
    pub(crate) fn advance_live_schema_sources(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(
            self.context.factory.same_original(factory) && self.context.live_domain,
            "live Operational schema transfer selected foreign or offline context"
        );
        let live = self
            .context
            .live
            .as_mut()
            .context("actual live Operational context absent")?;
        let count = live.inner.references.original_source_count();
        anyhow::ensure!(
            count <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES
                && live
                    .inner
                    .schema_sources
                    .len()
                    .checked_add(count)
                    .is_some_and(|total| total <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES),
            "Operational schema source collection exceeds its one fixed DDL owner"
        );
        if count == 0 {
            return Ok(());
        }
        let quote = quote_operational_schema_sources(live.inner.schema_sources.len(), count)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        anyhow::ensure!(
            count <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES
                && live
                    .inner
                    .schema_sources
                    .len()
                    .checked_add(count)
                    .is_some_and(|total| total <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES),
            "Operational schema source bound changed after funding"
        );
        live.inner
            .get_mut()
            .schema_sources
            .try_reserve_exact(count)?;
        live.inner.references.require_original_sources(factory)?;
        // No fallible/native work follows checkout of these actual entries.
        // Existing DDL, definitions and receipts move without recreating source.
        for (ddl, reference) in live.inner.get_mut().references.take_original_sources() {
            let (definition, proof) = reference.into_original_parts();
            live.inner
                .get_mut()
                .schema_sources
                .push(LiveOperationalSchemaSource {
                    _ddl: ddl,
                    _definition: definition,
                    terminal: Some(proof),
                });
        }
        Ok(())
    }
    pub fn terminal(&self) -> OriginalNativeConnectionTerminal {
        self.terminal.clone()
    }
    pub fn native_terminals(&self) -> Result<Vec<OriginalNativeConnectionTerminal>> {
        self.require_original(&self.context.factory)?;
        let mut terminals = vec![self.terminal()];
        if let Some(live) = &self.context.live {
            terminals.extend(
                live.inner
                    .references
                    .native_terminals(&self.context.factory)?,
            );
            anyhow::ensure!(
                live.inner.schema_native_failure.is_none(),
                "Operational schema source already entered native outside handoff"
            );
            for source in live.inner.schema_sources.iter() {
                let proof = source
                    .terminal
                    .as_ref()
                    .context("Operational schema receipt already moved outside")?;
                proof.require_original(&self.context.factory)?;
                terminals.push(proof.clone());
            }
        }
        Ok(terminals)
    }

    pub fn prior_refusal(&self) -> Option<&anyhow::Error> {
        self.prior_refusal.as_ref()
    }
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        factory.require_original_process()?;
        self.context.factory.require_original_process()?;
        anyhow::ensure!(
            self.context.factory.same_original(factory),
            "foreign Operational close factory"
        );
        self.terminal.require_original(factory)?;
        self.context.require_roots()?;
        // Refused acquisition may release before a main identity exists. Such
        // a genuine Released terminal is not a successful Operational close.
        if let OriginalNativeConnectionTerminal::Closed(proof) = &self.terminal {
            anyhow::ensure!(
                proof.domain() == self.context.domain(),
                "Operational close domain changed"
            );
            let current = self
                .context
                .root
                .entry_no_follow(OsStr::new(OPERATIONAL_DB_FILENAME))?
                .context("closed Operational main disappeared")?;
            anyhow::ensure!(
                current.entry_type == lillux::PinnedEntryType::Regular
                    && proof.bound_main_identity()
                        == Some((current.containing_device, current.inode)),
                "closed Operational source differs from actual Bound identity"
            );
        }
        Ok(())
    }
}
/// Original offline Operational continuation. No terminal transfer or Drop
/// proves physical disposal; the actual outside controller retains this owner.
pub struct OutsideOriginalOperationalCustody {
    inner: Held<OutsideOperationalParts>,
}
const OUTSIDE_OPERATIONAL_KIND: &str = "ryeos-state::outside-operational-custody";

/// Everything the outside frame owns, in the order its release drops it,
/// followed by plain copied facts. Constructible only in this module, so no
/// caller can swap the whole out from under its custody.
struct OutsideOperationalParts {
    // The actual live parent, initialization proof, marker and schema bodies
    // remain accessible until their typed retirement. No offline shortcut.
    live: Option<LiveClosedOperationalContext>,
    live_metadata: Option<LiveOperationalMetadataCustody>,
    root: Option<Arc<lillux::PinnedDirectory>>,
    namespace: Option<lillux::PinnedDirectoryLock>,
    terminal: Option<OriginalNativeConnectionTerminal>,
    native_failure: Option<lillux_sqlite::OriginalNativeOutsideCustodyFailure>,
    sql_custody: Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
    // Fixed actual root and namespace roles; absence never means registered.
    shared_registrations:
        [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>; 2],
    shared_registry_identity:
        Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    marker_source: OperationalMarkerSource,
    descriptor_owners: Vec<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>>,
    descriptor_receipts: Vec<Option<Arc<lillux_sqlite::RetiredOriginalFinalizerFile>>>,
    prior_refusal: Option<anyhow::Error>,
    live_domain: bool,
    retired_shared_roots_consumed: bool,
}
impl OutsideOperationalParts {
    /// Only actual descriptor obligations, not durable/account settlement.
    fn descriptors_retired(&self) -> bool {
        self.live.is_none()
            && self.live_metadata.is_none()
            && self.root.is_none()
            && self.namespace.is_none()
            && self.terminal.is_none()
            && self.native_failure.is_none()
            && self.sql_custody.is_empty()
            && (if self.retired_shared_roots_consumed {
                self.shared_registrations.iter().all(Option::is_none)
            } else {
                self.shared_registrations
                    .iter()
                    .all(|binding| binding.as_ref().is_some_and(|binding| binding.retired()))
            })
            && self.descriptor_receipts.iter().all(Option::is_some)
            && self.marker_source.retired_or_absent(!self.live_domain)
    }
    /// The registry-free part of final exhaustion.
    fn require_exhausted_holders(&self) -> Result<()> {
        anyhow::ensure!(
            self.retired_shared_roots_consumed && self.descriptors_retired(),
            "Operational frame retains a physical or native owner at final exhaustion"
        );
        anyhow::ensure!(
            self.descriptor_owners.len() == self.descriptor_receipts.len(),
            "Operational descriptor receipt census changed"
        );
        for (owner, receipt) in self
            .descriptor_owners
            .iter()
            .zip(self.descriptor_receipts.iter())
        {
            receipt
                .as_ref()
                .context("Operational descriptor receipt absent")?
                .require_original(owner)?;
        }
        crate::state_db::live_native::require_exhausted_refusals(self.prior_refusal.iter())
    }
}
/// Settled exactly when final exhaustion would accept the frame: shared roots
/// consumed, every descriptor, root, marker, terminal, packet and live body
/// retired and no owner-bearing refusal left. The frame then releases
/// normally; otherwise the whole graph stays retained until the process exits.
fn outside_operational_exhausted(parts: &OutsideOperationalParts) -> bool {
    parts.require_exhausted_holders().is_ok()
}
impl ClosedOriginalOperational {
    /// Move the actual live context after genuine aggregate acknowledgement.
    /// This consumes existing owners only; it neither disposes descriptors nor
    /// proves parent/native affinity retired. Refusal returns the whole source.
    pub(crate) fn into_live_outside_custody(
        self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        parent: &Arc<crate::state_db::live_native::LiveStateNativeOwner>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<OutsideOriginalOperationalCustody, (Self, anyhow::Error)> {
        let checked =
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| -> Result<()> {
                factory.require_original_process()?;
                self.context.factory.require_original_process()?;
                aggregate.require_original(factory)?;
                anyhow::ensure!(
                    Arc::ptr_eq(parent.factory(), factory),
                    "live Operational handoff selected foreign State factory"
                );
                anyhow::ensure!(
                    self.context.factory.same_original(factory) && self.context.live_domain,
                    "live Operational handoff selected foreign or offline source"
                );
                let live = self
                    .context
                    .live
                    .as_ref()
                    .context("actual live Operational State context absent")?;
                anyhow::ensure!(
                    Arc::ptr_eq(&live.inner.original, parent),
                    "live Operational handoff changed actual State parent"
                );
                anyhow::ensure!(
                    live.inner.references.original_source_count() == 0,
                    "live Operational schema sources must transfer before context handoff"
                );
                let quote = quote_operational_live_context()?;
                lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                    prepay(quote.bytes, quote.steps)
                })?;
                lillux_sqlite::OriginalNativeDestinationAuthority::require_original(
                    parent.as_ref(),
                    factory,
                )?;
                self.require_original(factory)
            });
        if let Err(error) = checked {
            return Err((self, error));
        }
        let Self {
            context,
            terminal,
            prior_refusal,
        } = self;
        let ContextOwner {
            factory: source_factory,
            root,
            namespace,
            read_only: _,
            live,
            live_domain: _,
        } = context;
        let outside = OutsideOriginalOperationalCustody::from_parts(OutsideOperationalParts {
            live,
            live_metadata: None,
            root: Some(root),
            namespace: Some(namespace),
            terminal: Some(terminal),
            native_failure: None,
            sql_custody: Vec::new(),
            shared_registrations: std::array::from_fn(|_| None),
            shared_registry_identity: None,
            marker_source: OperationalMarkerSource::new(),
            descriptor_owners: Vec::new(),
            descriptor_receipts: Vec::new(),
            prior_refusal,
            live_domain: true,
            retired_shared_roots_consumed: false,
        });
        // The supplied original factory and actual State parent remain owned
        // by the outside supervisor. Every resource-bearing field is cached.
        drop(source_factory);
        Ok(outside)
    }
}
impl OutsideOriginalOperationalCustody {
    fn from_parts(parts: OutsideOperationalParts) -> Self {
        Self {
            inner: Held::new(
                OUTSIDE_OPERATIONAL_KIND,
                parts,
                outside_operational_exhausted,
            ),
        }
    }
    /// Preserve the complete live context before ending only its duplicate
    /// parent Arc. The supplied canonical State parent survives this call.
    /// No marker is closed and no initialization/schema evidence is discarded.
    pub(crate) fn normalize_live_context(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        parent: &Arc<crate::state_db::live_native::LiveStateNativeOwner>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        lillux_sqlite::OriginalNativeDestinationAuthority::require_original(
            parent.as_ref(),
            factory,
        )?;
        anyhow::ensure!(
            self.inner.live_domain && Arc::ptr_eq(parent.factory(), factory),
            "live Operational context normalization changed domain or factory"
        );
        anyhow::ensure!(
            self.inner.terminal.is_none()
                && self.inner.native_failure.is_none()
                && self.inner.sql_custody.is_empty(),
            "live Operational native packets must transfer before context normalization"
        );
        if let Some(metadata) = self.inner.live_metadata.as_ref() {
            anyhow::ensure!(
                self.inner.live.is_none()
                    && std::sync::Weak::ptr_eq(&metadata.inner.parent, &Arc::downgrade(parent))
                    && std::sync::Weak::ptr_eq(&metadata.inner.factory, &Arc::downgrade(factory))
                    && metadata.inner.schema_native_failure.is_none()
                    && metadata
                        .inner
                        .schema_sources
                        .iter()
                        .all(|source| source.terminal.is_none())
                    && metadata.inner.references.original_source_count() == 0,
                "retained live Operational metadata changed original custody"
            );
            return Ok(());
        }
        let live = self
            .inner
            .live
            .as_ref()
            .context("actual live Operational context absent")?;
        anyhow::ensure!(
            Arc::ptr_eq(&live.inner.original, parent)
                && live.inner.schema_native_failure.is_none()
                && live
                    .inner
                    .schema_sources
                    .iter()
                    .all(|source| source.terminal.is_none())
                && live.inner.references.original_source_count() == 0
                && live.inner._initialization.is_none()
                && live.inner.initialization_retirement.is_some(),
            "live Operational context still retains native/schema sources or a foreign parent"
        );
        let quote = quote_operational_metadata()?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        self.require_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        lillux_sqlite::OriginalNativeDestinationAuthority::require_original(
            parent.as_ref(),
            factory,
        )?;
        let parent_identity = Arc::downgrade(parent);
        let factory_identity = Arc::downgrade(factory);
        let live = self
            .inner
            .get_mut()
            .live
            .take()
            .expect("validated retained live Operational context");
        // Every owned field is read out exactly once. The initialization
        // phase is a plain copied fact. A copy in another process is put back
        // and refused.
        let LiveClosedOperationalContextParts {
            schema_sources,
            schema_native_failure,
            original,
            references,
            _initialization,
            initialization_retirement,
            _marker,
        } = match live.into_parts() {
            Ok(parts) => parts,
            Err(live) => {
                self.inner.get_mut().live = Some(live);
                anyhow::bail!("foreign Operational live context");
            }
        };
        drop(_initialization);
        self.inner.get_mut().live_metadata = Some(LiveOperationalMetadataCustody::new(
            LiveOperationalMetadataParts {
                parent: parent_identity,
                factory: factory_identity,
                schema_sources,
                schema_native_failure,
                references,
                _initialization_retirement: initialization_retirement,
                _marker,
            },
        ));
        // All non-parent fields are cached first. The caller's same actual
        // parent, not a reconstructed authority, retains the canonical body.
        drop(original);
        Ok(())
    }

    /// Advance the actual live main receipt into a retained native packet.
    /// The parent/schema/initialization/marker graph remains cached throughout;
    /// actual receipt uniqueness still decides whether native transfer can run.
    pub(crate) fn advance_live_native_receipt(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if let Some(metadata) = self.inner.live_metadata.as_ref() {
            anyhow::ensure!(
                self.inner.live.is_none()
                    && self.inner.live_domain
                    && std::sync::Weak::ptr_eq(&metadata.inner.factory, &Arc::downgrade(factory))
                    && self.inner.terminal.is_none()
                    && self.inner.native_failure.is_none()
                    && self.inner.sql_custody.is_empty()
                    && metadata.inner.schema_native_failure.is_none()
                    && metadata
                        .inner
                        .schema_sources
                        .iter()
                        .all(|source| source.terminal.is_none())
                    && metadata.inner.references.original_source_count() == 0
                    && metadata.inner._initialization_retirement.is_some(),
                "normalized Operational context retains native sources or changed factory"
            );
            metadata
                .inner
                ._initialization_retirement
                .as_ref()
                .expect("checked Operational retirement receipt")
                .require_factory_witness(&Arc::downgrade(factory))?;
            return Ok(());
        }
        let live = self
            .inner
            .live
            .as_ref()
            .context("actual live Operational native context absent")?;
        anyhow::ensure!(
            Arc::ptr_eq(live.inner.original.factory(), factory),
            "live Operational native handoff changed original factory"
        );
        if live.inner.initialization_retirement.is_none() {
            anyhow::ensure!(
                matches!(
                    live.initialization_phase,
                    super::live_native::LiveOperationalInitializationPhase::Durable
                        | super::live_native::LiveOperationalInitializationPhase::NotEntered {
                            established_marker: Some(true),
                        }
                ),
                "successful live Operational open has an unresolved initialization phase"
            );
            let quote = quote_operational_initialization_retirement()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(quote.bytes, quote.steps)
            })?;
            self.require_process()?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            let live = self
                .inner
                .get_mut()
                .live
                .as_mut()
                .context("actual live Operational context disappeared after funding")?;
            anyhow::ensure!(
                Arc::ptr_eq(live.inner.original.factory(), factory)
                    && live.inner.initialization_retirement.is_none(),
                "live Operational initialization source changed after funding"
            );
            let retirement = super::live_native::capture_initialization_retirement(
                live.initialization_phase,
                live.inner._initialization.as_ref(),
                None,
                None,
                factory,
            )?;
            live.inner.get_mut().initialization_retirement = Some(retirement);
            // The fixed weak receipt now carries the startup-only fact. Drop
            // the strong factory-owning durability proof before any seal.
            drop(live.inner.get_mut()._initialization.take());
        } else if let Some(live) = self.inner.live.as_ref() {
            anyhow::ensure!(
                live.inner._initialization.is_none(),
                "live Operational initialization owner remains beside its receipt"
            );
            live.inner
                .initialization_retirement
                .as_ref()
                .expect("checked initialization retirement receipt")
                .require_factory_witness(&Arc::downgrade(factory))?;
        }
        if self.inner.terminal.is_none() && self.inner.native_failure.is_none() {
            return self.advance_live_schema_receipts(factory, aggregate, prepay);
        }
        let quote = quote_operational_main_packet(self.inner.sql_custody.len())?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        // Revalidate after the external funding callback and before touching
        // the inherited packet allocation or its native receipt graph.
        self.require_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(
            self.inner.sql_custody.len() < super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
            "Operational main packet bound changed after funding"
        );
        self.inner.get_mut().sql_custody.try_reserve_exact(1)?;
        // No synthetic alias census: the native consuming handoff retains the
        // original refusal if any other actual receipt owner still exists.
        self.retire_native_references(factory, aggregate, &|_| Ok(()))?;
        self.advance_live_schema_receipts(factory, aggregate, prepay)
    }
    fn advance_live_schema_receipts(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let issuer = self.inner.issuer();
        let parts = self.inner.get_mut();
        let live = parts
            .live
            .as_mut()
            .context("actual live Operational schema context absent")?;
        anyhow::ensure!(
            Arc::ptr_eq(live.inner.original.factory(), factory),
            "live Operational schema handoff changed original factory"
        );
        let count = live
            .inner
            .schema_sources
            .iter()
            .filter(|source| source.terminal.is_some())
            .count()
            .checked_add(usize::from(live.inner.schema_native_failure.is_some()))
            .context("live Operational schema receipt count overflow")?;
        anyhow::ensure!(
            live.inner.schema_sources.len() <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES
                && count <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
            "Operational schema packet sources exceed their one fixed DDL owner"
        );
        anyhow::ensure!(
            parts
                .sql_custody
                .len()
                .checked_add(count)
                .is_some_and(|total| total <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES),
            "Operational packet collection exceeds one schema source plus one main source"
        );
        if count == 0 {
            return Ok(());
        }
        let quote = quote_operational_packet_sources(parts.sql_custody.len(), count)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        // The live context was borrowed in the original process. Do not read
        // or mutate it after a funding callback unless that process still owns
        // the entire retained graph.
        anyhow::ensure!(
            !lillux::custody::foreign_process(issuer),
            "foreign Operational schema custody after funding callback"
        );
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(
            parts
                .sql_custody
                .len()
                .checked_add(count)
                .is_some_and(|total| total <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES),
            "Operational schema packet bound changed after funding"
        );
        parts.sql_custody.try_reserve_exact(count)?;
        if let Some((index, _)) = &live.inner.schema_native_failure {
            anyhow::ensure!(
                live.inner
                    .schema_sources
                    .get(*index)
                    .is_some_and(|source| source.terminal.is_none()),
                "retained Operational schema failure lost its original source slot"
            );
        }
        if let Some((index, failure)) = live.inner.get_mut().schema_native_failure.take() {
            match failure.into_original() {
                Ok(original) => {
                    live.inner.get_mut().schema_sources[index].terminal = Some(original)
                }
                Err(failure) => {
                    live.inner.get_mut().schema_native_failure = Some((index, failure));
                    anyhow::bail!("Operational schema native failure retains its original receipt");
                }
            }
        }
        // Validate the complete remaining finite source list before consuming
        // any receipt. Actual source bodies/DDL stay cached in their same slots.
        for source in live.inner.schema_sources.iter() {
            if let Some(proof) = &source.terminal {
                proof.require_original(factory)?;
            }
        }
        for (index, source) in live.inner.get_mut().schema_sources.iter_mut().enumerate() {
            let Some(proof) = source.terminal.take() else {
                continue;
            };
            match proof.into_outside_custody(aggregate) {
                Ok(packet) => parts.sql_custody.push(packet),
                Err(failure) => {
                    live.inner.get_mut().schema_native_failure = Some((index, failure));
                    anyhow::bail!("Operational schema native handoff retains original custody");
                }
            }
        }
        Ok(())
    }
    fn require_process(&self) -> Result<()> {
        anyhow::ensure!(
            !self.inner.is_foreign(),
            "foreign Operational outside custody"
        );
        Ok(())
    }
    pub fn prior_refusal(&self) -> Option<&anyhow::Error> {
        self.inner.prior_refusal.as_ref()
    }
    pub fn retire_native_references(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
        retire_aliases: &dyn Fn(&OriginalNativeConnectionTerminal) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        terminal.require_original(factory)?;
        if let Some(failure) = self.inner.get_mut().native_failure.take() {
            match failure.into_original() {
                Ok(original) => self.inner.get_mut().terminal = Some(original),
                Err(failure) => {
                    self.inner.get_mut().native_failure = Some(failure);
                    anyhow::bail!("Operational native refusal retains original terminal");
                }
            }
        }
        if let Some(original) = self.inner.terminal.as_ref() {
            original.require_original(factory)?;
            // Preserve the genuine canonical receipt through ordinary refusal
            // and unwind; the callback may remove only matching census aliases.
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                retire_aliases(original)
            })?;
        }
        // Alias retirement is an external callback too. Preserve the source
        // receipt if it returns in a foreign incarnation; never consume it on
        // the strength of the check made before that callback.
        self.require_process()?;
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        if let Some(original) = self.inner.get_mut().terminal.take() {
            match original.into_outside_custody(terminal) {
                Ok(custody) => self.inner.get_mut().sql_custody.push(custody),
                Err(failure) => {
                    let message = format!(
                        "Operational native transfer retains original terminal: {failure:#}"
                    );
                    self.inner.get_mut().native_failure = Some(failure);
                    anyhow::bail!(message);
                }
            }
        }
        Ok(())
    }
    /// # Safety
    /// Cache these actual payloads under the original outside controller before
    /// any further effect. Payload authority/root affinities still need their
    /// consuming retirement; zero native handles must be independently proved.
    pub unsafe fn take_original_sql_custody(
        &mut self,
    ) -> Result<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>> {
        self.require_process()?;
        anyhow::ensure!(
            self.inner.terminal.is_none() && self.inner.native_failure.is_none(),
            "Operational native handoff incomplete"
        );
        if let Some(live) = &self.inner.live {
            anyhow::ensure!(
                live.inner.schema_native_failure.is_none()
                    && live
                        .inner
                        .schema_sources
                        .iter()
                        .all(|source| source.terminal.is_none()),
                "Operational live schema native handoff incomplete"
            );
        }
        anyhow::ensure!(
            self.inner.sql_custody.len() <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
            "Operational packet vector exceeds one schema source plus one main source"
        );
        Ok(std::mem::take(&mut self.inner.get_mut().sql_custody))
    }
    /// # Safety
    /// Join the actual SAME original physical inventory as every other closed
    /// subsystem before sealing it. Refusal restores the exact original owner.
    pub unsafe fn register_shared_roots(
        &mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        anyhow::ensure!(
            !self.inner.retired_shared_roots_consumed,
            "retired Operational roots cannot be registered again"
        );
        let actual_registry = registry.identity()?;
        if let Some(expected) = self.inner.shared_registry_identity.as_ref() {
            expected.require_same_registry(&actual_registry)?;
        } else {
            self.inner.get_mut().shared_registry_identity = Some(actual_registry);
        }
        anyhow::ensure!(
            self.inner.terminal.is_none()
                && self.inner.native_failure.is_none()
                && self.inner.sql_custody.is_empty()
                && self.inner.live.is_none(),
            "Operational native payload or live context handoff incomplete"
        );
        anyhow::ensure!(
            !self.inner.live_domain || self.inner.live_metadata.is_some(),
            "live Operational metadata custody was not preserved"
        );
        let live_domain = self.inner.live_domain;
        let parts = self.inner.get_mut();
        let marker = parts
            .live_metadata
            .as_mut()
            .map(|metadata| &mut metadata.inner.get_mut()._marker);
        parts
            .marker_source
            .register(marker, registry, prepay, !live_domain)?;
        if self.inner.root.is_some() {
            let quote = quote_operational_root_transfer();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.require_process()?;
            let actual = self.inner.get_mut().root.take().unwrap();
            match unsafe { registry.register_directory(actual, prepay) } {
                Ok(binding) => self.inner.get_mut().shared_registrations[0] = Some(binding),
                Err((actual, error)) => {
                    self.inner.get_mut().root = Some(actual);
                    return Err(error);
                }
            }
        }
        if self.inner.namespace.is_some() {
            let quote = quote_operational_root_transfer();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.require_process()?;
            let actual = self.inner.get_mut().namespace.take().unwrap();
            match unsafe { registry.register_namespace(actual, prepay) } {
                Ok(binding) => self.inner.get_mut().shared_registrations[1] = Some(binding),
                Err((actual, error)) => {
                    self.inner.get_mut().namespace = Some(actual);
                    return Err(error);
                }
            }
        }
        self.require_shared_roots_registered(registry, prepay)
    }
    /// Verify actual root/namespace transfers into this SAME original registry.
    /// This is registration evidence only, not native exclusion, descriptor
    /// retirement or settlement. Refusal leaves all original custody intact.
    pub fn require_shared_roots_registered(
        &self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        anyhow::ensure!(
            !self.inner.retired_shared_roots_consumed,
            "retired Operational roots cannot reenter the live registry census"
        );
        let actual_registry = registry.identity()?;
        self.inner
            .shared_registry_identity
            .as_ref()
            .context("Operational shared-root registry identity absent")?
            .require_same_registry(&actual_registry)?;
        anyhow::ensure!(
            self.inner.live.is_none()
                && self.inner.terminal.is_none()
                && self.inner.native_failure.is_none()
                && self.inner.sql_custody.is_empty()
                && self.inner.root.is_none()
                && self.inner.namespace.is_none(),
            "Operational original owners remain outside registration"
        );
        anyhow::ensure!(
            !self.inner.live_domain || self.inner.live_metadata.is_some(),
            "live Operational metadata custody was not preserved"
        );
        anyhow::ensure!(
            self.inner.shared_registrations.iter().all(Option::is_some),
            "actual Operational root or namespace registration missing"
        );
        let marker_queries = if self.inner.live_domain { 1 } else { 0 };
        let quote = quote_operational_membership(registry, 2 + marker_queries)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        for (index, slot) in self.inner.shared_registrations.iter().enumerate() {
            let binding = slot
                .as_ref()
                .context("actual Operational root or namespace registration missing")?;
            registry.require_membership(binding)?;
            let identity = binding.observed_source_identity()?;
            anyhow::ensure!(matches!((index, identity),
                (0, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
                | (1, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })),
                "Operational registered original role changed");
        }
        let marker = self
            .inner
            .live_metadata
            .as_ref()
            .map(|metadata| &metadata.inner._marker);
        self.inner
            .marker_source
            .require_registered(marker, registry, !self.inner.live_domain)?;
        Ok(())
    }

    /// Consume the root registrations and any live marker descriptor after
    /// their exact close receipts belong to the same retired registry.
    pub fn consume_retired_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        let identity = self
            .inner
            .shared_registry_identity
            .as_ref()
            .context("Operational shared-root registry identity absent before retirement")?;
        registry.require_same_registry(identity)?;
        let quote = quote_operational_retired_shared_roots()?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        registry.require_same_registry(
            self.inner
                .shared_registry_identity
                .as_ref()
                .context("Operational shared-root registry identity disappeared after funding")?,
        )?;
        if self.inner.retired_shared_roots_consumed {
            anyhow::ensure!(
                self.inner.shared_registrations.iter().all(Option::is_none)
                    && self.inner.root.is_none()
                    && self.inner.namespace.is_none(),
                "consumed Operational root registrations reappeared"
            );
            let live_domain = self.inner.live_domain;
            let parts = self.inner.get_mut();
            parts.marker_source.consume_retired(
                parts
                    .live_metadata
                    .as_mut()
                    .map(|metadata| &mut metadata.inner.get_mut()._marker),
                registry,
                prepay,
                !live_domain,
            )?;
            return Ok(());
        }
        anyhow::ensure!(
            self.inner.root.is_none() && self.inner.namespace.is_none(),
            "retired Operational frame retains a physical root alias"
        );
        for (index, binding) in self.inner.shared_registrations.iter().enumerate() {
            let binding = binding
                .as_ref()
                .context("retired Operational root or namespace registration absent")?;
            let source = binding.observed_source_identity()?;
            anyhow::ensure!(matches!((index, source),
                (0, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
                | (1, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })),
                "retired Operational root registration changed role");
        }
        for binding in self.inner.shared_registrations.iter().flatten() {
            drop(binding.receipt_in(registry)?);
        }
        let live_domain = self.inner.live_domain;
        let parts = self.inner.get_mut();
        parts.marker_source.consume_retired(
            parts
                .live_metadata
                .as_mut()
                .map(|metadata| &mut metadata.inner.get_mut()._marker),
            registry,
            prepay,
            !live_domain,
        )?;
        self.inner.get_mut().shared_registrations = std::array::from_fn(|_| None);
        self.inner.get_mut().retired_shared_roots_consumed = true;
        Ok(())
    }

    fn require_live_metadata_retirement_source(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<OutsideWork> {
        self.require_process()?;
        anyhow::ensure!(
            self.inner.live_domain
                && self.inner.live.is_none()
                && self.inner.live_metadata.is_some(),
            "live Operational metadata retirement lacks its normalized source"
        );
        anyhow::ensure!(
            self.inner.retired_shared_roots_consumed
                && self.inner.shared_registrations.iter().all(Option::is_none)
                && self.inner.root.is_none()
                && self.inner.namespace.is_none()
                && self.inner.terminal.is_none()
                && self.inner.native_failure.is_none()
                && self.inner.sql_custody.is_empty()
                && self.inner.descriptor_receipts.iter().all(Option::is_some)
                && self.inner.marker_source.retired_or_absent(false),
            "Operational physical sources remain before live metadata retirement"
        );
        require_retired_operational_metadata_registry(
            self.inner.shared_registry_identity.as_ref(),
            registry,
        )?;
        let metadata = self
            .inner
            .live_metadata
            .as_ref()
            .expect("checked live metadata source");
        anyhow::ensure!(
            metadata.inner.schema_native_failure.is_none()
                && metadata.inner.schema_sources.len()
                    <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES
                && metadata
                    .inner
                    .schema_sources
                    .iter()
                    .all(|source| source.terminal.is_none())
                && metadata.inner.references.original_source_count() == 0
                && metadata.inner._marker.is_none(),
            "live Operational metadata retains an unconsumed native or marker source"
        );
        metadata
            .inner
            ._initialization_retirement
            .as_ref()
            .context("live Operational initialization disposition absent")?
            .require_factory_witness(&metadata.inner.factory)?;
        let quote = quote_operational_live_metadata_retirement(
            metadata.inner.schema_sources.capacity(),
            metadata.inner.references.retirement_capacity(),
            true,
        )?;
        Ok(quote)
    }

    /// Consume startup-only semantic metadata after the exact live State and
    /// Operational root receipts, marker receipt, and Store packet sources
    /// have completed. Physical/native owners are checked before prepayment;
    /// refusal leaves the complete metadata frame available for retry.
    pub(crate) fn consume_retired_live_metadata(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        if self.inner.live_metadata.is_none() {
            anyhow::ensure!(
                !self.inner.live_domain || self.inner.retired_shared_roots_consumed,
                "live Operational metadata disappeared before its root receipts"
            );
            if self.inner.live_domain {
                require_retired_operational_metadata_registry(
                    self.inner.shared_registry_identity.as_ref(),
                    registry,
                )?;
            }
            return Ok(());
        }
        let quote = self.require_live_metadata_retirement_source(registry)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        let checked_quote = self.require_live_metadata_retirement_source(registry)?;
        anyhow::ensure!(
            quote.bytes == checked_quote.bytes && quote.steps == checked_quote.steps,
            "Operational metadata retirement quote changed after prepayment"
        );

        // All fallible identity, receipt, and funding checks are complete.
        // The exact owner is consumed once; the empty replacement keeps this
        // slot empty for idempotent later observations.
        let metadata = self.inner.get_mut().live_metadata.take();
        let metadata = metadata.expect("validated live Operational metadata");
        // The checks above include its settled predicate, so its Drop
        // releases every field normally.
        drop(metadata);
        Ok(())
    }

    pub fn descriptor_owners(&self) -> &[Arc<lillux_sqlite::RetainedOriginalFinalizerFile>] {
        &self.inner.descriptor_owners
    }
    /// Only actual descriptor obligations, not durable/account settlement.
    pub fn descriptors_retired(&self) -> bool {
        !self.inner.is_foreign() && self.inner.descriptors_retired()
    }
}
impl OriginalOperationalFailure {
    /// Consume the actual stored phase before effects. No bookkeeping lock
    /// spans native cleanup, and a retry cannot reopen or renew the grant.
    pub fn retry_close(self) -> std::result::Result<ClosedOriginalOperational, Self> {
        let Self {
            error,
            context,
            custody,
        } = self;
        advance_close(
            context,
            custody.into_inner().unwrap_or_else(|p| p.into_inner()),
            Some(error),
        )
    }
}
// Cleanup must remain available after SQL admission/deadline refusal. Validate
// retained ownership metadata here; actual namespace/incarnation is checked
// against the native terminal after close, without another SQL admission.
fn require_no_duplicate_keepers(db: &OperationalDb) -> Result<()> {
    anyhow::ensure!(
        db._initialization_marker.is_none() || matches!(&db.conn, OperationalConnection::Live(_)),
        "original Operational owner has duplicated protocol keepers"
    );
    Ok(())
}

/// The refusal of a close the native failure still retains. The failure keeps
/// its typed cause for its own next retry, so this owned refusal carries the
/// cause's text and its class: a cause that is not a fact (contention, an
/// unobservable check, a momentary host-resource shortage) refuses as an
/// unobservable check and does not latch, and a fact cause refuses as a fact.
fn close_pending_refusal(what: &str, cause: Option<&anyhow::Error>) -> anyhow::Error {
    match cause {
        Some(cause) if !lillux_sqlite::native_refusal_is_fact(cause) => {
            anyhow::Error::new(lillux_sqlite::NativeCheckUnobservable)
                .context(format!("{what}: {cause:#}"))
        }
        Some(cause) => anyhow::anyhow!("{what}: {cause:#}"),
        None => anyhow::anyhow!("{what}"),
    }
}

/// This close attempt's refusal, layered over an earlier attempt's refusal
/// when there was one. The result is classified by this attempt's refusal
/// alone; the earlier one is kept for display.
fn close_refusal_over_prior(error: anyhow::Error, prior: Option<anyhow::Error>) -> anyhow::Error {
    use lillux_sqlite::RefusalSupersedes as _;
    match prior {
        Some(earlier) => error.superseding(earlier),
        None => error,
    }
}

fn advance_close(
    mut context: ContextOwner,
    custody: Custody,
    prior: Option<anyhow::Error>,
) -> std::result::Result<ClosedOriginalOperational, OriginalOperationalFailure> {
    let attempted = match custody {
        Custody::BeforeOpen => Err((
            anyhow::anyhow!("Operational acquisition had no native terminal"),
            Custody::BeforeOpen,
        )),
        Custody::Open(mut owner) => {
            if owner.retained_admission().is_some() {
                match owner.recover_retained_admission() {
                    Ok(()) => owner
                        .retry_close()
                        .map(OriginalNativeConnectionTerminal::Released)
                        .map_err(|owner| {
                            (
                                anyhow::anyhow!("Operational acquisition cleanup pending"),
                                Custody::Open(owner),
                            )
                        }),
                    Err(error) => Err((error, Custody::Open(owner))),
                }
            } else {
                owner
                    .retry_close()
                    .map(OriginalNativeConnectionTerminal::Released)
                    .map_err(|owner| {
                        (
                            anyhow::anyhow!("Operational acquisition cleanup pending"),
                            Custody::Open(owner),
                        )
                    })
            }
        }
        Custody::Live(owner) => owner
            .close()
            .map(OriginalNativeConnectionTerminal::Closed)
            .map_err(|owner| {
                (
                    close_pending_refusal("Operational close pending", owner.error()),
                    Custody::Close(owner),
                )
            }),
        Custody::Operational(db) => {
            let checked = (|| -> Result<()> {
                match &db.conn {
                    OperationalConnection::Original {
                        owner,
                        factory,
                        reference: _,
                    } => {
                        anyhow::ensure!(
                            !context.live_domain
                                && context.factory.same_original(factory)
                                && owner.domain() == context.domain(),
                            "Operational close factory/domain changed"
                        );
                    }
                    OperationalConnection::Live(live) => {
                        anyhow::ensure!(
                            context.live_domain
                                && Arc::ptr_eq(live.original.factory(), &context.factory)
                                && live.owner.domain() == OriginalNativeDomain::NodeOperational,
                            "live Operational close changed original purpose/factory"
                        );
                    }
                }
                require_no_duplicate_keepers(&db)
            })();
            if let Err(error) = checked {
                return Err(OriginalOperationalFailure {
                    error: prior.unwrap_or(error),
                    context,
                    custody: Mutex::new(Custody::Operational(db)),
                });
            }
            let owner = match db.conn {
                OperationalConnection::Original {
                    owner,
                    factory: _,
                    reference: _,
                } => owner,
                OperationalConnection::Live(live) => {
                    context.live = Some(LiveClosedOperationalContext::new(
                        LiveClosedOperationalContextParts {
                            schema_sources: Vec::new(),
                            schema_native_failure: None,
                            original: live.original,
                            references: live.references,
                            _initialization: live.initialization,
                            initialization_retirement: None,
                            _marker: db._initialization_marker,
                        },
                        live.initialization_phase,
                    ));
                    live.owner
                }
            };
            owner
                .close()
                .map(OriginalNativeConnectionTerminal::Closed)
                .map_err(|owner| {
                    (
                        close_pending_refusal("Operational close pending", owner.error()),
                        Custody::Close(owner),
                    )
                })
        }
        Custody::Close(owner) => owner
            .retry()
            .map(OriginalNativeConnectionTerminal::Closed)
            .map_err(|owner| {
                (
                    close_pending_refusal("Operational native close retry pending", owner.error()),
                    Custody::Close(owner),
                )
            }),
        Custody::Terminal(terminal) => Ok(terminal),
    };
    let terminal = match attempted {
        Ok(terminal) => terminal,
        Err((error, custody)) => {
            return Err(OriginalOperationalFailure {
                error: close_refusal_over_prior(error, prior),
                context,
                custody: Mutex::new(custody),
            });
        }
    };
    let closed = ClosedOriginalOperational {
        context,
        terminal,
        prior_refusal: prior,
    };
    if let Err(error) = closed.require_original(&closed.context.factory) {
        return Err(OriginalOperationalFailure {
            error: closed.prior_refusal.unwrap_or(error),
            context: closed.context,
            custody: Mutex::new(Custody::Terminal(closed.terminal)),
        });
    }
    Ok(closed)
}

impl PreparedReplayIndexReset {
    /// Consume preparation without granting replay service or mutation. Native
    /// refusal stays in the typed error owner carried by anyhow, and a missing
    /// transferred owner can never manufacture a close terminal.
    pub fn close_original(
        mut self,
        factory: Arc<OriginalNativeFactory>,
    ) -> Result<ClosedOriginalOperational> {
        let db = self
            .db
            .take()
            .context("prepared Operational owner already transferred")?;
        db.close_original(factory, self.read_only)
            .map_err(anyhow::Error::new)
    }

    /// Caller supplies the exact original issued inputs for this runtime root
    /// and mode, plus the closed current-schema reference it built through the
    /// same factory (see [`prepare_operational_schema_reference`]); the caller
    /// keeps that reference's terminal for its census. No ordinary connection,
    /// initialization or schema repair fallback.
    pub fn prepare_original(
        root: Arc<lillux::PinnedDirectory>,
        namespace: lillux::PinnedDirectoryLock,
        factory: Arc<OriginalNativeFactory>,
        inputs: OriginalNativeBoundInputs,
        reference: &sqlite_schema::ClosedNativeSchemaSqlReference,
        read_only: bool,
    ) -> std::result::Result<Self, OriginalOperationalFailure> {
        let context = ContextOwner {
            factory,
            root,
            namespace,
            read_only,
            live: None,
            live_domain: false,
        };
        let mut custody = Custody::BeforeOpen;
        let prepared = (|| -> Result<ReplayIndexResetReport> {
            context.require_roots()?;
            context.factory.require_runtime_binding(&context.root)?;
            reference.require_original_closed(
                &context.factory,
                lillux_sqlite::OriginalNativeDomain::NodeOperationalSchemaReference,
            )?;
            let directory = context.root.clone();
            anyhow::ensure!(
                inputs.domain() == context.domain(),
                "original Operational requested/input domain mismatch before open"
            );
            let owner = match context.factory.open_bound(inputs) {
                Ok(owner) => owner,
                Err(owner) => {
                    let error =
                        anyhow::anyhow!("original Operational acquisition: {}", owner.error());
                    custody = Custody::Open(owner);
                    return Err(error);
                }
            };
            custody = Custody::Live(owner);
            let Custody::Live(owner) = &custody else {
                unreachable!()
            };
            anyhow::ensure!(
                owner.domain() == context.domain(),
                "original Operational acquisition domain mismatch"
            );
            let path = directory.path().join(OPERATIONAL_DB_FILENAME);
            owner.with_sql(|connection| {
                configure_connection(connection)?;
                assert_operational_identity(connection, &path)?;
                assert_integrity(connection, &path)?;
                let mode: String =
                    connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                anyhow::ensure!(
                    mode == "wal",
                    "original Operational source is not established WAL"
                );
                Ok(())
            })?;
            let Custody::Live(owner) = std::mem::replace(&mut custody, Custody::BeforeOpen) else {
                unreachable!()
            };
            // Adopt actual native custody before the final report/binding checks.
            custody = Custody::Operational(OperationalDb {
                conn: OperationalConnection::Original {
                    owner,
                    factory: context.factory.clone(),
                    reference: reference.definition().clone(),
                },
                path,
                _runtime_directory: directory,
                _directory_lock: context.namespace.clone(),
                _initialization_marker: None,
            });
            let Custody::Operational(db) = &custody else {
                unreachable!()
            };
            ensure_operational_bindings(db)?;
            db.with_sql(|connection| {
                inspect_replay_index_reset(connection, &db.path, db.current_schema_reference())
            })
        })();
        let report = match prepared {
            Ok(report) => report,
            Err(error) => {
                return Err(OriginalOperationalFailure {
                    error,
                    context,
                    custody: Mutex::new(custody),
                });
            }
        };
        let Custody::Operational(db) = custody else {
            unreachable!()
        };
        Ok(Self {
            db: Some(db),
            report,
            read_only,
            activation: ReplayResetActivationPhase::Prepared,
        })
    }
}
impl OperationalDb {
    /// Consume only the actual live Operational owner; offline close remains
    /// a distinct purpose and cannot stand in for this service terminal.
    pub fn close_live(
        self,
        factory: Arc<OriginalNativeFactory>,
    ) -> std::result::Result<ClosedOriginalOperational, OriginalOperationalFailure> {
        let context = ContextOwner {
            factory,
            root: Arc::clone(&self._runtime_directory),
            namespace: self._directory_lock.clone(),
            read_only: false,
            live: None,
            live_domain: true,
        };
        advance_close(context, Custody::Operational(self), None)
    }

    /// Consume the actual original Operational owner, never infer native close
    /// from an ordinary handle or reopen it for terminal validation.
    pub fn close_original(
        self,
        factory: Arc<OriginalNativeFactory>,
        read_only: bool,
    ) -> std::result::Result<ClosedOriginalOperational, OriginalOperationalFailure> {
        let root = self._runtime_directory.clone();
        let context = ContextOwner {
            factory,
            root,
            namespace: self._directory_lock.clone(),
            read_only,
            live: None,
            live_domain: false,
        };
        let checked = (|| -> Result<()> {
            let OperationalConnection::Original {
                owner,
                factory,
                reference: _,
            } = &self.conn
            else {
                anyhow::bail!("ordinary Operational handle cannot provide original close evidence")
            };
            anyhow::ensure!(
                context.factory.same_original(factory) && owner.domain() == context.domain(),
                "Operational close factory/domain changed"
            );
            require_no_duplicate_keepers(&self)
        })();
        if let Err(error) = checked {
            return Err(OriginalOperationalFailure {
                error,
                context,
                custody: Mutex::new(Custody::Operational(self)),
            });
        }
        advance_close(context, Custody::Operational(self), None)
    }
}

#[cfg(test)]
mod abandoned_schema_source_tests {
    use super::{LiveClosedOperationalContext, LiveOperationalMetadataCustody};
    use std::sync::Arc;

    const CONTEXT: &str = "ryeos-state::operational-live-context";
    const METADATA: &str = "ryeos-state::operational-live-metadata";

    fn metadata(marker: Option<std::fs::File>) -> LiveOperationalMetadataCustody {
        LiveOperationalMetadataCustody::new(super::LiveOperationalMetadataParts {
            parent: std::sync::Weak::new(),
            factory: std::sync::Weak::new(),
            schema_sources: Vec::new(),
            schema_native_failure: None,
            references: super::super::live_native::LiveOperationalSchemaReferences::empty_for_test(
            ),
            _initialization_retirement: None,
            _marker: marker,
        })
    }

    #[test]
    fn settled_live_metadata_releases_normally() {
        let before = lillux::custody::abandoned_custody().count(METADATA);
        drop(metadata(None));
        assert_eq!(lillux::custody::abandoned_custody().count(METADATA), before);
    }

    #[test]
    fn live_metadata_with_its_marker_is_retained_whole() {
        let marker = tempfile::tempfile().unwrap();
        let before = lillux::custody::abandoned_custody().count(METADATA);
        drop(metadata(Some(marker)));
        assert_eq!(
            lillux::custody::abandoned_custody().count(METADATA),
            before + 1
        );
    }

    #[test]
    fn settled_live_context_releases_normally() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let runtime = tempfile::tempdir().unwrap();
        let state = crate::test_support::TestState::open(
            runtime.path(),
            Arc::new(crate::refs::TrustStore::new()),
        )
        .unwrap();
        let original = state.live_native_owner_for_test();
        let aliases = Arc::strong_count(&original);
        let before = lillux::custody::abandoned_custody().count(CONTEXT);
        drop(LiveClosedOperationalContext::new(
            super::LiveClosedOperationalContextParts {
                schema_sources: Vec::new(),
                schema_native_failure: None,
                original: Arc::clone(&original),
                references:
                    super::super::live_native::LiveOperationalSchemaReferences::empty_for_test(),
                _initialization: None,
                initialization_retirement: None,
                _marker: None,
            },
            super::super::live_native::LiveOperationalInitializationPhase::Failed,
        ));
        assert_eq!(lillux::custody::abandoned_custody().count(CONTEXT), before);
        assert_eq!(
            Arc::strong_count(&original),
            aliases,
            "the parent alias was released"
        );
        drop(original);
        state.close().unwrap();
    }
}

#[cfg(test)]
mod outside_quote_tests {
    use super::{
        LiveOperationalSchemaSource, OperationalMarkerSource,
        quote_operational_initialization_retirement, quote_operational_live_metadata_retirement,
        quote_operational_main_packet, quote_operational_marker_owner,
        quote_operational_marker_receipt, quote_operational_packet_sources,
        quote_operational_schema_sources, require_retired_operational_metadata_registry,
    };
    use lillux_sqlite::original_physical_custody::{
        OriginalPhysicalRegistry, OutsideWork, RetiredOriginalPhysicalRegistry,
    };
    use lillux_sqlite::{
        OriginalNativeOutsideConnectionCustody, OriginalOutsideFinalizerFileRetirementAuthority,
        OriginalOutsideFinalizerFileRetirementInputs, RetainedOriginalFinalizerFile,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    struct MarkerRetirementAuthority {
        owner: Arc<RetainedOriginalFinalizerFile>,
        remaining: AtomicU64,
    }

    unsafe impl OriginalOutsideFinalizerFileRetirementAuthority for MarkerRetirementAuthority {
        fn require_original(
            &self,
            owner: &Arc<RetainedOriginalFinalizerFile>,
        ) -> anyhow::Result<()> {
            anyhow::ensure!(
                Arc::ptr_eq(owner, &self.owner),
                "marker test selected a foreign owner"
            );
            Ok(())
        }

        fn debit_original_steps(&self, steps: u64) -> anyhow::Result<()> {
            self.remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(steps)
                })
                .map(|_| ())
                .map_err(|_| anyhow::anyhow!("marker test authority exhausted"))
        }
    }

    fn register_test_control_record(registry: &mut OriginalPhysicalRegistry) {
        let owner = unsafe {
            RetainedOriginalFinalizerFile::issued(
                tempfile::tempfile().unwrap(),
                lillux_sqlite::OriginalFinalizerFileKind::ControlRecord,
            )
        };
        match unsafe { registry.register_control_record(owner, &|_, _| Ok(())) } {
            Ok(_) => {}
            Err(_) => panic!("test control record must register"),
        }
    }

    fn retire_test_registry(
        mut registry: OriginalPhysicalRegistry,
    ) -> RetiredOriginalPhysicalRegistry {
        unsafe { registry.seal_inventory() }.unwrap();
        unsafe { registry.prepare_descriptor_owners(&|_, _| Ok(())) }.unwrap();
        unsafe {
            registry
                .retire_non_operator_descriptors(&|_, _| Ok(()), &|_, owner| {
                    let authority = Arc::new(MarkerRetirementAuthority {
                        owner: Arc::clone(&owner),
                        remaining: AtomicU64::new(64),
                    });
                    let inputs =
                        OriginalOutsideFinalizerFileRetirementInputs::issued(owner, authority)?;
                    Ok(Arc::new(inputs))
                })
                .unwrap();
        }
        match registry.retire() {
            Ok(retired) => retired,
            Err(_) => panic!("fully retired test registry must produce a receipt"),
        }
    }

    #[test]
    fn operational_owner_quotes_cover_bounded_source_vectors() {
        let schema = quote_operational_schema_sources(0, 1).unwrap();
        assert_eq!(
            schema.bytes,
            (std::mem::size_of::<LiveOperationalSchemaSource>() + std::mem::size_of::<Vec<()>>())
                as u64
        );
        assert_eq!(schema.steps, 8);
        assert!(quote_operational_schema_sources(1, 1).is_err());

        let packets = quote_operational_packet_sources(1, 1).unwrap();
        assert_eq!(
            packets.bytes,
            (2 * std::mem::size_of::<OriginalNativeOutsideConnectionCustody>()
                + std::mem::size_of::<Vec<()>>()) as u64
        );
        assert_eq!(packets.steps, 12);
        assert!(quote_operational_packet_sources(2, 1).is_err());
        assert_eq!(quote_operational_main_packet(1).unwrap().steps, 8);
        assert!(quote_operational_main_packet(usize::MAX).is_err());

        let owner = quote_operational_marker_owner().unwrap();
        assert_eq!(
            owner.bytes,
            (std::mem::size_of::<RetainedOriginalFinalizerFile>()
                + 2 * std::mem::size_of::<usize>()) as u64
        );
        assert_eq!(owner.steps, 4);
        assert_eq!(quote_operational_marker_receipt().steps, 16);
    }

    #[test]
    fn live_metadata_retirement_quote_covers_fixed_schema_and_initialization_owners() {
        assert_eq!(
            quote_operational_live_metadata_retirement(0, 0, false)
                .unwrap()
                .steps,
            12
        );
        assert_eq!(
            quote_operational_live_metadata_retirement(1, 1, true)
                .unwrap()
                .steps,
            20
        );
        assert_eq!(
            quote_operational_live_metadata_retirement(1, 1, true)
                .unwrap()
                .bytes,
            0
        );
        assert!(quote_operational_live_metadata_retirement(2, 0, false).is_err());
        assert_eq!(
            quote_operational_initialization_retirement().unwrap(),
            OutsideWork { bytes: 0, steps: 8 }
        );
    }

    #[test]
    fn retired_live_metadata_retry_requires_the_original_registry() {
        let mut original = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        register_test_control_record(&mut original);
        let original_identity = original.identity().unwrap();
        let retired = retire_test_registry(original);
        require_retired_operational_metadata_registry(Some(&original_identity), &retired).unwrap();

        let mut foreign = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        register_test_control_record(&mut foreign);
        let foreign = retire_test_registry(foreign);
        assert!(
            require_retired_operational_metadata_registry(Some(&original_identity), &foreign,)
                .is_err()
        );
        assert!(require_retired_operational_metadata_registry(None, &retired).is_err());
    }

    #[test]
    fn live_marker_uses_same_registry_and_exact_retired_close_receipt() {
        // The live marker is a metadata-only pin, exactly as Operational
        // startup captures it.
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("marker"), b"m").unwrap();
        let pinned = lillux::PinnedDirectory::open(directory.path())
            .unwrap()
            .unwrap();
        let mut marker = Some(
            pinned
                .open_mount_entry(std::ffi::OsStr::new("marker"))
                .unwrap()
                .unwrap(),
        );
        let mut source = OperationalMarkerSource::new();
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();

        assert!(
            source
                .register(
                    Some(&mut marker),
                    &mut registry,
                    &|_, _| anyhow::bail!("funding unavailable"),
                    false
                )
                .is_err()
        );
        assert!(
            marker.is_some(),
            "prepay refusal must preserve the original File"
        );

        source
            .register(Some(&mut marker), &mut registry, &|_, _| Ok(()), false)
            .unwrap();
        assert!(
            marker.is_none(),
            "accepted marker must move into its retained owner"
        );
        source
            .require_registered(Some(&marker), &registry, false)
            .unwrap();

        let mut foreign = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        register_test_control_record(&mut foreign);
        let foreign = retire_test_registry(foreign);
        assert!(
            source
                .consume_retired(Some(&mut marker), &foreign, &|_, _| Ok(()), false)
                .is_err()
        );

        let retired = retire_test_registry(registry);
        source
            .consume_retired(Some(&mut marker), &retired, &|_, _| Ok(()), false)
            .unwrap();
        source
            .consume_retired(Some(&mut marker), &retired, &|_, _| Ok(()), false)
            .unwrap();
        assert!(source.retired_or_absent(false));
    }

    #[test]
    fn only_failed_start_may_retain_typed_marker_absence() {
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        let mut source = OperationalMarkerSource::new();
        assert!(
            source
                .register(None, &mut registry, &|_, _| Ok(()), false)
                .is_err()
        );

        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        let mut source = OperationalMarkerSource::new();
        source
            .register(None, &mut registry, &|_, _| Ok(()), true)
            .unwrap();
        source.require_registered(None, &registry, true).unwrap();
        register_test_control_record(&mut registry);
        let retired = retire_test_registry(registry);
        source
            .consume_retired(None, &retired, &|_, _| Ok(()), true)
            .unwrap();
        assert!(source.retired_or_absent(true));
        assert!(!source.retired_or_absent(false));
    }
}

impl OutsideOriginalOperationalCustody {
    /// Final post-release census: descriptors, roots, marker and live
    /// metadata ended under the exact retired registry. Reads only cached
    /// receipts and identities.
    pub(crate) fn require_exhausted(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_exhausted_holders()?;
        registry.require_same_registry(
            self.inner
                .shared_registry_identity
                .as_ref()
                .context("Operational registry identity absent at final exhaustion")?,
        )
    }
    /// The registry-free part of final exhaustion. A dropped frame settles on
    /// exactly these checks.
    pub(crate) fn require_exhausted_holders(&self) -> Result<()> {
        self.require_process()?;
        self.inner.require_exhausted_holders()
    }
}
#[cfg(test)]
mod outside_custody_drop_tests {
    use super::{
        OperationalMarkerSource, OutsideOperationalParts, OutsideOriginalOperationalCustody,
    };

    const KIND: &str = "ryeos-state::outside-operational-custody";

    fn frame(shared_roots_consumed: bool) -> OutsideOriginalOperationalCustody {
        OutsideOriginalOperationalCustody::from_parts(OutsideOperationalParts {
            live: None,
            live_metadata: None,
            root: None,
            namespace: None,
            terminal: None,
            native_failure: None,
            sql_custody: Vec::new(),
            shared_registrations: std::array::from_fn(|_| None),
            shared_registry_identity: None,
            marker_source: OperationalMarkerSource::consumed_for_test(true),
            descriptor_owners: Vec::new(),
            descriptor_receipts: Vec::new(),
            prior_refusal: None,
            live_domain: true,
            retired_shared_roots_consumed: shared_roots_consumed,
        })
    }

    #[test]
    fn dropped_exhausted_frame_releases_normally() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let frame = frame(true);
        frame.require_exhausted_holders().unwrap();
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(frame);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn dropped_frame_with_unconsumed_roots_is_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        // Shared roots never registered and never consumed: the frame still
        // owes its root receipts, so it is held whole.
        let frame = frame(false);
        assert!(frame.require_exhausted_holders().is_err());
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(frame);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
    }
}

#[cfg(test)]
mod close_refusal_tests {
    use super::{close_pending_refusal, close_refusal_over_prior};
    use lillux_sqlite::original_host_owner::FixedHostRefusal;
    use lillux_sqlite::{NativeCheckContended, native_refusal_is_fact};

    fn fact() -> anyhow::Error {
        anyhow::anyhow!("Operational close changed original purpose/factory")
    }
    fn contended() -> anyhow::Error {
        anyhow::Error::new(FixedHostRefusal::Contended).context("producer control claim")
    }

    /// A later close refusal is classified by itself over an earlier one.
    #[test]
    fn a_later_close_refusal_classifies_over_the_earlier_one() {
        let later_fact = close_refusal_over_prior(fact(), Some(contended()));
        assert!(native_refusal_is_fact(&later_fact), "{later_fact:#}");
        assert!(format!("{later_fact:#}").contains("producer control claim"));
        let later_contention =
            close_refusal_over_prior(anyhow::Error::new(NativeCheckContended), Some(fact()));
        assert!(
            !native_refusal_is_fact(&later_contention),
            "{later_contention:#}"
        );
        assert!(native_refusal_is_fact(&close_refusal_over_prior(
            fact(),
            None
        )));
    }

    /// A pending close keeps its retained cause's text and class.
    #[test]
    fn a_pending_close_keeps_the_retained_cause() {
        let held_by_contention =
            close_pending_refusal("Operational native close retry pending", Some(&contended()));
        assert!(
            !native_refusal_is_fact(&held_by_contention),
            "{held_by_contention:#}"
        );
        assert!(format!("{held_by_contention:#}").contains("producer control claim"));
        let held_by_fact =
            close_pending_refusal("Operational native close retry pending", Some(&fact()));
        assert!(native_refusal_is_fact(&held_by_fact), "{held_by_fact:#}");
        assert!(format!("{held_by_fact:#}").contains("changed original purpose/factory"));

        // A failed retry held by contention, over an earlier fact, does not
        // latch; a failed retry held by a fact, over earlier contention, does.
        let retry_contended = close_refusal_over_prior(
            close_pending_refusal("Operational native close retry pending", Some(&contended())),
            Some(fact()),
        );
        assert!(
            !native_refusal_is_fact(&retry_contended),
            "{retry_contended:#}"
        );
        let retry_fact = close_refusal_over_prior(
            close_pending_refusal("Operational native close retry pending", Some(&fact())),
            Some(contended()),
        );
        assert!(native_refusal_is_fact(&retry_fact), "{retry_fact:#}");
    }
}
