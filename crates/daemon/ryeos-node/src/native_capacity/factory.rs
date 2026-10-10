//! Original live factory ownership and producer intervals. This owns native
//! admission; schema, destination authority and supervisor drain are callers.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{Context, Result, ensure};
use lillux::PinnedDirectory;
use lillux_sqlite::{
    OriginalNativeAdmissionAuthority, OriginalNativeBudgetOwner, OriginalNativeConnectionAdmission,
    OriginalNativeFactory, OriginalNativeProjectionLeaseAdmission,
    OriginalNativeProjectionLeaseAuthority,
    SqliteInspectionProductionControl, SqliteInspectionProductionKind,
    SqliteInspectionProductionPermit, SqliteInspectionProductionPublicationFailure,
};

use super::LiveNativeCapacityReservation;
use lillux_sqlite::original_host_owner::OriginalNativeRegionHandle;

const MAX_TICKETS: usize = 64;
const MAX_PREPARED_CONTROLS: usize = 256;

struct LiveNativeAuthority {
    original: Weak<LiveNativeAuthority>,
    reservation: Arc<LiveNativeCapacityReservation>,
    budget: Arc<OriginalNativeBudgetOwner>,
    region: OriginalNativeRegionHandle,
    issuer: OnceLock<lillux_sqlite::OriginalNativeProducerCellIndex>,
    abandoned: AtomicU8,
    admission_closed: AtomicBool,
    admission_gate: Mutex<()>,
}

/// Read-only partial-custody observation; never terminal or release authority.
#[derive(Debug, Clone, Copy)]
pub struct LiveProducerControlCensus {
    pub prepared_attempts: usize,
    pub registered_controls: usize,
    pub unbound_controls: usize,
    pub retired_permit_references: usize,
    pub unresolved_preparations: usize,
}

// Ordinary callback wrapper under the enclosing node cap. The permit itself
// holds the actual counted resident producer cell; no registry/backedge owns
// this wrapper inside the mapped region.
struct LiveProductionControl { authority: Arc<LiveNativeAuthority> }
impl SqliteInspectionProductionControl for LiveProductionControl {
    fn checkpoint(&self) -> Result<()> { self.authority.require_original(false) }
    fn complete(&self) -> Result<()> { self.authority.require_original(true) }
    fn abandoned(&self) {
        if self.authority.reservation.inputs.require_original_process().is_ok() {
            self.authority.abandoned.store(1, Ordering::Release);
        }
    }
}

impl OriginalNativeAdmissionAuthority for LiveNativeAuthority {
    fn require_original(&self, cleanup: bool) -> Result<()> {
        self.reservation.require_original(cleanup)?;
        if !cleanup {
            ensure!(
                self.abandoned.load(Ordering::Acquire) == 0,
                "live native custody unsettled"
            );
        }
        Ok(())
    }

    fn require_runtime_binding(&self, runtime: &PinnedDirectory) -> Result<()> {
        self.require_original(false)?;
        ensure!(
            self.reservation.inputs.runtime.is_same_directory(runtime)?,
            "live native factory selects foreign runtime"
        );
        runtime.ensure_path_binding()
    }

    fn budget(&self) -> Arc<OriginalNativeBudgetOwner> {
        Arc::clone(&self.budget)
    }

    fn debit_sql_vm_steps(&self, steps: u64) -> Result<()> {
        // SQLite invokes this inside its lexical SQL scope. No production
        // mutex is held across native work, and VM work consumes the same
        // prepaid service counter as allocator work.
        self.require_original(false)?;
        self.budget.debit_steps(steps, false)
    }

    fn begin_connection(&self) -> Result<OriginalNativeConnectionAdmission> {
        let (ticket, permit) = self.begin_ticket(SqliteInspectionProductionKind::Native)?;
        // SAFETY: one original interval and the same irreversibly debited budget.
        unsafe {
            OriginalNativeConnectionAdmission::issued(Arc::clone(&self.budget), ticket, permit)
                .map_err(anyhow::Error::new)
        }
    }

    fn begin_projection_lease(&self) -> Result<OriginalNativeProjectionLeaseAdmission> {
        let (ticket, permit) = self.begin_ticket(SqliteInspectionProductionKind::ProjectionLease)?;
        // SAFETY: a distinct lease interval, issued from the SAME nonrecycled
        // ticket counter; no SQL producer can acknowledge this lease permit.
        unsafe {
            OriginalNativeProjectionLeaseAdmission::issued(Arc::clone(&self.budget), ticket, permit)
                .map_err(anyhow::Error::new)
        }
    }

}

impl LiveNativeAuthority {
    /// Both native owner kinds consume the one original ticket namespace.
    /// The local start/close gate covers the finite claim and ticket binding;
    /// it is released before the returned permit can perform native work.
    fn begin_ticket(
        &self,
        kind: SqliteInspectionProductionKind,
    ) -> Result<(u32, SqliteInspectionProductionPermit)> {
        self.require_original(false)?;
        let _gate = self.admission_gate.try_lock()
            .map_err(|_| anyhow::anyhow!("original live producer admission busy"))?;
        ensure!(!self.admission_closed.load(Ordering::Acquire), "live native admission closed");
        let original = self
            .original
            .upgrade()
            .context("original live authority no longer retained")?;
        // The nonrecycled cell is in the SAME resident graph. A refusal after
        // claiming it remains visible to that region's terminal census.
        let core = self.region.claim_producer_control(kind)?;
        let ticket = match self.budget.debit_connection_ticket() {
            Ok(ticket) => ticket,
            Err(error) => return Err(anyhow::Error::new(LiveNativeProducerPreparationFailure {
                original: Arc::clone(&self.reservation), core, ticket: None, error,
            })),
        };
        if let Err(error) = core.bind_ticket(ticket) {
            return Err(anyhow::Error::new(LiveNativeProducerPreparationFailure {
                original: Arc::clone(&self.reservation), core, ticket: Some(ticket), error: error.into(),
            }));
        }
        let control: Arc<dyn SqliteInspectionProductionControl> =
            Arc::new(LiveProductionControl { authority: original });
        // SAFETY: the real original reservation and resident producer cell
        // share this sole counter; no copied index or ordinary Arc grants it.
        let permit = unsafe { SqliteInspectionProductionPermit::issued_from_region(kind, control, core) }
            .map_err(|retained| anyhow::Error::new(LiveNativeProducerPublicationFailure {
                original: Arc::clone(&self.reservation), retained,
            }))?;
        Ok((ticket, permit))
    }
}

/// Keeps the actual preparation and any irreversibly issued ticket. No
/// failed binding is inferred to have refunded or retired that ticket.
pub struct LiveNativeProducerPreparationFailure {
    original: Arc<LiveNativeCapacityReservation>,
    core: lillux_sqlite::OriginalNativeProducerControl,
    ticket: Option<u32>,
    error: anyhow::Error,
}
impl LiveNativeProducerPreparationFailure {
    /// Only a failed debit (no ticket issued) can retire as a never-published
    /// preparation. The actual lower cell checks its own unbound state.
    pub fn retire_unbound(self) -> std::result::Result<anyhow::Error, Self> {
        if self.original.inputs.require_original_process().is_err() || self.ticket.is_some() {
            return Err(self);
        }
        if self.core.retire_unbound_preparation().is_err() { return Err(self); }
        Ok(self.error)
    }
    /// Recover an issued but never-published ticket through the SAME actual
    /// factory and its C no-acquisition transition. The returned Released
    /// receipt joins the ordinary exhaustive native terminal census.
    pub fn retire_bound(self, factory: &Arc<LiveNativeFactory>)
        -> std::result::Result<(anyhow::Error, lillux_sqlite::OriginalNativeConnectionTerminal), Self> {
        if self.ticket.is_none() || !Arc::ptr_eq(&self.original, &factory.authority.reservation)
            || !self.core.observation().is_ok_and(|observation| Some(observation.ticket) == self.ticket) {
            return Err(self);
        }
        let Some(native) = factory.factory.get() else { return Err(self); };
        let released = match native.retire_unpublished_ticket(&self.core) {
            Ok(released) => released,
            Err(_) => return Err(self),
        };
        Ok((self.error, lillux_sqlite::OriginalNativeConnectionTerminal::Released(released)))
    }

    pub fn into_parts(self) -> std::result::Result<(Arc<LiveNativeCapacityReservation>, lillux_sqlite::OriginalNativeProducerControl, Option<u32>, anyhow::Error), Self> {
        if self.original.inputs.require_original_process().is_err() { return Err(self); }
        Ok((self.original, self.core, self.ticket, self.error))
    }
}
impl std::fmt::Debug for LiveNativeProducerPreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveNativeProducerPreparationFailure").field("ticket", &self.ticket).field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveNativeProducerPreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original producer preparation retained: {}", self.error)
    }
}
impl std::error::Error for LiveNativeProducerPreparationFailure {}

/// Actual refused publication custody, accessible to the outside supervisor.
/// The lower failure retains its original cell and callback; no failed attempt
/// is silently dropped or converted into a replacement admission.
pub struct LiveNativeProducerPublicationFailure {
    original: Arc<LiveNativeCapacityReservation>,
    retained: SqliteInspectionProductionPublicationFailure,
}
impl LiveNativeProducerPublicationFailure {
    /// Retire only the actual uninstalled issuer. All fallible operations
    /// borrow its original custody before the final consuming transfer.
    fn retire_uninstalled(self) -> std::result::Result<(), Self> {
        if self.original.close_uninstalled_construction().is_err()
            || self.retained.retire_uninstalled_factory_issuer().is_err() {
            return Err(self);
        }
        let Self { original, retained } = self;
        match retained.into_parts() {
            Ok((_kind, callback, core)) => {
                drop(core);
                drop(callback);
                drop(original);
                Ok(())
            }
            Err(retained) => Err(Self { original, retained }),
        }
    }
    pub fn into_parts(self) -> std::result::Result<(Arc<LiveNativeCapacityReservation>, SqliteInspectionProductionPublicationFailure), Self> {
        if self.original.inputs.require_original_process().is_err() { return Err(self); }
        Ok((self.original, self.retained))
    }
}
impl std::fmt::Debug for LiveNativeProducerPublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveNativeProducerPublicationFailure").field("refusal", &self.retained.refusal).finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveNativeProducerPublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original producer publication retained: {}", self.retained.refusal)
    }
}
impl std::error::Error for LiveNativeProducerPublicationFailure {}

// Issued only by the actual retained live factory after its irreversible
// producer gate closes. The lower backend separately requires every genuine
// connection/lease terminal; this is not a caller-provided drain flag.
struct LiveCleanupAuthority { original: Arc<LiveNativeFactory> }
impl lillux_sqlite::OriginalNativeCleanupAuthority for LiveCleanupAuthority {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        let authority = &self.original.authority;
        authority.require_original(true)?;
        ensure!(authority.admission_closed.load(Ordering::Acquire), "live producer gate is not sealed");
        let _gate = authority.admission_gate.try_lock()
            .map_err(|_| anyhow::anyhow!("live producer start has not quiesced"))?;
        let original = self.original.factory.get().context("original live factory absent")?;
        ensure!(Arc::ptr_eq(original, factory), "live cleanup selects foreign native factory");
        let issuer = authority.issuer.get().context("actual live issuer cell absent")?;
        authority.region.require_quiescent_nonissuer_producers(*issuer)?;
        Ok(())
    }
}

pub struct LiveNativeFactory {
    authority: Arc<LiveNativeAuthority>,
    factory: OnceLock<Arc<OriginalNativeFactory>>,
}

pub struct LiveNativeFactoryFailure {
    _original: Arc<LiveNativeFactory>,
    error: anyhow::Error,
}
impl LiveNativeFactoryFailure {
    /// Recover an actual publication refusal before a lower factory exists.
    /// No replacement factory, budget or backend terminal is issued.
    pub fn retire_uninstalled(self) -> std::result::Result<(), Self> {
        if self._original.authority.reservation.inputs.require_original_process().is_err()
            || self._original.factory.get().is_some()
            || self._original.close_admission().is_err() { return Err(self); }
        let Self { _original, error } = self;
        match error.downcast::<LiveNativeProducerPublicationFailure>() {
            Ok(failure) => match failure.retire_uninstalled() {
                Ok(()) => { drop(_original); Ok(()) }
                Err(failure) => Err(Self { _original, error: anyhow::Error::new(failure) }),
            },
            Err(error) => {
                // Earlier input/claim refusal can precede issuer creation; a
                // failed handle transfer can instead leave the actual issuer
                // abandoned before any lower factory was installed. Close the
                // real admission/region first, never guess a native terminal.
                let retired = (|| -> Result<()> {
                    _original.close_admission()?;
                    _original.authority.reservation.close_uninstalled_construction()?;
                    if let Some(index) = _original.authority.issuer.get() {
                        _original.authority.region.retire_uninstalled_factory_issuer(*index)?;
                    } else {
                        ensure!(_original.authority.region.producer_control_census()?.allocated == 0,
                            "uninstalled construction has an unowned producer cell");
                    }
                    Ok(())
                })();
                if retired.is_err() { return Err(Self { _original, error }); }
                drop(error);
                drop(_original);
                Ok(())
            }
        }
    }
    /// Return actual custody to the outside supervisor, preserving the same
    /// failed factory, reservation and resident graph. No retry grant is issued.
    pub fn into_original(self) -> std::result::Result<Arc<LiveNativeFactory>, Self> {
        if self._original.authority.reservation.inputs.require_original_process().is_err() {
            return Err(self);
        }
        Ok(self._original)
    }

    pub fn original(&self) -> &Arc<LiveNativeFactory> {
        &self._original
    }
}
impl std::fmt::Debug for LiveNativeFactoryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveNativeFactoryFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveNativeFactoryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original live native factory retained: {}", self.error)
    }
}
impl std::error::Error for LiveNativeFactoryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl LiveNativeFactory {
    /// Pure quote for the complete fixed native graph: counter, factory state,
    /// nonrecycled producer cells and ticket receipts. Ordinary wrappers live
    /// under the separately enforced node process envelope.
    pub(super) fn minimum_host_payload_bytes(tickets: u32) -> Result<u64> {
        ensure!(tickets > 0 && tickets as usize <= MAX_TICKETS,
            "host payload quote has unsupported ticket count");
        let layout = lillux_sqlite::original_host_owner::original_native_region_layout()?;
        Ok(u64::try_from(layout.layout.size())?)
    }
    /// The single concrete resident graph initialization. Mapping/page work
    /// is quoted separately by the actual host owner.
    pub(super) fn minimum_host_placement_steps() -> u64 { 1 }
    /// Retain the host before any factory/bootstrap effect. Installation uses
    /// one original issuer interval and never exposes a replacement factory.
    pub fn install(reservation: Arc<LiveNativeCapacityReservation>) -> Result<Arc<Self>> {
        // Claim the original one-use attempt before any host placement or
        // wrapper allocation. A failed preparation cannot authorize retry on
        // the same paid reservation or consume unquoted duplicate state.
        reservation.claim_factory_install()?;
        let budget = reservation.budget()?;
        let region = reservation.region_handle()?;
        let authority = Arc::new_cyclic(|original| LiveNativeAuthority {
            original: original.clone(), reservation, budget, region, issuer: OnceLock::new(),
            abandoned: AtomicU8::new(0), admission_closed: AtomicBool::new(false),
            admission_gate: Mutex::new(()),
        });
        let original = Arc::new(Self {
            authority,
            factory: OnceLock::new(),
        });
        let result = (|| -> Result<()> {
            original.authority.require_original(false)?;
            let core = original.authority.region.claim_producer_control(
                SqliteInspectionProductionKind::FactoryIssuer)?;
            original.authority.issuer.set(core.index())
                .map_err(|_| anyhow::anyhow!("actual live issuer index already retained"))?;
            let control: Arc<dyn SqliteInspectionProductionControl> = Arc::new(LiveProductionControl {
                authority: Arc::clone(&original.authority),
            });
            // SAFETY: this one-use issuer owns a counted cell in the actual
            // original resident graph, authenticated by the retained account.
            let permit = unsafe { SqliteInspectionProductionPermit::issued_from_region(
                SqliteInspectionProductionKind::FactoryIssuer, control, core,
            ) }.map_err(|retained| anyhow::Error::new(LiveNativeProducerPublicationFailure {
                original: Arc::clone(&original.authority.reservation), retained,
            }))?;
            let host: Arc<dyn OriginalNativeAdmissionAuthority> = original.authority.clone();
            // SAFETY: authentic retained original reservation, same prepaid
            // counter and one active issuer interval above, before any effect.
            let factory = match unsafe {
                OriginalNativeFactory::install_from_original_issuer(original.authority.region.try_clone()?, host, permit)
            } {
                Ok(factory) => factory,
                Err(error) => {
                    original
                        .factory
                        .set(Arc::clone(error.factory()))
                        .map_err(|_| anyhow::anyhow!("original failed factory cache conflict"))?;
                    return Err(anyhow::Error::new(error));
                }
            };
            original
                .factory
                .set(factory)
                .map_err(|_| anyhow::anyhow!("original factory cache conflict"))?;
            original.authority.require_original(false)?;
            Ok(())
        })();
        result.map_err(|error| {
            anyhow::Error::new(LiveNativeFactoryFailure {
                _original: Arc::clone(&original),
                error,
            })
        })?;
        Ok(original)
    }

    /// Stop new native tickets without acknowledging any existing owner.
    /// Closing local admission is permitted even after the original time window
    /// expires; the actual original process remains required.
    pub fn close_admission(&self) -> Result<()> {
        self.authority.reservation.inputs.require_original_process()?;
        self.authority.admission_closed.store(true, Ordering::Release);
        let _gate = self.authority.admission_gate.try_lock()
            .map_err(|_| anyhow::anyhow!("native admission close busy; gate remains closed"))?;
        Ok(())
    }

    pub fn producer_control_census(&self) -> Result<LiveProducerControlCensus> {
        self.authority.require_original(true)?;
        self.authority.budget.debit_steps((MAX_PREPARED_CONTROLS + 1) as u64, true)?;
        let resident = self.authority.region.producer_control_census()?;
        let census = LiveProducerControlCensus {
            prepared_attempts: resident.allocated,
            registered_controls: resident.allocated - resident.unpublished,
            unbound_controls: resident.unbound_nonissuer,
            retired_permit_references: resident.allocated - resident.live_references,
            unresolved_preparations: resident.active + resident.abandoned + resident.unpublished,
        };
        self.authority.require_original(true)?;
        Ok(census)
    }

    /// Finish the native component using exhaustive original SQL and lease
    /// receipts. This preserves the same factory on any refused close and
    /// does not release the mapping, namespace, reservation or durable account.
    pub fn finish_live_native(
        self: &Arc<Self>,
        terminals: &[lillux_sqlite::OriginalNativeConnectionTerminal],
        leases: &[Arc<lillux_sqlite::ClosedOriginalProjectionLease>],
    ) -> Result<Arc<lillux_sqlite::OriginalNativeTerminal>> {
        let result = (|| -> Result<_> {
            self.close_admission()?;
            let factory = self.factory.get().context("original live factory absent")?;
            let authority: Arc<dyn lillux_sqlite::OriginalNativeCleanupAuthority> =
                Arc::new(LiveCleanupAuthority { original: Arc::clone(self) });
            // SAFETY: the actual producer gate is irreversibly closed and
            // start has quiesced; resident producer state and every native
            // terminal are rechecked by these original owners at each effect.
            let inputs = unsafe { lillux_sqlite::OriginalLiveNativeDrainInputs::issued(
                Arc::clone(factory), authority,
            ) }?;
            factory.finish_live_backend(terminals, leases, &inputs).map_err(anyhow::Error::new)
        })();
        result.map_err(|error| anyhow::Error::new(LiveNativeFactoryFailure {
            _original: Arc::clone(self), error,
        }))
    }

    /// Retire the retained budget facade's counted resident handle only after
    /// the same factory has acknowledged the complete native terminal. This
    /// does not release the region, settle the account, or replace the
    /// reservation; the consuming outside finalizer still owns those stages.
    pub fn retire_native_budget_handle(
        &self,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<lillux_sqlite::OriginalNativeBudgetSnapshot> {
        self.authority.require_original(true)?;
        let factory = self.factory.get().context("original live factory absent")?;
        terminal.require_original(factory)?;
        self.authority
            .budget
            .retire_original_handle(factory, terminal)
    }

    /// Close a genuine failed initialization before any Ready handoff or
    /// resource ticket. The lower owner checks those facts; this never grants
    /// Ready admission or constructs replacement native state.
    pub fn finish_initialization_refusal(self: &Arc<Self>) -> Result<Arc<lillux_sqlite::OriginalNativeTerminal>> {
        let result = (|| -> Result<_> {
            self.close_admission()?;
            self.authority.require_original(true)?;
            let factory = self.factory.get().context("original failed native factory absent")?;
            let authority = LiveCleanupAuthority { original: Arc::clone(self) };
            lillux_sqlite::OriginalNativeCleanupAuthority::require_original(&authority, factory)?;
            factory.finish_original_initialization_refusal().map_err(anyhow::Error::new)
        })();
        result.map_err(|error| anyhow::Error::new(LiveNativeFactoryFailure {
            _original: Arc::clone(self), error,
        }))
    }

    pub fn initialize(self: &Arc<Self>) -> Result<()> {
        let result = self
            .factory
            .get()
            .context("original live factory absent")?
            .initialize_original_backend()
            .map_err(anyhow::Error::new);
        result.map_err(|error| {
            anyhow::Error::new(LiveNativeFactoryFailure {
                _original: Arc::clone(self),
                error,
            })
        })
    }

    /// Prepay concrete outside receiver storage from the original reservation
    /// only after genuine same-factory aggregate acknowledgement. No Ready or
    /// service opening is re-entered and no replacement allowance is issued.
    pub fn prepay_outside_control(&self, terminal: &lillux_sqlite::OriginalNativeTerminal,
        memory_bytes: u64, steps: u64) -> Result<()> {
        self.authority.require_original(true)?;
        let factory = self.factory.get().context("original live factory absent")?;
        terminal.require_original(factory)?;
        self.authority.reservation.prepay_outside_control(memory_bytes, steps)
    }

    /// Issue metadata-pin retirement inputs for the exact pins still held by
    /// one live packet. The same retained reservation funds every attempt;
    /// this does not create a new cleanup allowance or accept copied pins.
    pub fn packet_metadata_pin_retirement_inputs(
        &self,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        pins: &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
    ) -> Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>> {
        self.authority.require_original(true)?;
        let factory = self.factory.get().context("original live factory absent")?;
        terminal.require_original(factory)?;
        leaf.require_original(factory, terminal)?;
        self.authority.reservation.packet_metadata_pin_retirement_inputs(
            Arc::clone(factory), Arc::clone(terminal), leaf, pins,
        )
    }

    /// Transfer the same registry allocated by this genuine reservation before
    /// mapping. The consuming supervisor must cache it before any further
    /// fallible step; a second registry is never allocated for shutdown.
    pub fn take_prepared_physical_registry(&self, terminal: &lillux_sqlite::OriginalNativeTerminal)
        -> Result<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry> {
        self.authority.require_original(true)?;
        terminal.require_original(self.factory.get().context("original live factory absent")?)?;
        self.authority.reservation.take_prepared_physical_registry()
    }
    /// Retain the actual native owner for cleanup after service admission or
    /// its service window closes. This is not a Ready/service handoff and
    /// permits no new destination or ticket authority.
    pub fn retained_factory_for_cleanup(&self) -> Result<Arc<OriginalNativeFactory>> {
        self.authority.require_original(true)?;
        self.factory.get().cloned().context("original live factory absent")
    }

    /// Checked handoff of the SAME paid factory. Destination admission remains
    /// with the actual StateStore namespace owner; this grants no path/domain.
    pub fn ready_factory(self: &Arc<Self>) -> Result<Arc<OriginalNativeFactory>> {
        self.authority.require_original(false)?;
        let factory = self.factory.get().context("original live factory absent")?;
        factory.require_ready()?;
        Ok(Arc::clone(factory))
    }

    /// Original service/cleanup endpoint for this exact retained factory.
    /// A cleanup request never creates a new occupancy interval.
    pub fn original_projection_lease_deadline(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        cleanup: bool,
    ) -> Result<lillux::time::MonotonicDeadline> {
        self.authority.require_original(cleanup)?;
        let original = self.factory.get().context("original live factory absent")?;
        ensure!(Arc::ptr_eq(original, factory), "foreign live projection lease factory");
        let inputs = &self.authority.reservation.inputs;
        let deadline = if cleanup { inputs.cleanup_deadline } else { inputs.sql_deadline };
        ensure!(!deadline.has_elapsed(), "original live projection lease deadline elapsed");
        Ok(deadline)
    }

    /// Return the original captured service deadline, never a fresh interval.
    pub fn original_sql_deadline(&self) -> Result<lillux::time::MonotonicDeadline> {
        self.authority.require_original(false)?;
        let deadline = self.authority.reservation.inputs.sql_deadline;
        ensure!(
            !deadline.has_elapsed(),
            "original live SQL deadline elapsed"
        );
        Ok(deadline)
    }
}

impl OriginalNativeProjectionLeaseAuthority for LiveNativeFactory {
    fn require_original(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        cleanup: bool,
    ) -> Result<()> {
        // Revalidate the retained actual BOOTTIME observer for every lower
        // acquisition/cleanup check, including after fork waits and effects.
        self.authority.require_original(cleanup)?;
        let original = self.factory.get().context("original live factory absent")?;
        ensure!(Arc::ptr_eq(original, factory), "foreign live projection lease factory");
        Ok(())
    }

    fn fork_deadline(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        cleanup: bool,
    ) -> Result<lillux::time::MonotonicDeadline> {
        // Local wait cap only. The trait's require_original checks remain
        // mandatory around native effects; this cap cannot grant BOOTTIME.
        self.original_projection_lease_deadline(factory, cleanup)
    }
}
