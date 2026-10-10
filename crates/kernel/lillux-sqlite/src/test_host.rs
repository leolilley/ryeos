//! Test-only native host. Each acquisition installs one real arena through
//! the same budget, region, install and initialization sequence a production
//! issuer runs, and tears it down through the exhaustive terminal census.
//!
//! The arena is a process singleton, so hosts serialize on a process-wide
//! mutex. The host holds that mutex guard and is therefore not `Send`: use it
//! from the acquiring thread only, with current-thread runtimes. Test binaries
//! that use it need nextest or `--test-threads=1`. Acquire the host before any
//! fork lease and never wait for it while holding one.
use crate::original_host_owner::backing::HostStorage;
use crate::original_host_owner::region::NativeHostRegion;
use crate::original_host_owner::OriginalNativeRegionHandle;
use crate::{
    OriginalDestinationCustodyFailure, OriginalDestinationOutsideCustody,
    OriginalLiveNativeDrainInputs, OriginalNativeAdmissionAuthority, OriginalNativeBoundInputs,
    OriginalNativeBoundMode, OriginalNativeBudget, OriginalNativeBudgetOwner,
    OriginalNativeCleanupAuthority, OriginalNativeConnection, OriginalNativeConnectionAdmission,
    OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority, OriginalNativeDomain,
    OriginalNativeFactory, OriginalNativeOpenFailure, OriginalNativeOutsideLeafEvidence,
    OriginalNativeProducerCellIndex, OriginalNativeProducerControl,
    OriginalNativeProjectionLeaseAdmission, OriginalNativeTerminal, OriginalNativeTransientInputs,
    SqliteInspectionProductionControl, SqliteInspectionProductionKind,
    SqliteInspectionProductionPermit,
};
use anyhow::{Context, Result, ensure};
use lillux::PinnedDirectory;
use std::cell::RefCell;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;
const RELEASE_ATTEMPTS: u64 = 8;
const ACQUIRE_DEADLINE: Duration = Duration::from_secs(120);

/// Native envelope one test host installs. The SQLite arena is
/// `(storage_bytes - 64 KiB) / 2`, page-rounded; cleanup is quoted from the
/// same layout before the arena exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestNativeEnvelope {
    pub storage_bytes: u64,
    pub allocator_steps: u64,
    pub tickets: u32,
}
impl Default for TestNativeEnvelope {
    fn default() -> Self {
        Self { storage_bytes: 16 * MIB, allocator_steps: 1_000_000_000, tickets: 64 }
    }
}

static SERIAL: Mutex<()> = Mutex::new(());
static HOLDER: Mutex<Option<String>> = Mutex::new(None);
static LEAKED: Mutex<Option<String>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(1);

fn current_holder() -> String {
    let thread = std::thread::current();
    format!("{} ({:?})", thread.name().unwrap_or("<unnamed>"), thread.id())
}

/// Serialized owner of one real native arena at a time.
pub struct TestNativeHost {
    envelope: TestNativeEnvelope,
    live: Option<LiveTestHost>,
    _serial: MutexGuard<'static, ()>,
}

impl TestNativeHost {
    /// Default envelope: 16 MiB storage, 64 tickets, 10^9 allocator steps.
    pub fn acquire() -> Self {
        Self::acquire_with(TestNativeEnvelope::default())
    }

    /// Panics naming the current holder if the host stays busy past the
    /// acquire deadline, and with the leaked census if a previous host could
    /// not release its arena.
    pub fn acquire_with(envelope: TestNativeEnvelope) -> Self {
        let started = Instant::now();
        let serial = loop {
            match SERIAL.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if started.elapsed() >= ACQUIRE_DEADLINE {
                        let holder = HOLDER.lock().unwrap_or_else(PoisonError::into_inner).clone();
                        panic!(
                            "TestNativeHost acquire deadline elapsed after {:?}; held by {}",
                            ACQUIRE_DEADLINE,
                            holder.as_deref().unwrap_or("<unknown>")
                        );
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        if let Some(census) = LEAKED.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            panic!("previous TestNativeHost leaked its arena: {census}");
        }
        *HOLDER.lock().unwrap_or_else(PoisonError::into_inner) = Some(current_holder());
        let live = match LiveTestHost::install(envelope) {
            Ok(live) => live,
            Err(error) => {
                let census = format!("install refused: {error:#}");
                *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(census.clone());
                panic!("TestNativeHost {census}");
            }
        };
        Self { envelope, live: Some(live), _serial: serial }
    }

    fn live(&self) -> &LiveTestHost {
        self.live.as_ref().expect("live test host present outside teardown")
    }

    pub fn factory(&self) -> &Arc<OriginalNativeFactory> {
        &self.live().factory
    }

    /// Destination authority for opens this host issues directly.
    pub fn destination(&self) -> Arc<dyn OriginalNativeDestinationAuthority> {
        self.live().destination.clone()
    }

    /// One in-memory connection on a fresh ticket.
    pub fn open_transient(&self) -> OriginalNativeConnection {
        let inputs = unsafe {
            // SAFETY: test-only issuance under this host's own funded factory.
            OriginalNativeTransientInputs::issued(OriginalNativeDomain::StateTransient, self.destination())
        }
        .expect("test transient inputs");
        match self.factory().open_transient(inputs) {
            Ok(connection) => connection,
            Err(failure) => panic!("test transient open refused: {failure}"),
        }
    }

    /// One bound connection on a fresh ticket. A refusal is returned with
    /// its owner; hand it to [`Self::retire_open_failure`].
    pub fn open_bound(
        &self,
        root: Arc<PinnedDirectory>,
        name: &OsStr,
        mode: OriginalNativeBoundMode,
        domain: OriginalNativeDomain,
    ) -> std::result::Result<OriginalNativeConnection, OriginalNativeOpenFailure> {
        let inputs = unsafe {
            // SAFETY: test-only issuance under this host's own funded factory.
            OriginalNativeBoundInputs::issued(root, name, mode, domain, self.destination(), None)
        }
        .expect("test bound inputs");
        self.factory().open_bound(inputs)
    }

    /// Close a connection and record its terminal in this host's census.
    pub fn close(&self, connection: OriginalNativeConnection) {
        let closed = match connection.close() {
            Ok(closed) => closed,
            Err(failure) => match failure.retry() {
                Ok(closed) => closed,
                Err(failure) => panic!("test connection close retained: {failure}"),
            },
        };
        self.retain_terminal(OriginalNativeConnectionTerminal::Closed(closed));
    }

    /// Release a refused open and record its terminal in this host's census.
    pub fn retire_open_failure(&self, failure: OriginalNativeOpenFailure) {
        match failure.retry_close() {
            Ok(released) => self.retain_terminal(OriginalNativeConnectionTerminal::Released(released)),
            Err(failure) => panic!("test open failure release retained: {failure}"),
        }
    }

    /// Record a terminal produced by code under test.
    pub fn retain_terminal(&self, terminal: OriginalNativeConnectionTerminal) {
        terminal.require_original(self.factory()).expect("terminal belongs to this test host");
        self.live().terminals.borrow_mut().push(terminal);
    }

    /// Record a closed projection lease for the final census.
    pub fn retain_lease(&self, lease: Arc<crate::ClosedOriginalProjectionLease>) {
        self.live().leases.borrow_mut().push(lease);
    }

    /// Issued ticket count and recorded terminals.
    pub fn ticket_census(&self) -> (u32, usize) {
        let live = self.live();
        let issued = live.factory.budget_snapshot().map(|snapshot| snapshot.connections_issued).unwrap_or(u32::MAX);
        (issued, live.terminals.borrow().len())
    }

    /// Tear down this arena and install a fresh one under the same serial
    /// guard, for suites that need more than one envelope of tickets.
    pub fn cycle(&mut self) {
        let live = self.live.take().expect("live test host present");
        if let Err(census) = live.finish() {
            *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(census.clone());
            panic!("TestNativeHost cycle leaked its arena: {census}");
        }
        match LiveTestHost::install(self.envelope) {
            Ok(live) => self.live = Some(live),
            Err(error) => {
                let census = format!("reinstall refused: {error:#}");
                *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(census.clone());
                panic!("TestNativeHost {census}");
            }
        }
    }
}

impl Drop for TestNativeHost {
    fn drop(&mut self) {
        let Some(live) = self.live.take() else { return };
        let finished = live.finish();
        *HOLDER.lock().unwrap_or_else(PoisonError::into_inner) = None;
        if let Err(census) = finished {
            *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(census.clone());
            if !std::thread::panicking() {
                panic!("TestNativeHost leaked its arena: {census}");
            }
        }
    }
}

struct LiveTestHost {
    region: NativeHostRegion,
    admission: Arc<TestAdmission>,
    destination: Arc<TestDestination>,
    factory: Arc<OriginalNativeFactory>,
    terminals: RefCell<Vec<OriginalNativeConnectionTerminal>>,
    leases: RefCell<Vec<Arc<crate::ClosedOriginalProjectionLease>>>,
}

impl LiveTestHost {
    fn install(envelope: TestNativeEnvelope) -> Result<Self> {
        let layout = HostStorage::quote_native_region().map_err(anyhow::Error::msg)?;
        let cleanup = OriginalNativeFactory::required_cleanup_steps(envelope.storage_bytes, envelope.tickets)?
            .checked_add(RELEASE_ATTEMPTS)
            .context("test cleanup quote overflow")?;
        // SAFETY: test-only funding; this host is the sole issuer of the envelope.
        let raw = unsafe {
            OriginalNativeBudget::prepaid_by_original_issuer(
                envelope.storage_bytes, envelope.allocator_steps, envelope.tickets, cleanup,
            )
        }?;
        let mut host_region = NativeHostRegion::new(layout);
        host_region.enter(raw, RELEASE_ATTEMPTS, || Ok(()))?;
        host_region.map_resident(|| Ok(()))?;
        let generation = GENERATION.fetch_add(1, Ordering::AcqRel);
        // SAFETY: test-only funding of this host's own quote; the generation
        // comes from a process-wide counter and is never reused.
        let budget: Arc<OriginalNativeBudgetOwner> = unsafe { host_region.initialize(generation) }?;
        let region = host_region.handle()?;
        let admission = Arc::new_cyclic(|original| TestAdmission {
            original: original.clone(),
            budget,
            region,
            issuer: OnceLock::new(),
            closed: AtomicBool::new(false),
        });
        let core = admission.region.claim_producer_control(SqliteInspectionProductionKind::FactoryIssuer)?;
        admission
            .issuer
            .set(core.index())
            .map_err(|_| anyhow::anyhow!("test issuer index already set"))?;
        let control: Arc<dyn SqliteInspectionProductionControl> = Arc::new(TestProductionControl);
        // SAFETY: the issuer cell belongs to this host's own region.
        let permit = unsafe {
            SqliteInspectionProductionPermit::issued_from_region(
                SqliteInspectionProductionKind::FactoryIssuer, control, core,
            )
        }
        .map_err(|failure| anyhow::anyhow!("test issuer publication refused: {}", failure.refusal))?;
        let authority: Arc<dyn OriginalNativeAdmissionAuthority> = admission.clone();
        // SAFETY: test-only installation of this host's funded region.
        let factory = unsafe {
            OriginalNativeFactory::install_from_original_issuer(admission.region.try_clone()?, authority, permit)
        }
        .map_err(anyhow::Error::new)?;
        factory.initialize_original_backend().map_err(anyhow::Error::new)?;
        let destination = Arc::new(TestDestination { factory: Arc::downgrade(&factory) });
        Ok(Self {
            region: host_region,
            admission,
            destination,
            factory,
            terminals: RefCell::new(Vec::new()),
            leases: RefCell::new(Vec::new()),
        })
    }

    /// Drain, finish, seal and release. A refusal returns the census text.
    fn finish(self) -> std::result::Result<(), String> {
        let LiveTestHost { mut region, admission, destination, factory, terminals, leases } = self;
        let terminals = terminals.into_inner();
        let leases = leases.into_inner();
        let resident_bytes = region.bytes();
        let census = || {
            let issued = factory.budget_snapshot().map(|snapshot| snapshot.connections_issued);
            format!(
                "issued tickets {issued:?}, recorded terminals {}, resident region {} bytes",
                terminals.len(),
                resident_bytes
            )
        };
        admission.closed.store(true, Ordering::Release);
        let cleanup: Arc<dyn OriginalNativeCleanupAuthority> = Arc::new(TestCleanup { admission: admission.clone() });
        // SAFETY: admission is closed above; the cleanup authority rechecks
        // producer quiescence before every native effect.
        let drained = match unsafe { OriginalLiveNativeDrainInputs::issued(factory.clone(), cleanup) } {
            Ok(inputs) => inputs,
            Err(error) => return Err(format!("drain refused: {error:#}; {}", census())),
        };
        let terminal = match factory.finish_live_backend(&terminals, &leases, &drained) {
            Ok(terminal) => terminal,
            Err(failure) => return Err(format!("finish refused: {failure}; {}", census())),
        };
        if let Err(error) = admission.budget.retire_original_handle(&factory, &terminal) {
            return Err(format!("budget retirement refused: {error:#}; {}", census()));
        }
        let summary = census();
        drop(terminal);
        drop(drained);
        drop(terminals);
        drop(leases);
        drop(destination);
        drop(factory);
        drop(admission);
        let release = (|| -> std::result::Result<(), String> {
            region.begin_close().map_err(|refusal| format!("{refusal:#}"))?;
            let census = region.census().map_err(|refusal| format!("{refusal:#}"))?;
            region
                .release(|| Ok(()))
                .map_err(|refusal| format!("release refused ({refusal:#}); region {census:?}"))?;
            region.require_released().map_err(|refusal| format!("{refusal:#}"))
        })();
        release.map_err(|error| format!("{error}; {summary}"))
    }
}

struct TestAdmission {
    original: Weak<TestAdmission>,
    budget: Arc<OriginalNativeBudgetOwner>,
    region: OriginalNativeRegionHandle,
    issuer: OnceLock<OriginalNativeProducerCellIndex>,
    closed: AtomicBool,
}

impl TestAdmission {
    fn begin_ticket(&self, kind: SqliteInspectionProductionKind) -> Result<(u32, SqliteInspectionProductionPermit)> {
        ensure!(!self.closed.load(Ordering::Acquire), "test native admission closed");
        ensure!(self.original.strong_count() > 0, "test admission no longer retained");
        let core = self.region.claim_producer_control(kind)?;
        let ticket = match self.budget.debit_connection_ticket() {
            Ok(ticket) => ticket,
            Err(error) => return Err(anyhow::Error::new(TestProducerFailure { error, _core: Some(core) })),
        };
        if let Err(refusal) = core.bind_ticket(ticket) {
            return Err(anyhow::Error::new(TestProducerFailure { error: refusal.into(), _core: Some(core) }));
        }
        let control: Arc<dyn SqliteInspectionProductionControl> = Arc::new(TestProductionControl);
        // SAFETY: the bound cell and ticket come from this host's own region.
        let permit = unsafe { SqliteInspectionProductionPermit::issued_from_region(kind, control, core) }
            .map_err(|failure| anyhow::Error::new(TestPublicationFailure {
                refusal: failure.refusal,
                _retained: failure,
            }))?;
        Ok((ticket, permit))
    }
}

impl OriginalNativeAdmissionAuthority for TestAdmission {
    fn require_original(&self, cleanup: bool) -> Result<()> {
        ensure!(cleanup || !self.closed.load(Ordering::Acquire), "test native admission closed");
        Ok(())
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

/// Retains the claimed producer cell of a refused ticket preparation.
struct TestProducerFailure {
    error: anyhow::Error,
    _core: Option<OriginalNativeProducerControl>,
}
impl std::fmt::Debug for TestProducerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestProducerFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for TestProducerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "test producer preparation retained: {}", self.error)
    }
}
impl std::error::Error for TestProducerFailure {}

/// Retains a refused producer publication with its cell and callback.
struct TestPublicationFailure {
    refusal: crate::original_host_owner::FixedHostRefusal,
    _retained: crate::SqliteInspectionProductionPublicationFailure,
}
impl std::fmt::Debug for TestPublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestPublicationFailure").field("refusal", &self.refusal).finish_non_exhaustive()
    }
}
impl std::fmt::Display for TestPublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "test producer publication retained: {}", self.refusal)
    }
}
impl std::error::Error for TestPublicationFailure {}

struct TestProductionControl;
impl SqliteInspectionProductionControl for TestProductionControl {
    fn checkpoint(&self) -> Result<()> {
        Ok(())
    }
    fn complete(&self) -> Result<()> {
        Ok(())
    }
    fn abandoned(&self) {}
}

struct TestCleanup {
    admission: Arc<TestAdmission>,
}
impl OriginalNativeCleanupAuthority for TestCleanup {
    fn require_original(&self, _factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        ensure!(self.admission.closed.load(Ordering::Acquire), "test producer admission still open");
        let issuer = self.admission.issuer.get().context("test issuer cell absent")?;
        self.admission.region.require_quiescent_nonissuer_producers(*issuer)?;
        Ok(())
    }
}

struct TestDestination {
    factory: Weak<OriginalNativeFactory>,
}
impl OriginalNativeDestinationAuthority for TestDestination {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        let original = self.factory.upgrade().context("test destination factory gone")?;
        ensure!(Arc::ptr_eq(&original, factory), "test destination selects foreign factory");
        Ok(())
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
            anyhow::anyhow!("test destination has no outside custody receiver"),
        ))
    }
}

#[cfg(test)]
impl TestNativeHost {
    /// Arm one ticket around a raw open in this crate's tests.
    pub(crate) fn armed<T>(&self, open: impl FnOnce() -> T) -> (T, crate::native::RawTestTicket) {
        self.factory().arm_raw_for_test(open).expect("armed raw test ticket")
    }
    /// Raw SQLite work inside an allocator scope of this host's arena.
    pub(crate) fn scope<T>(&self, work: impl FnOnce() -> T) -> T {
        self.factory().raw_scope_for_test(work).expect("raw test allocator scope")
    }
    /// Raw native cleanup work, such as stopped intake, in a cleanup scope.
    pub(crate) fn cleanup_scope<T>(&self, work: impl FnOnce() -> T) -> T {
        self.factory().raw_cleanup_scope_for_test(work).expect("raw test cleanup scope")
    }
    /// Recorded shared-mapping bytes and nonempty ledger slots.
    pub(crate) fn shared_census(&self) -> (u64, u32) {
        self.factory().shared_census_for_test().expect("native shared census")
    }
    /// Retire a raw ticket once everything it opened is closed.
    pub(crate) fn retire_raw(&self, raw: crate::native::RawTestTicket) {
        let terminal = self.factory().retire_raw_for_test(raw).expect("raw test ticket retirement");
        self.retain_terminal(terminal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unarmed_open_is_refused_with_misuse() {
        let refused = rusqlite::Connection::open_in_memory().err().expect("unarmed open refused");
        assert_eq!(refused.sqlite_error_code(), Some(rusqlite::ErrorCode::ApiMisuse), "{refused}");
    }

    #[test]
    fn transient_connection_runs_sql_and_census_releases_arena() {
        let mut host = TestNativeHost::acquire();
        let connection = host.open_transient();
        let value: i64 = connection
            .with_sql(|sql| Ok(sql.query_row("SELECT 6 * 7", [], |row| row.get(0))?))
            .unwrap();
        assert_eq!(value, 42);
        host.close(connection);
        assert_eq!(host.ticket_census(), (1, 1));
        host.cycle();
        assert_eq!(host.ticket_census(), (0, 0));
        let again = host.open_transient();
        host.close(again);
    }

    #[test]
    fn bound_refusal_is_released_into_the_census() {
        let host = TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let failure = host
            .open_bound(root, OsStr::new("absent.sqlite3"), OriginalNativeBoundMode::Existing,
                OriginalNativeDomain::RuntimeMutation)
            .err()
            .expect("absent bound database refused");
        host.retire_open_failure(failure);
        assert_eq!(host.ticket_census(), (1, 1));
    }

    /// Native allocation cost must not grow with the blocks other
    /// connections keep live, and freed blocks must be reused.
    #[test]
    fn allocation_cost_is_independent_of_live_blocks() {
        let host = TestNativeHost::acquire();
        let ddl: String = (0..60)
            .map(|n| format!("CREATE TABLE t{n}(a INTEGER PRIMARY KEY, b TEXT, c BLOB); CREATE INDEX i{n} ON t{n}(b, c);"))
            .collect();
        let steps = || host.factory().budget_snapshot().unwrap().allocator_steps;
        let mut held = Vec::new();
        let mut costs = Vec::new();
        for _ in 0..6 {
            let before = steps();
            let connection = host.open_transient();
            connection.with_sql(|sql| Ok(sql.execute_batch(&ddl)?)).unwrap();
            costs.push(steps() - before);
            held.push(connection);
        }
        assert!(costs.iter().all(|cost| *cost <= costs[0] + costs[0] / 4), "{costs:?}");
        let churn = held.pop().unwrap();
        for _ in 0..50 {
            churn
                .with_sql(|sql| {
                    Ok(sql.execute_batch(
                        "CREATE TABLE scratch(x BLOB); INSERT INTO scratch VALUES(zeroblob(65536)); DROP TABLE scratch;",
                    )?)
                })
                .unwrap();
        }
        held.push(churn);
        for connection in held {
            host.close(connection);
        }
    }

    /// With no larger free bin, a request that fits none of its own bin's
    /// fragments must be refused after a bounded probe, not after walking
    /// every fragment in that bin.
    #[test]
    fn own_bin_walk_is_bounded_without_a_larger_bin() {
        // Request sizes whose native spans are 512 (fragment) and 768 (probe
        // request), both in the 512..1024 bin, plus 256-byte separators.
        const FRAGMENT: i32 = 400;
        const SEPARATOR: i32 = 1;
        const PROBE: i32 = 700;
        let host = TestNativeHost::acquire();
        let warm = host.open_transient();
        host.close(warm);
        let steps = || host.factory().budget_snapshot().unwrap().allocator_steps;
        let (fragments, cost, probe) = host.scope(|| {
            let malloc = |n: i32| unsafe { libsqlite3_sys::sqlite3_malloc(n) };
            let mut fragments = Vec::new();
            let mut separators = Vec::new();
            // Exhaust every block that could serve a 512 span, leaving each
            // fragment between live separators so freeing cannot coalesce.
            loop {
                let fragment = malloc(FRAGMENT);
                if fragment.is_null() {
                    break;
                }
                let separator = malloc(SEPARATOR);
                if separator.is_null() {
                    // The last fragment has no live right neighbour; keep it
                    // live so it cannot coalesce.
                    separators.push(fragment);
                    break;
                }
                fragments.push(fragment);
                separators.push(separator);
            }
            loop {
                let separator = malloc(SEPARATOR);
                if separator.is_null() {
                    break;
                }
                separators.push(separator);
            }
            let count = fragments.len();
            for fragment in fragments.drain(..) {
                unsafe { libsqlite3_sys::sqlite3_free(fragment) };
            }
            let before = steps();
            let probe = malloc(PROBE);
            let cost = steps() - before;
            if !probe.is_null() {
                unsafe { libsqlite3_sys::sqlite3_free(probe) };
            }
            for separator in separators {
                unsafe { libsqlite3_sys::sqlite3_free(separator) };
            }
            (count, cost, probe.is_null())
        });
        assert!(fragments >= 256, "too few fragments to show a bound: {fragments}");
        assert!(probe, "no fragment can serve the probe request");
        // One entry charge plus at most the bounded own-bin probes.
        assert!(cost <= 9, "own-bin walk cost {cost} with {fragments} fragments");
    }

    /// The bound VFS has no rollback-journal role, so a database the native
    /// factory creates must already be in WAL when it is handed over, and
    /// ordinary writes must then succeed.
    #[test]
    fn created_bound_database_is_writable_in_wal() {
        for mode in [OriginalNativeBoundMode::CreateExclusive, OriginalNativeBoundMode::OpenOrCreate] {
            let host = TestNativeHost::acquire();
            let tmp = tempfile::tempdir().unwrap();
            let root = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
            let connection = match host.open_bound(root, OsStr::new("runtime.sqlite3"), mode,
                OriginalNativeDomain::NodeRuntime) {
                Ok(connection) => connection,
                Err(failure) => {
                    let error = failure.to_string();
                    host.retire_open_failure(failure);
                    panic!("{mode:?} bound create refused: {error}");
                }
            };
            let (journal, value) = connection
                .with_sql(|sql| {
                    sql.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES(7);")?;
                    let journal: String = sql.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                    let value: i64 = sql.query_row("SELECT x FROM t", [], |row| row.get(0))?;
                    Ok((journal, value))
                })
                .unwrap();
            assert_eq!((journal.as_str(), value), ("wal", 7), "{mode:?}");
            assert!(!tmp.path().join("runtime.sqlite3-journal").exists());
            host.close(connection);
        }
    }

    /// A create interrupted before its first WAL commit leaves a zero-byte
    /// main. Reopened as Existing it must still enter WAL and take writes.
    #[test]
    fn interrupted_create_leaves_an_existing_main_writable_in_wal() {
        let host = TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let created = match host.open_bound(Arc::clone(&root), OsStr::new("runtime.sqlite3"),
            OriginalNativeBoundMode::CreateExclusive, OriginalNativeDomain::NodeRuntime) {
            Ok(connection) => connection,
            Err(failure) => {
                let error = failure.to_string();
                host.retire_open_failure(failure);
                panic!("bound create refused: {error}");
            }
        };
        host.close(created);
        // The completed create staged its header; replay the crash window by
        // leaving the exclusively created main empty again.
        std::fs::remove_file(tmp.path().join("runtime.sqlite3")).unwrap();
        std::fs::File::create(tmp.path().join("runtime.sqlite3")).unwrap();
        for sidecar in ["runtime.sqlite3-wal", "runtime.sqlite3-shm"] {
            let _ = std::fs::remove_file(tmp.path().join(sidecar));
        }
        let connection = match host.open_bound(root, OsStr::new("runtime.sqlite3"),
            OriginalNativeBoundMode::Existing, OriginalNativeDomain::NodeRuntime) {
            Ok(connection) => connection,
            Err(failure) => {
                let error = failure.to_string();
                host.retire_open_failure(failure);
                panic!("empty existing bound open refused: {error}");
            }
        };
        let (journal, value) = connection
            .with_sql(|sql| {
                sql.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES(9);")?;
                let journal: String = sql.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                let value: i64 = sql.query_row("SELECT x FROM t", [], |row| row.get(0))?;
                Ok((journal, value))
            })
            .unwrap();
        assert_eq!((journal.as_str(), value), ("wal", 9));
        assert!(!tmp.path().join("runtime.sqlite3-journal").exists());
        host.close(connection);
    }

    /// A generated projection's closed-main durability gate consults the
    /// native lease census; a held generation lease must not read as an
    /// alias of the projection it guards.
    struct AdmittedLease;
    impl crate::OriginalNativeProjectionLeaseAuthority for AdmittedLease {
        fn require_original(&self, _factory: &Arc<OriginalNativeFactory>, _cleanup: bool) -> Result<()> {
            Ok(())
        }
        fn fork_deadline(&self, _factory: &Arc<OriginalNativeFactory>, _cleanup: bool)
            -> Result<lillux::time::MonotonicDeadline> {
            Ok(lillux::time::MonotonicDeadline::after(Duration::from_secs(60)))
        }
    }

    #[test]
    fn generated_bound_durability_succeeds_while_a_projection_lease_is_held() {
        let host = TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        std::fs::File::create(tmp.path().join("generated.sqlite3.lease")).unwrap();
        let metadata = root.open_mount_entry(OsStr::new("generated.sqlite3.lease")).unwrap().unwrap();
        let authority: Arc<dyn crate::OriginalNativeProjectionLeaseAuthority> = Arc::new(AdmittedLease);
        // SAFETY: test-only lease under this host's own funded factory; the
        // metadata inode is a fresh regular file no SQLite role uses.
        let inputs = unsafe {
            crate::OriginalProjectionLeaseInputs::issued(
                Arc::clone(&root), metadata, crate::OriginalProjectionLeaseMode::Exclusive, authority,
            )
        };
        let lease = match host.factory().acquire_projection_lease(inputs) {
            Ok(lease) => lease,
            Err(failure) => panic!("test lease refused: {failure}"),
        };
        let connection = match host.open_bound(root, OsStr::new("generated.sqlite3"),
            OriginalNativeBoundMode::CreateExclusive, OriginalNativeDomain::StateGeneratedProjection) {
            Ok(connection) => connection,
            Err(failure) => {
                let error = failure.to_string();
                host.retire_open_failure(failure);
                panic!("generated bound create refused: {error}");
            }
        };
        connection
            .with_sql(|sql| {
                sql.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES(1);")?;
                let (busy, frames, checkpointed): (i64, i64, i64) = sql
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                ensure!(busy == 0 && frames == checkpointed, "checkpoint incomplete");
                Ok(())
            })
            .unwrap();
        let closed = match connection.close() {
            Ok(closed) => closed,
            Err(failure) => panic!("generated close retained: {failure}"),
        };
        let synced = closed.sync_generated_bound().map(|_| ()).map_err(|failure| failure.to_string());
        host.retain_terminal(OriginalNativeConnectionTerminal::Closed(closed));
        match lease.close() {
            Ok(crate::OriginalProjectionLeaseDisposition::Closed(closed)) => host.retain_lease(closed),
            Ok(_) => panic!("held lease closed as never acquired"),
            Err(failure) => panic!("test lease close retained: {failure}"),
        }
        synced.unwrap();
    }
    /// A guarded evidence read under a held projection lease succeeds only
    /// inside the owning factory's native scope. Unscoped, the lease census
    /// has no owner to answer for and the read refuses; scoped, a lease on
    /// the read inode itself still refuses.
    #[test]
    fn guarded_read_succeeds_under_a_held_projection_lease_only_when_scoped() {
        let host = TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        std::fs::File::create(tmp.path().join("generated.sqlite3.lease")).unwrap();
        std::fs::write(tmp.path().join("generation.json"), b"{}").unwrap();
        let metadata = root.open_mount_entry(OsStr::new("generated.sqlite3.lease")).unwrap().unwrap();
        let authority: Arc<dyn crate::OriginalNativeProjectionLeaseAuthority> = Arc::new(AdmittedLease);
        // SAFETY: test-only lease under this host's own funded factory; the
        // metadata inode is a fresh regular file no SQLite role uses.
        let inputs = unsafe {
            crate::OriginalProjectionLeaseInputs::issued(
                Arc::clone(&root), metadata, crate::OriginalProjectionLeaseMode::Exclusive, authority,
            )
        };
        let lease = match host.factory().acquire_projection_lease(inputs) {
            Ok(lease) => lease,
            Err(failure) => panic!("test lease refused: {failure}"),
        };
        let unscoped = crate::read_regular_without_sqlite_alias(&root, OsStr::new("generation.json"), 64)
            .map_err(|error| error.to_string());
        let scoped = host.factory()
            .read_regular_without_sqlite_alias(&root, OsStr::new("generation.json"), 64)
            .map_err(|error| error.to_string());
        let scoped_leased = host.factory()
            .read_regular_without_sqlite_alias(&root, OsStr::new("generated.sqlite3.lease"), 64)
            .map_err(|error| error.to_string());
        match lease.close() {
            Ok(crate::OriginalProjectionLeaseDisposition::Closed(closed)) => host.retain_lease(closed),
            Ok(_) => panic!("held lease closed as never acquired"),
            Err(failure) => panic!("test lease close retained: {failure}"),
        }
        let refused = unscoped.expect_err("unscoped guarded read must refuse under a held lease");
        assert!(refused.contains("refused (5)"), "{refused}");
        assert_eq!(scoped.unwrap(), Some(b"{}".to_vec()));
        let refused = scoped_leased.expect_err("scoped read of the leased inode must refuse");
        assert!(refused.contains("refused (5)"), "{refused}");
        assert_eq!(
            crate::read_regular_without_sqlite_alias(&root, OsStr::new("generation.json"), 64).unwrap(),
            Some(b"{}".to_vec())
        );
    }
}
