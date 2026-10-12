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
//! [`OfflineNativeHost::retain_terminal`] before
//! [`OfflineNativeHost::finish_backend`]; [`OfflineNativeHost::release`]
//! refuses while any alias of the factory or backend terminal survives.
use crate::original_host_owner::backing::{HostStorage, HostStorageRegionReleaseFact};
use crate::original_host_owner::region::{NativeHostRegion, next_resident_generation};
use crate::original_host_owner::OriginalNativeRegionHandle;
use crate::{
    ClosedOriginalProjectionLease, OriginalDestinationCustodyFailure,
    OriginalDestinationOutsideCustody, OriginalLiveNativeDrainInputs,
    OriginalNativeAdmissionAuthority, OriginalNativeArenaDisposition, OriginalNativeBudget,
    OriginalNativeBudgetOwner, OriginalNativeBudgetSnapshot,
    OriginalNativeCleanupAuthority, OriginalNativeConnectionAdmission,
    OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority, OriginalNativeFactory,
    OriginalNativeFactoryFailure, OriginalNativeOutsideLeafEvidence,
    OriginalNativeProducerCellIndex, OriginalNativeProducerControl,
    ReleasedOriginalNativeAcquisition,
    OriginalNativeProjectionLeaseAdmission, OriginalNativeTerminal,
    SqliteInspectionProductionControl, SqliteInspectionProductionKind,
    SqliteInspectionProductionPermit, SqliteInspectionProductionPublicationFailure,
    CONTROL_STORAGE, MAX_NATIVE_CONNECTIONS,
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
    /// Refuse a runtime directory other than the one this authority holds
    /// exclusively. The host asks before any factory runtime binding.
    fn require_runtime(&self, runtime: &PinnedDirectory) -> Result<()>;
    /// Charge `steps` SQL VM steps to the caller's own SQL quota before the
    /// host debits its budget. The host calls this from the progress handler
    /// of every connection it admits, once per opcode, so it must stay cheap:
    /// in-memory counters and deadlines only, no IO and no durable check. A
    /// refusal stops the running statement.
    fn charge_sql_vm_steps(&self, steps: u64) -> Result<()>;
}

/// Exact cleanup steps a raw budget must reserve for `storage` bytes and
/// `tickets` connections: the native teardown quote plus the release steps.
pub fn offline_native_required_cleanup_steps(storage: u64, tickets: u32) -> Result<u64> {
    OriginalNativeFactory::required_cleanup_steps(storage, tickets)?
        .checked_add(OFFLINE_NATIVE_RELEASE_STEPS)
        .context("offline native cleanup quote overflow")
}

/// Arena block size of the native allocator.
const NATIVE_ARENA_BLOCK_BYTES: u64 = 256;
/// Worst-case allocator charge of one block's turnover: the request charge,
/// the bounded bin probes, the larger-bin fallback and the free.
const NATIVE_BLOCK_TURNOVER_STEPS: u64 = 1 + 8 + 1 + 1;
/// Per-ticket service charges outside SQL: one lease scan (its scan bound,
/// the ticket table and a fixed margin), two grouped shared-map reservations
/// over the fixed map table, and the factory's own per-ticket bookkeeping.
const NATIVE_TICKET_SERVICE_STEPS: u64 = (4096 + 64 + 16) + 2 * (64 + 1) + 8;

/// Minimum allocator funding margin, in native steps, that a raw budget must
/// carry beyond SQL opcodes and teardown for `storage` bytes and `tickets`
/// connections. It is a heuristic margin, NOT a bound: it prices one complete
/// turnover of every arena block at the worst-case per-request charge, plus
/// each ticket's fixed service charges, but SQLite may reuse the arena any
/// number of times. A budget that meets this margin can still exhaust; native
/// exhaustion then fails closed. The signed envelope, not this margin, bounds
/// the work.
pub fn offline_native_allocation_funding_margin_steps(storage: u64, tickets: u32) -> Result<u64> {
    ensure!(
        storage > CONTROL_STORAGE && tickets > 0 && tickets as usize <= MAX_NATIVE_CONNECTIONS,
        "offline native allocation funding margin outside the arena's storage or ticket bounds"
    );
    // The arena maps at most half of the storage beyond its control block.
    let blocks = (storage - CONTROL_STORAGE) / 2 / NATIVE_ARENA_BLOCK_BYTES;
    blocks
        .checked_mul(NATIVE_BLOCK_TURNOVER_STEPS)
        .and_then(|steps| steps.checked_add(u64::from(tickets).checked_mul(NATIVE_TICKET_SERVICE_STEPS)?))
        .context("offline native allocation funding margin overflow")
}

/// Plain facts of a host whose backend finished: the region's process,
/// generation and size; the raw budget's funded storage; the backend
/// terminal's arena disposition; the budget handle's counters at its
/// retirement; and the census of issued tickets, ticketed terminals
/// (including released unpublished tickets), ticketless refusals and leases.
/// No field carries an owner, so callers keep them across
/// [`OfflineNativeHost::release`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineNativeFacts {
    pub process: u32,
    pub generation: u64,
    pub region_bytes: u64,
    /// Funded storage of the raw budget.
    pub storage_bytes: u64,
    pub arena: OriginalNativeArenaDisposition,
    /// The budget handle's counters at its retirement.
    pub budget: OriginalNativeBudgetSnapshot,
    pub tickets_issued: u32,
    /// Ticketed terminals, including released unpublished tickets.
    pub terminals: usize,
    /// Acquisitions refused before a ticket was debited.
    pub ticketless: usize,
    pub leases: usize,
}

/// A released host. `facts` is present when the backend finished, and
/// `region` when an admitted region was physically released; a partial host
/// refused before its factory finished carries neither.
pub struct OfflineNativeReleased {
    pub facts: Option<OfflineNativeFacts>,
    pub region: Option<HostStorageRegionReleaseFact>,
    /// How the durability pins of census terminals ended at the release.
    pub durability_pins: DurabilityPinRelease,
}

/// Durability pins of census terminals that a host release ended, counted
/// across every release attempt of the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DurabilityPinRelease {
    /// Pins whose descriptor the release closed.
    pub closed: usize,
    /// Pins whose close consumed the descriptor but reported an error.
    pub consumed_with_error: usize,
}

impl DurabilityPinRelease {
    pub(crate) fn count(&mut self, close: crate::native::DurabilityPinClose) -> Result<()> {
        let slot = match close {
            crate::native::DurabilityPinClose::Closed => &mut self.closed,
            crate::native::DurabilityPinClose::ConsumedWithError => &mut self.consumed_with_error,
            crate::native::DurabilityPinClose::NotClosedHere => return Ok(()),
        };
        *slot = slot.checked_add(1).context("offline native retired pin count overflow")?;
        Ok(())
    }
}
impl std::fmt::Debug for OfflineNativeReleased {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineNativeReleased")
            .field("facts", &self.facts)
            .field("region_released", &self.region.is_some())
            .field("durability_pins", &self.durability_pins)
            .finish()
    }
}

/// A host release refused because an alias of its backend terminal or its
/// factory survives outside the host. Both stay with the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfflineNativeAliasRefusal {
    pub backend: usize,
    pub backend_held: usize,
    pub factory: usize,
    pub factory_held: usize,
}
impl std::fmt::Display for OfflineNativeAliasRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "offline native release refused while aliases survive: backend terminal strong count {} \
             (host holds {}), factory strong count {} (host holds {}); both retained",
            self.backend, self.backend_held, self.factory, self.factory_held
        )
    }
}
impl std::error::Error for OfflineNativeAliasRefusal {}

/// A refused finish stage with the whole host retained for another attempt.
pub struct OfflineNativeRetained {
    pub host: OfflineNativeHost,
    pub error: anyhow::Error,
}
impl std::fmt::Debug for OfflineNativeRetained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineNativeRetained").field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for OfflineNativeRetained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "offline native host release refused; host retained: {:#}", self.error)
    }
}
impl std::error::Error for OfflineNativeRetained {}

/// Sole owner of one process-scoped native arena. No Drop performs native
/// cleanup. Teardown is [`Self::finish_backend`] then [`Self::release`]
/// (composed by [`Self::finish`]); each stage is resumable, and `release`
/// consumes the host so nothing can reach its factory or region afterwards.
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
    /// Recorded once the backend finished and the budget handle retired.
    facts: Option<OfflineNativeFacts>,
    /// Funded storage of `raw`, reported identically on every finish path.
    storage_bytes: u64,
    /// Durability pins ended by release so far, kept across a retry.
    durability_pins: DurabilityPinRelease,
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
    pub fn release(self) -> std::result::Result<anyhow::Error, Self> {
        let Self { host, error } = self;
        let Some(host) = host else { return Ok(error) };
        match host.finish() {
            Ok(_) => Ok(error),
            Err(OfflineNativeRetained { host, error: cleanup }) => Err(Self {
                host: Some(host),
                error: error.context(format!("partial host release retained: {cleanup:#}")),
            }),
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
            facts: None,
            storage_bytes: 0,
            durability_pins: DurabilityPinRelease::default(),
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
        self.region.confirm_backing_residency()?;
        let residency = self.region.backing_residency_proof()
            .context("offline native region has no residency proof")?;
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
            ticketed: Mutex::new(Vec::new()),
            refusal: AtomicBool::new(false),
            residency,
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

    /// The admitting factory: valid from Ready until the backend finishes.
    pub fn factory(&self) -> Result<&Arc<OriginalNativeFactory>> {
        ensure!(self.ready && self.backend.is_none(), "offline native factory is not admitting");
        self.factory.as_ref().context("offline native factory absent")
    }

    /// The same factory after [`Self::finish_backend`] completed and before
    /// [`Self::release`], for the post-backend steps that join the backend
    /// terminal: final acknowledgements, retirements after the backend and
    /// outside custody. It admits no new connection.
    pub fn post_backend_factory(&self) -> Result<&Arc<OriginalNativeFactory>> {
        ensure!(self.backend.is_some() && self.facts.is_some(), "offline native backend has not finished");
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
        let checked = self.census_open().and_then(|factory| terminal.require_original(factory));
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
        let checked = self.census_open().and_then(|factory| lease.require_original(factory));
        if let Err(error) = checked {
            return Err((lease, error));
        }
        self.leases.lock().unwrap_or_else(PoisonError::into_inner).push(lease);
        Ok(())
    }

    /// The census closes when the backend finishes against it.
    fn census_open(&self) -> Result<&Arc<OriginalNativeFactory>> {
        ensure!(self.backend.is_none(), "offline native census closed: the backend already finished");
        self.factory.as_ref().context("offline native factory absent")
    }

    /// Issued ticket count and recorded terminals plus leases. Once the
    /// backend finished this answers from [`Self::facts`], and it refuses
    /// while the backend terminal exists without its facts, so it can never
    /// contradict them.
    pub fn ticket_census(&self) -> Result<(u32, usize)> {
        if let Some(facts) = &self.facts {
            let recorded = facts.terminals.checked_add(facts.leases).context("offline native census overflow")?;
            return Ok((facts.tickets_issued, recorded));
        }
        ensure!(self.backend.is_none(), "offline native census closed: backend finished without its facts");
        let factory = self.factory.as_ref().context("offline native factory absent")?;
        let issued = factory.budget_snapshot()?.connections_issued;
        let recorded = self.terminals.lock().unwrap_or_else(PoisonError::into_inner).len()
            + self.leases.lock().unwrap_or_else(PoisonError::into_inner).len();
        Ok((issued, recorded))
    }

    /// Close admission, release every debited-but-unpublished ticket, drain
    /// and finish the backend against the exhaustive terminal census, retire
    /// the budget handle and record [`OfflineNativeFacts`]. The region stays
    /// unsealed and the factory stays reachable through
    /// [`Self::post_backend_factory`]. Each stage is retained before the
    /// next; a retry after success returns the same terminal.
    pub fn finish_backend(&mut self) -> Result<Arc<OriginalNativeTerminal>> {
        if self.facts.is_some() {
            return self.backend.clone().context("finished offline native backend terminal absent");
        }
        self.closed.store(true, Ordering::Release);
        self.authority.checkpoint(true)?;
        let factory = self.factory.clone().context("offline native factory absent; no backend to finish")?;
        if self.backend.is_none() {
            let leases = self.leases.lock().unwrap_or_else(PoisonError::into_inner).clone();
            // The factory's own Ready acknowledgement selects the finish:
            // initialization can refuse after Ready was published.
            let finished = if factory.ready_acknowledged()? {
                let admission = self.admission.clone().context("offline admission absent")?;
                let released = admission.release_ticketed(&factory)?;
                self.terminals.lock().unwrap_or_else(PoisonError::into_inner).extend(released);
                let terminals = self.terminals.lock().unwrap_or_else(PoisonError::into_inner).clone();
                if self.drained.is_none() {
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
        let backend = self.backend.clone().context("offline native backend terminal absent")?;
        let admission = self.admission.clone().context("offline admission absent")?;
        let budget = admission.budget.retire_original_handle(&factory, &backend)?;
        let generation = self.region.generation().context("finished offline native region lacks its generation")?;
        self.facts = Some(OfflineNativeFacts {
            process: self.region.process(),
            generation,
            region_bytes: self.region.bytes(),
            storage_bytes: self.storage_bytes,
            arena: backend.arena_disposition()?,
            tickets_issued: budget.connections_issued,
            budget,
            terminals: self.terminals.lock().unwrap_or_else(PoisonError::into_inner).len(),
            ticketless: self.ticketless.lock().unwrap_or_else(PoisonError::into_inner).len(),
            leases: self.leases.lock().unwrap_or_else(PoisonError::into_inner).len(),
        });
        Ok(backend)
    }

    /// The facts recorded by [`Self::finish_backend`].
    pub fn facts(&self) -> Result<OfflineNativeFacts> {
        self.facts.clone().context("offline native backend has not finished")
    }

    /// End the host. In order: drop the drain inputs, terminals, leases,
    /// destination, factory failures and ticketed preparations; refuse,
    /// keeping both and naming their counts, while any alias of the backend
    /// terminal or the factory survives outside this host; only then drop
    /// both and seal, dispose and release the region. A refusal returns the
    /// whole host, and every completed stage stays done for the retry.
    ///
    /// A host whose factory exists must have finished its backend first.
    ///
    /// ```compile_fail,E0382
    /// fn released(host: lillux_sqlite::offline_host::OfflineNativeHost) {
    ///     let _ = host.release();
    ///     let _ = host.post_backend_factory();
    /// }
    /// ```
    pub fn release(mut self) -> std::result::Result<OfflineNativeReleased, OfflineNativeRetained> {
        match self.release_stages() {
            Ok(released) => Ok(released),
            Err(error) => Err(OfflineNativeRetained { host: self, error }),
        }
    }

    fn release_stages(&mut self) -> Result<OfflineNativeReleased> {
        self.closed.store(true, Ordering::Release);
        self.authority.checkpoint(true)?;
        ensure!(
            self.factory.is_none() || self.facts.is_some(),
            "offline native release refused: the backend has not finished"
        );
        drop(self.drained.take());
        self.retire_terminal_durability_pins()?;
        self.terminals.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.ticketless.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.leases.lock().unwrap_or_else(PoisonError::into_inner).clear();
        drop(self.destination.take());
        self.factory_failures.clear();
        if let Some(admission) = &self.admission {
            admission.ticketed.lock().unwrap_or_else(PoisonError::into_inner).clear();
        }
        // The host holds one reference to each, and the backend terminal one
        // more to its factory. Any other reference is a caller alias.
        let backend = self.backend.as_ref().map_or(0, Arc::strong_count);
        let backend_held = usize::from(self.backend.is_some());
        let factory = self.factory.as_ref().map_or(0, Arc::strong_count);
        let factory_held = usize::from(self.factory.is_some()) + backend_held;
        if backend != backend_held || factory != factory_held {
            return Err(OfflineNativeAliasRefusal { backend, backend_held, factory, factory_held }.into());
        }
        drop(self.backend.take());
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
        let region = match &self.facts {
            Some(facts) => Some(self.region.region_release_fact(facts.generation, facts.region_bytes)?),
            None => None,
        };
        Ok(OfflineNativeReleased {
            facts: self.facts.clone(),
            region,
            durability_pins: self.durability_pins,
        })
    }

    /// Close the durability pins of every census terminal before the census
    /// ends; their Drop would only keep each descriptor open until process
    /// exit. Each close first rechecks the caller's cleanup window. A closed
    /// connection holds at most a main and a WAL pin, and closed connections
    /// are ticketed, so the count is bounded by twice the ticket limit.
    ///
    /// Other descriptors a terminal can reach are not retired here. Its
    /// destination inputs carry the caller's destination authority, whose
    /// owners (directories, destination roles, finalizer files) belong to the
    /// caller's types and end with them; this host has no typed close for
    /// them. Inspection bodies own no descriptor: their destination roles are
    /// held and retired by the caller's copy work and disposal owners.
    fn retire_terminal_durability_pins(&mut self) -> Result<()> {
        let authority = self.authority.clone();
        let recheck = || authority.checkpoint(true);
        for slot in [&self.terminals, &self.ticketless] {
            let terminals = slot.lock().unwrap_or_else(PoisonError::into_inner).clone();
            for terminal in terminals {
                if let OriginalNativeConnectionTerminal::Closed(closed) = terminal {
                    closed
                        .retire_durability_pins(&recheck, &mut self.durability_pins)
                        .context("offline native release refused: a census durability pin did not retire")?;
                }
            }
        }
        Ok(())
    }

    /// [`Self::finish_backend`] then [`Self::release`]; a partial host
    /// without a factory goes straight to release.
    pub fn finish(mut self) -> std::result::Result<OfflineNativeReleased, OfflineNativeRetained> {
        if self.factory.is_some() {
            match self.finish_backend() {
                Ok(backend) => drop(backend),
                Err(error) => return Err(OfflineNativeRetained { host: self, error }),
            }
        }
        self.release()
    }
}


struct OfflineAdmission {
    original: Weak<OfflineAdmission>,
    budget: Arc<OriginalNativeBudgetOwner>,
    region: OriginalNativeRegionHandle,
    issuer: OnceLock<OriginalNativeProducerCellIndex>,
    closed: Arc<AtomicBool>,
    authority: Arc<dyn OfflineNativeAuthority>,
    /// Producer preparations refused after their ticket was debited. Each
    /// keeps its cell until finish turns it into a released terminal.
    ticketed: Mutex<Vec<OfflineTicketedPreparation>>,
    /// Service-refusal latch of this one offline factory. Every service check
    /// and VM step reads it; cleanup never does. Never cleared.
    refusal: AtomicBool,
    /// The residency proof of this host's region, copied when residency
    /// completed. Every new ticket re-checks it; the region stays mapped
    /// while the admission lives.
    residency: lillux::process_control::ResidencyProof,
}

/// A debited ticket whose producer never published: either its bind was
/// refused, or its publication was. The cell, and the publication failure
/// that owns it, stay here until the factory acknowledges the ticket unused.
struct OfflineTicketedPreparation {
    ticket: u32,
    core: Option<OriginalNativeProducerControl>,
    publication: Option<SqliteInspectionProductionPublicationFailure>,
    callback: Option<Arc<dyn SqliteInspectionProductionControl>>,
    terminal: Option<Arc<ReleasedOriginalNativeAcquisition>>,
    recorded: bool,
}

impl OfflineAdmission {
    fn production_control(&self) -> Arc<dyn SqliteInspectionProductionControl> {
        Arc::new(OfflineProductionControl { closed: self.closed.clone(), authority: self.authority.clone() })
    }

    fn begin_ticket(&self, kind: SqliteInspectionProductionKind) -> Result<(u32, SqliteInspectionProductionPermit)> {
        self.require_original(false)?;
        // A new ticket is new native work against the resident region.
        self.residency
            .confirm_swap_unreachable()
            .map_err(|reason| anyhow::anyhow!("native residency no longer holds: {reason}"))?;
        ensure!(self.original.strong_count() > 0, "offline admission no longer retained");
        let core = self.region.claim_producer_control(kind)?;
        let ticket = match self.budget.debit_connection_ticket() {
            Ok(ticket) => ticket,
            Err(error) => return Err(retire_unbound(core, error)),
        };
        if let Err(refusal) = core.bind_ticket(ticket) {
            // The ticket is debited: the census needs its terminal, so the
            // cell stays with the admission until finish releases it.
            self.retain_ticketed(OfflineTicketedPreparation {
                ticket, core: Some(core), publication: None, callback: None, terminal: None, recorded: false,
            });
            anyhow::bail!("offline producer ticket {ticket} bind refused: {refusal}; retained for its released terminal");
        }
        // SAFETY: the bound cell and ticket come from this host's own region.
        match unsafe { SqliteInspectionProductionPermit::issued_from_region(kind, self.production_control(), core) } {
            Ok(permit) => Ok((ticket, permit)),
            Err(failure) => {
                let refusal = failure.refusal;
                self.retain_ticketed(OfflineTicketedPreparation {
                    ticket, core: None, publication: Some(failure), callback: None, terminal: None, recorded: false,
                });
                anyhow::bail!("offline producer ticket {ticket} publication refused: {refusal}; retained for its released terminal")
            }
        }
    }

    fn retain_ticketed(&self, preparation: OfflineTicketedPreparation) {
        self.ticketed.lock().unwrap_or_else(PoisonError::into_inner).push(preparation);
    }

    /// Acknowledge every retained debited-but-unpublished ticket through the
    /// factory and return the released terminals not yet recorded. A refusal
    /// keeps every preparation, including completed ones, for a retry.
    fn release_ticketed(&self, factory: &Arc<OriginalNativeFactory>) -> Result<Vec<OriginalNativeConnectionTerminal>> {
        let mut retained = std::mem::take(&mut *self.ticketed.lock().unwrap_or_else(PoisonError::into_inner));
        // No bookkeeping lock spans the native effects below.
        let result = (|| -> Result<Vec<OriginalNativeConnectionTerminal>> {
            for preparation in &mut retained {
                if let Some(failure) = preparation.publication.take() {
                    match failure.into_parts() {
                        Ok((_, callback, core)) => {
                            preparation.callback = Some(callback);
                            preparation.core = Some(core);
                        }
                        Err(failure) => {
                            preparation.publication = Some(failure);
                            anyhow::bail!("offline publication custody transfer refused for ticket {}", preparation.ticket);
                        }
                    }
                }
                if preparation.terminal.is_none() {
                    let core = preparation.core.as_ref().context("offline ticketed preparation cell absent")?;
                    let observed = core.observation()?.ticket;
                    if observed == 0 {
                        core.bind_ticket(preparation.ticket)?;
                    } else {
                        ensure!(
                            observed == preparation.ticket,
                            "offline ticketed preparation cell carries ticket {observed}, not its debited ticket {}",
                            preparation.ticket
                        );
                    }
                    preparation.terminal = Some(factory.retire_unpublished_ticket(core)?);
                }
                if let Some(callback) = &preparation.callback {
                    callback.complete()?;
                }
            }
            let mut released = Vec::new();
            for preparation in &mut retained {
                if !preparation.recorded {
                    let terminal = preparation.terminal.clone().context("offline released ticket terminal absent")?;
                    released.push(OriginalNativeConnectionTerminal::Released(terminal));
                    preparation.recorded = true;
                }
            }
            Ok(released)
        })();
        self.ticketed.lock().unwrap_or_else(PoisonError::into_inner).extend(retained);
        result
    }
}

impl OriginalNativeAdmissionAuthority for OfflineAdmission {
    fn require_original(&self, cleanup: bool) -> Result<()> {
        if !cleanup {
            ensure!(!self.refusal.load(Ordering::SeqCst), "offline native service refused");
        }
        ensure!(cleanup || !self.closed.load(Ordering::Acquire), "offline native admission closed");
        let checked = self.authority.checkpoint(cleanup);
        if let Err(error) = &checked {
            if crate::native_refusal_is_fact(error) {
                self.refuse_service();
            }
        }
        checked
    }
    fn require_runtime_binding(&self, runtime: &PinnedDirectory) -> Result<()> {
        self.require_original(false)?;
        self.authority.require_runtime(runtime)?;
        runtime.ensure_path_binding()
    }
    fn budget(&self) -> Arc<OriginalNativeBudgetOwner> {
        self.budget.clone()
    }
    fn debit_sql_vm_steps(&self, steps: u64) -> Result<()> {
        ensure!(!self.refusal.load(Ordering::SeqCst), "offline native service refused");
        self.authority.charge_sql_vm_steps(steps)?;
        self.budget.debit_steps(steps, false)
    }
    fn refuse_service(&self) {
        self.refusal.store(true, Ordering::SeqCst);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_host::TestNativeHost;

    #[test]
    fn allocation_funding_margin_prices_one_arena_turnover_and_every_ticket() {
        let storage = 50_331_648;
        let blocks = (storage - CONTROL_STORAGE) / 2 / NATIVE_ARENA_BLOCK_BYTES;
        assert_eq!(
            offline_native_allocation_funding_margin_steps(storage, 64).unwrap(),
            blocks * NATIVE_BLOCK_TURNOVER_STEPS + 64 * NATIVE_TICKET_SERVICE_STEPS
        );
        assert!(
            offline_native_allocation_funding_margin_steps(storage, 2).unwrap()
                < offline_native_allocation_funding_margin_steps(storage, 3).unwrap()
        );
        assert!(offline_native_allocation_funding_margin_steps(CONTROL_STORAGE, 1).is_err());
        assert!(offline_native_allocation_funding_margin_steps(storage, 0).is_err());
        assert!(
            offline_native_allocation_funding_margin_steps(storage, MAX_NATIVE_CONNECTIONS as u32 + 1).is_err()
        );
    }

    #[test]
    fn released_ticket_requires_the_cell_to_carry_its_debited_ticket() {
        let test = TestNativeHost::acquire();
        let host = test.live_host();
        let admission = host.admission.clone().expect("installed admission");
        let factory = host.factory().expect("admitting factory").clone();
        let core = admission
            .region
            .claim_producer_control(SqliteInspectionProductionKind::Native)
            .expect("producer cell");
        let ticket = admission.budget.debit_connection_ticket().expect("debited ticket");
        core.bind_ticket(ticket).map_err(|refusal| anyhow::anyhow!("{refusal}")).expect("bound cell");
        admission.retain_ticketed(OfflineTicketedPreparation {
            ticket: ticket + 1,
            core: Some(core),
            publication: None,
            callback: None,
            terminal: None,
            recorded: false,
        });
        let error = match admission.release_ticketed(&factory) {
            Ok(_) => panic!("a foreign cell ticket is refused"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains(&format!("carries ticket {ticket}, not its debited ticket {}", ticket + 1)),
            "{message}"
        );
        {
            let mut retained = admission.ticketed.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!(retained.len(), 1, "the refused preparation is kept for a retry");
            assert!(retained[0].core.is_some() && retained[0].terminal.is_none() && !retained[0].recorded);
            // Restore the debited ticket so the host's own finish releases
            // the cell into the census.
            retained[0].ticket = ticket;
        }
        drop(factory);
        drop(admission);
        drop(test);
    }

    #[test]
    fn census_answers_from_facts_once_the_backend_finished() {
        let mut test = TestNativeHost::acquire();
        let connection = test.open_transient();
        test.close(connection);
        let host = test.live_host_mut();
        drop(host.finish_backend().expect("backend finished"));
        let facts = host.facts().expect("facts recorded");
        assert_eq!(host.ticket_census().expect("census"), (facts.tickets_issued, facts.terminals + facts.leases));
        assert_eq!(host.ticket_census().expect("census"), (1, 1));
    }
}
