//! Process-scoped native host for stopped-node consumers. One host installs
//! one real arena through the same budget, region, install and
//! initialization sequence every original issuer runs, admits connections
//! under the caller's authority, and tears the arena down through the
//! exhaustive terminal census and the resumable region release.
//!
//! Funding is the caller's non-forgeable [`OriginalNativeBudget`], minted
//! only by `prepaid_by_original_issuer` from an envelope reserved under the
//! caller's exclusive stopped-node authority. The arena is private anonymous
//! memory: nothing charged by this host outlives the process.
//!
//! Connections finish in-process. Every connection a consumer opens must be
//! closed or released and its terminal handed back with
//! [`OfflineNativeHost::retain_terminal`] before [`OfflineNativeHost::finish`];
//! the region seal refuses while any factory alias survives.
use crate::original_host_owner::backing::HostStorage;
use crate::original_host_owner::region::{NativeHostRegion, next_resident_generation};
use crate::original_host_owner::OriginalNativeRegionHandle;
use crate::{
    ClosedOriginalProjectionLease, OriginalDestinationCustodyFailure,
    OriginalDestinationOutsideCustody, OriginalLiveNativeDrainInputs,
    OriginalNativeAdmissionAuthority, OriginalNativeBudget, OriginalNativeBudgetOwner,
    OriginalNativeCleanupAuthority, OriginalNativeConnectionAdmission,
    OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority, OriginalNativeFactory,
    OriginalNativeFactoryFailure, OriginalNativeOutsideLeafEvidence,
    OriginalNativeProducerCellIndex, OriginalNativeProducerControl,
    OriginalNativeProjectionLeaseAdmission, OriginalNativeTerminal,
    SqliteInspectionProductionControl, SqliteInspectionProductionKind,
    SqliteInspectionProductionPermit, SqliteInspectionProductionPublicationFailure,
};
use anyhow::{Context, Result, ensure};
use lillux::PinnedDirectory;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

/// Cleanup steps reserved for the resumable region release attempts.
pub const OFFLINE_NATIVE_RELEASE_STEPS: u64 = 8;

/// The caller's stopped-node authority. Every admission, destination,
/// producer and cleanup boundary of the host revalidates it.
pub trait OfflineNativeAuthority: Send + Sync {
    /// `cleanup == false` is the operation window; `true` is the cleanup
    /// window, which must remain open after the operation window closes.
    fn checkpoint(&self, cleanup: bool) -> Result<()>;
}

/// Exact cleanup steps a raw budget must reserve for `storage` bytes and
/// `tickets` connections: the native teardown quote plus the release steps.
pub fn offline_native_required_cleanup_steps(storage: u64, tickets: u32) -> Result<u64> {
    OriginalNativeFactory::required_cleanup_steps(storage, tickets)?
        .checked_add(OFFLINE_NATIVE_RELEASE_STEPS)
        .context("offline native cleanup quote overflow")
}

/// Final ticket and work census of a released host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineNativeCensus {
    pub storage_bytes: u64,
    pub connections_issued: u32,
    pub terminals: usize,
    pub leases: usize,
    pub allocator_steps: u64,
    pub cleanup_steps: u64,
}

/// Sole owner of one process-scoped native arena. No Drop performs native
/// cleanup; [`Self::finish`] is the only teardown and it is resumable.
pub struct OfflineNativeHost {
    authority: Arc<dyn OfflineNativeAuthority>,
    region: NativeHostRegion,
    closed: Arc<AtomicBool>,
    admission: Option<Arc<OfflineAdmission>>,
    publication_failure: Option<SqliteInspectionProductionPublicationFailure>,
    factory: Option<Arc<OriginalNativeFactory>>,
    /// Every factory refusal, in order; none replaces an earlier owner.
    factory_failures: Vec<OriginalNativeFactoryFailure>,
    ready: bool,
    destination: Option<Arc<OfflineDestination>>,
    terminals: Mutex<Vec<OriginalNativeConnectionTerminal>>,
    /// Acquisitions refused before a ticket was debited: retained until the
    /// seal, but outside the ticket census.
    ticketless: Mutex<Vec<OriginalNativeConnectionTerminal>>,
    leases: Mutex<Vec<Arc<ClosedOriginalProjectionLease>>>,
    drained: Option<OriginalLiveNativeDrainInputs>,
    backend: Option<Arc<OriginalNativeTerminal>>,
    census: Option<OfflineNativeCensus>,
    released: bool,
    /// Funded storage of `raw`, reported identically on every finish path.
    storage_bytes: u64,
}

/// A refused issuance with the partially built host retained. Release it
/// with [`OfflineNativeHostFailure::release`]; the original refusal is
/// always returned, never converted into success.
pub struct OfflineNativeHostFailure {
    pub host: Option<OfflineNativeHost>,
    pub error: anyhow::Error,
}
impl std::fmt::Debug for OfflineNativeHostFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineNativeHostFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for OfflineNativeHostFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "offline native host issuance refused; partial host retained: {:#}", self.error)
    }
}
impl std::error::Error for OfflineNativeHostFailure {}
impl OfflineNativeHostFailure {
    /// Tear down the partial host. `Ok` carries the original refusal; `Err`
    /// keeps the partial host and the cleanup refusal for another attempt.
    pub fn release(mut self) -> std::result::Result<anyhow::Error, Self> {
        let Some(host) = self.host.as_mut() else { return Ok(self.error) };
        match host.finish() {
            Ok(_) => Ok(self.error),
            Err(cleanup) => {
                self.error = self.error.context(format!("partial host release retained: {cleanup:#}"));
                Err(self)
            }
        }
    }
}

impl OfflineNativeHost {
    /// Install one arena funded by `raw`. `raw` must reserve at least
    /// [`offline_native_required_cleanup_steps`] for its own storage and
    /// tickets. Every effect is preceded by `authority.checkpoint(false)`.
    ///
    /// # Safety
    /// The caller must hold exclusive stopped-node authority for the whole
    /// life of the host: no live daemon and no other maintainer may use the
    /// node's databases concurrently (in production, the node StateLock and
    /// the runtime namespace lock). `raw` must have been minted once, for
    /// this host, from an envelope reserved under that exclusion. `authority`
    /// must actually revalidate that exclusion, and the operation and cleanup
    /// windows, on every `checkpoint`; an authority that always succeeds is
    /// only sound where the exclusion is structural (a test holding the
    /// process-wide host guard on private directories).
    pub unsafe fn issue(
        raw: OriginalNativeBudget,
        authority: Arc<dyn OfflineNativeAuthority>,
    ) -> std::result::Result<Self, OfflineNativeHostFailure> {
        // The quote is pure; `raw` is an inert counter with no native effect
        // yet, so a quote refusal has nothing to release.
        let layout = HostStorage::quote_native_region()
            .map_err(|error| OfflineNativeHostFailure { host: None, error: anyhow::Error::msg(error) })?;
        let mut host = Self::new(authority, NativeHostRegion::new(layout));
        match host.install(raw) {
            Ok(()) => Ok(host),
            Err(error) => Err(OfflineNativeHostFailure { host: Some(host), error }),
        }
    }

    fn new(authority: Arc<dyn OfflineNativeAuthority>, region: NativeHostRegion) -> Self {
        Self {
            authority,
            region,
            closed: Arc::new(AtomicBool::new(false)),
            admission: None,
            publication_failure: None,
            factory: None,
            factory_failures: Vec::new(),
            ready: false,
            destination: None,
            terminals: Mutex::new(Vec::new()),
            ticketless: Mutex::new(Vec::new()),
            leases: Mutex::new(Vec::new()),
            drained: None,
            backend: None,
            census: None,
            released: false,
            storage_bytes: 0,
        }
    }

    fn install(&mut self, raw: OriginalNativeBudget) -> Result<()> {
        let snapshot = raw.snapshot();
        self.storage_bytes = snapshot.storage_bytes;
        let required = offline_native_required_cleanup_steps(snapshot.storage_bytes, raw.connections_limit())?;
        ensure!(
            snapshot.cleanup_reserved >= required,
            "offline native budget reserves {} cleanup steps; the arena requires {required}",
            snapshot.cleanup_reserved
        );
        let authority = self.authority.clone();
        self.region.enter(raw, OFFLINE_NATIVE_RELEASE_STEPS, || authority.checkpoint(false))?;
        self.region.map_resident(|| authority.checkpoint(false))?;
        let generation = next_resident_generation()?;
        // SAFETY: `issue`'s contract: `raw` was minted once for this host
        // under the caller's exclusive stopped-node authority, which
        // `authority` revalidates; the generation comes from the single
        // process-wide resident counter and is never reused.
        let budget = unsafe { self.region.initialize(generation) }?;
        let region = self.region.handle()?;
        let admission = Arc::new_cyclic(|original| OfflineAdmission {
            original: original.clone(),
            budget,
            region,
            issuer: OnceLock::new(),
            closed: self.closed.clone(),
            authority: self.authority.clone(),
        });
        self.admission = Some(admission.clone());
        self.authority.checkpoint(false)?;
        let core = admission.region.claim_producer_control(SqliteInspectionProductionKind::FactoryIssuer)?;
        admission
            .issuer
            .set(core.index())
            .map_err(|_| anyhow::anyhow!("offline issuer index already set"))?;
        // Every fallible step precedes the permit, so a refusal never drops it.
        let factory_region = admission.region.try_clone()?;
        let control = admission.production_control();
        // SAFETY: the issuer cell belongs to this host's own region.
        let permit = match unsafe {
            SqliteInspectionProductionPermit::issued_from_region(
                SqliteInspectionProductionKind::FactoryIssuer, control, core,
            )
        } {
            Ok(permit) => permit,
            Err(failure) => {
                let refusal = failure.refusal;
                self.publication_failure = Some(failure);
                anyhow::bail!("offline issuer publication refused: {refusal}");
            }
        };
        let issuer: Arc<dyn OriginalNativeAdmissionAuthority> = admission.clone();
        // SAFETY: this host is the sole issuer of its funded region; the
        // admission authority revalidates the caller before every ticket.
        let factory = match unsafe {
            OriginalNativeFactory::install_from_original_issuer(factory_region, issuer, permit)
        } {
            Ok(factory) => factory,
            Err(failure) => {
                let error = anyhow::anyhow!("offline factory install refused: {failure}");
                self.factory = Some(failure.factory().clone());
                self.factory_failures.push(failure);
                return Err(error);
            }
        };
        self.factory = Some(factory.clone());
        if let Err(failure) = factory.initialize_original_backend() {
            let error = anyhow::anyhow!("offline native backend initialization refused: {failure}");
            self.factory_failures.push(failure);
            return Err(error);
        }
        self.ready = true;
        self.destination = Some(Arc::new(OfflineDestination {
            factory: Arc::downgrade(&factory),
            authority: self.authority.clone(),
        }));
        Ok(())
    }

    pub fn factory(&self) -> Result<&Arc<OriginalNativeFactory>> {
        ensure!(self.ready && self.backend.is_none(), "offline native factory is not admitting");
        self.factory.as_ref().context("offline native factory absent")
    }

    /// Destination authority for opens issued under this host.
    pub fn destination(&self) -> Result<Arc<dyn OriginalNativeDestinationAuthority>> {
        ensure!(self.ready && self.backend.is_none(), "offline native factory is not admitting");
        let destination = self.destination.as_ref().context("offline native destination absent")?;
        Ok(destination.clone())
    }

    /// Record a terminal produced by a consumer of this host. A refusal
    /// hands the terminal back with the reason.
    pub fn retain_terminal(
        &self,
        terminal: OriginalNativeConnectionTerminal,
    ) -> std::result::Result<(), (OriginalNativeConnectionTerminal, anyhow::Error)> {
        let checked = self
            .factory
            .as_ref()
            .context("offline native factory absent")
            .and_then(|factory| terminal.require_original(factory));
        if let Err(error) = checked {
            return Err((terminal, error));
        }
        let slot = if terminal.ticket().is_some() { &self.terminals } else { &self.ticketless };
        slot.lock().unwrap_or_else(PoisonError::into_inner).push(terminal);
        Ok(())
    }

    /// Record a closed projection lease for the final census. A refusal
    /// hands the lease back with the reason.
    pub fn retain_lease(
        &self,
        lease: Arc<ClosedOriginalProjectionLease>,
    ) -> std::result::Result<(), (Arc<ClosedOriginalProjectionLease>, anyhow::Error)> {
        let checked = self
            .factory
            .as_ref()
            .context("offline native factory absent")
            .and_then(|factory| lease.require_original(factory));
        if let Err(error) = checked {
            return Err((lease, error));
        }
        self.leases.lock().unwrap_or_else(PoisonError::into_inner).push(lease);
        Ok(())
    }

    /// Issued ticket count and recorded terminals plus leases.
    pub fn ticket_census(&self) -> Result<(u32, usize)> {
        let factory = self.factory.as_ref().context("offline native factory absent")?;
        let issued = factory.budget_snapshot()?.connections_issued;
        let recorded = self.terminals.lock().unwrap_or_else(PoisonError::into_inner).len()
            + self.leases.lock().unwrap_or_else(PoisonError::into_inner).len();
        Ok((issued, recorded))
    }

    /// Close admission, drain and finish the backend against the exhaustive
    /// terminal census, retire the budget handle, drop every factory alias
    /// this host owns, then seal, dispose and release the region. Each stage
    /// is retained before the next; a refusal can be retried.
    pub fn finish(&mut self) -> Result<OfflineNativeCensus> {
        if self.released {
            return self.census.clone().context("released offline native host lacks its census");
        }
        self.closed.store(true, Ordering::Release);
        self.authority.checkpoint(true)?;
        if let Some(factory) = self.factory.clone() {
            if self.backend.is_none() {
                let terminals = self.terminals.lock().unwrap_or_else(PoisonError::into_inner).clone();
                let leases = self.leases.lock().unwrap_or_else(PoisonError::into_inner).clone();
                // The factory's own Ready acknowledgement selects the finish:
                // initialization can refuse after Ready was published.
                let finished = if factory.ready_acknowledged()? {
                    if self.drained.is_none() {
                        let admission = self.admission.clone().context("offline admission absent")?;
                        let cleanup: Arc<dyn OriginalNativeCleanupAuthority> = Arc::new(OfflineCleanup { admission });
                        // SAFETY: admission is closed above and never reopens;
                        // the cleanup authority rechecks producer quiescence
                        // and the caller's cleanup window before every effect.
                        self.drained = Some(unsafe { OriginalLiveNativeDrainInputs::issued(factory.clone(), cleanup) }?);
                    }
                    let drained = self.drained.as_ref().context("offline drain inputs absent")?;
                    factory.finish_live_backend(&terminals, &leases, drained)
                } else {
                    factory.finish_original_initialization_refusal()
                };
                match finished {
                    Ok(terminal) => self.backend = Some(terminal),
                    Err(failure) => {
                        let error = anyhow::anyhow!("offline native backend finish retained: {failure}");
                        self.factory_failures.push(failure);
                        return Err(error);
                    }
                }
            }
            if self.census.is_none() {
            let backend = self.backend.clone().context("offline native backend terminal absent")?;
            let admission = self.admission.clone().context("offline admission absent")?;
            let snapshot = admission.budget.retire_original_handle(&factory, &backend)?;
            let terminals = self.terminals.lock().unwrap_or_else(PoisonError::into_inner).len();
            let leases = self.leases.lock().unwrap_or_else(PoisonError::into_inner).len();
            self.census = Some(OfflineNativeCensus {
                storage_bytes: self.storage_bytes,
                connections_issued: snapshot.connections_issued,
                terminals,
                leases,
                allocator_steps: snapshot.allocator_steps,
                cleanup_steps: snapshot.cleanup_steps,
            });
            }
        }
        // Every alias this host owns ends here; consumers already handed
        // their terminals back. The seal below refuses any survivor.
        drop(self.backend.take());
        drop(self.drained.take());
        self.terminals.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.ticketless.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.leases.lock().unwrap_or_else(PoisonError::into_inner).clear();
        drop(self.destination.take());
        self.factory_failures.clear();
        drop(self.factory.take());
        drop(self.publication_failure.take());
        drop(self.admission.take());
        let authority = self.authority.clone();
        self.region.begin_close()?;
        if let Err(error) = self.region.release(|| authority.checkpoint(true)) {
            let census = self.region.census().map(|census| format!("{census:?}")).unwrap_or_default();
            return Err(error.context(format!("offline native region release retained; region {census}")));
        }
        self.region.require_released()?;
        let census = self.census.clone().unwrap_or(OfflineNativeCensus {
            storage_bytes: self.storage_bytes,
            connections_issued: 0,
            terminals: 0,
            leases: 0,
            allocator_steps: 0,
            cleanup_steps: 0,
        });
        self.census = Some(census.clone());
        self.released = true;
        Ok(census)
    }
}


struct OfflineAdmission {
    original: Weak<OfflineAdmission>,
    budget: Arc<OriginalNativeBudgetOwner>,
    region: OriginalNativeRegionHandle,
    issuer: OnceLock<OriginalNativeProducerCellIndex>,
    closed: Arc<AtomicBool>,
    authority: Arc<dyn OfflineNativeAuthority>,
}

impl OfflineAdmission {
    fn production_control(&self) -> Arc<dyn SqliteInspectionProductionControl> {
        Arc::new(OfflineProductionControl { closed: self.closed.clone(), authority: self.authority.clone() })
    }

    fn begin_ticket(&self, kind: SqliteInspectionProductionKind) -> Result<(u32, SqliteInspectionProductionPermit)> {
        self.require_original(false)?;
        ensure!(self.original.strong_count() > 0, "offline admission no longer retained");
        let core = self.region.claim_producer_control(kind)?;
        let ticket = match self.budget.debit_connection_ticket() {
            Ok(ticket) => ticket,
            Err(error) => return Err(retire_unbound(core, error)),
        };
        if let Err(refusal) = core.bind_ticket(ticket) {
            return Err(retire_unbound(core, refusal.into()));
        }
        // SAFETY: the bound cell and ticket come from this host's own region.
        let permit = unsafe { SqliteInspectionProductionPermit::issued_from_region(kind, self.production_control(), core) }
            .map_err(|failure| anyhow::Error::new(OfflinePublicationFailure {
                refusal: failure.refusal,
                _retained: failure,
            }))?;
        Ok((ticket, permit))
    }
}

impl OriginalNativeAdmissionAuthority for OfflineAdmission {
    fn require_original(&self, cleanup: bool) -> Result<()> {
        ensure!(cleanup || !self.closed.load(Ordering::Acquire), "offline native admission closed");
        self.authority.checkpoint(cleanup)
    }
    fn require_runtime_binding(&self, runtime: &PinnedDirectory) -> Result<()> {
        self.require_original(false)?;
        runtime.ensure_path_binding()
    }
    fn budget(&self) -> Arc<OriginalNativeBudgetOwner> {
        self.budget.clone()
    }
    fn debit_sql_vm_steps(&self, steps: u64) -> Result<()> {
        self.budget.debit_steps(steps, false)
    }
    fn begin_connection(&self) -> Result<OriginalNativeConnectionAdmission> {
        let (ticket, permit) = self.begin_ticket(SqliteInspectionProductionKind::Native)?;
        // SAFETY: ticket and interval come from this host's own budget.
        unsafe { OriginalNativeConnectionAdmission::issued(self.budget.clone(), ticket, permit) }
            .map_err(anyhow::Error::new)
    }
    fn begin_projection_lease(&self) -> Result<OriginalNativeProjectionLeaseAdmission> {
        let (ticket, permit) = self.begin_ticket(SqliteInspectionProductionKind::ProjectionLease)?;
        // SAFETY: ticket and interval come from this host's own budget.
        unsafe { OriginalNativeProjectionLeaseAdmission::issued(self.budget.clone(), ticket, permit) }
            .map_err(anyhow::Error::new)
    }
}

/// End a claimed producer cell whose ticket was never bound, so the region
/// seal does not wait on it. If the cell refuses retirement, it stays inside
/// the returned error.
fn retire_unbound(core: OriginalNativeProducerControl, error: anyhow::Error) -> anyhow::Error {
    match core.retire_unbound_preparation() {
        Ok(()) => error.context("unbound producer preparation retired"),
        Err(refusal) => anyhow::Error::new(OfflineProducerFailure {
            error: error.context(format!("unbound producer retirement refused: {refusal}")),
            _core: core,
        }),
    }
}

/// Retains the claimed producer cell of a refused ticket preparation.
struct OfflineProducerFailure {
    error: anyhow::Error,
    _core: OriginalNativeProducerControl,
}
impl std::fmt::Debug for OfflineProducerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineProducerFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for OfflineProducerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "offline producer preparation retained: {}", self.error)
    }
}
impl std::error::Error for OfflineProducerFailure {}

/// Retains a refused producer publication with its cell and callback.
struct OfflinePublicationFailure {
    refusal: crate::original_host_owner::FixedHostRefusal,
    _retained: SqliteInspectionProductionPublicationFailure,
}
impl std::fmt::Debug for OfflinePublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflinePublicationFailure").field("refusal", &self.refusal).finish_non_exhaustive()
    }
}
impl std::fmt::Display for OfflinePublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "offline producer publication retained: {}", self.refusal)
    }
}
impl std::error::Error for OfflinePublicationFailure {}

struct OfflineProductionControl {
    closed: Arc<AtomicBool>,
    authority: Arc<dyn OfflineNativeAuthority>,
}
impl SqliteInspectionProductionControl for OfflineProductionControl {
    fn checkpoint(&self) -> Result<()> {
        self.authority.checkpoint(self.closed.load(Ordering::Acquire))
    }
    fn complete(&self) -> Result<()> {
        Ok(())
    }
    fn abandoned(&self) {}
}

struct OfflineCleanup {
    admission: Arc<OfflineAdmission>,
}
impl OriginalNativeCleanupAuthority for OfflineCleanup {
    fn require_original(&self, _factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        ensure!(self.admission.closed.load(Ordering::Acquire), "offline producer admission still open");
        self.admission.authority.checkpoint(true)?;
        let issuer = self.admission.issuer.get().context("offline issuer cell absent")?;
        self.admission.region.require_quiescent_nonissuer_producers(*issuer)?;
        Ok(())
    }
}

struct OfflineDestination {
    factory: Weak<OriginalNativeFactory>,
    authority: Arc<dyn OfflineNativeAuthority>,
}
impl OriginalNativeDestinationAuthority for OfflineDestination {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        let original = self.factory.upgrade().context("offline destination factory gone")?;
        ensure!(Arc::ptr_eq(&original, factory), "offline destination selects foreign factory");
        // Close and cleanup also pass here, so require the cleanup window,
        // which spans the operation window and outlasts it.
        self.authority.checkpoint(true)
    }
    fn into_outside_custody(
        self: Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        _aggregate: &OriginalNativeTerminal,
        _leaf: &OriginalNativeOutsideLeafEvidence,
        _prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<OriginalDestinationOutsideCustody, OriginalDestinationCustodyFailure> {
        Err(OriginalDestinationCustodyFailure::retain(
            factory.clone(),
            self,
            anyhow::anyhow!("offline native host finishes its connections in-process; no outside custody receiver"),
        ))
    }
}
