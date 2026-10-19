//! Test seeding and opening of a node's State through the native gate.
//! Every SQLite file is opened on a ticket of a [`TestNativeHost`] and closed
//! into its census, with the same schema SQL and constructors a live node
//! runs; no stock connection is opened.
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use lillux_sqlite::test_host::TestNativeHost;
use lillux_sqlite::{OriginalNativeBoundMode, OriginalNativeDomain};

use crate::operational::{OPERATIONAL_DB_FILENAME, configure_connection, operational_schema_spec};
use crate::refs::TrustStore;
use crate::state_db::{RuntimeLivenessInspector, StateDb};

/// Establish the State roots a live node creates under `runtime_state_dir`
/// and its current-schema Operational database with the initialization
/// marker, built on `host`. Every ticket reaches the host's census.
pub fn seed_node_state(host: &TestNativeHost, runtime_state_dir: &Path) -> Result<()> {
    let runtime = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
        .context("establish test runtime state root")?;
    for (name, mode) in [("objects", 0o777), ("refs", 0o777), ("locators", 0o777)] {
        runtime.open_or_create_child(OsStr::new(name), mode)?;
    }
    runtime
        .open_or_create_child(OsStr::new("recovery"), 0o700)?
        .open_or_create_child(OsStr::new("thread-projection"), 0o700)?;
    crate::recovery::CasMutationGuard::ensure_anchor(runtime_state_dir)?;
    crate::gc::GcLock::ensure_anchor(runtime_state_dir)?;
    let root = Arc::new(runtime);
    let path = runtime_state_dir.join(OPERATIONAL_DB_FILENAME);
    let connection = match host.open_bound(
        Arc::clone(&root),
        OsStr::new(OPERATIONAL_DB_FILENAME),
        OriginalNativeBoundMode::CreateExclusive,
        OriginalNativeDomain::NodeOperational,
    ) {
        Ok(connection) => connection,
        Err(failure) => {
            let error = anyhow::anyhow!("test Operational open refused: {failure}");
            host.retire_open_failure(failure);
            return Err(error);
        }
    };
    let initialized = connection.with_sql(|sql| {
        configure_connection(sql)?;
        crate::sqlite_schema::init_owned(
            sql,
            &operational_schema_spec(),
            crate::operational::SCHEMA_SQL,
            &path,
        )?;
        let mode: String = sql.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        anyhow::ensure!(
            mode == "wal",
            "seeded Operational journal mode is {mode}, not WAL"
        );
        Ok(())
    });
    host.close(connection);
    initialized?;
    crate::operational::write_initialized_marker(&root)?;
    Ok(())
}

/// Lease authority of a test host: admits generation leases of that host's
/// own factory only, all against one deadline fixed when the State opens.
struct TestLeaseAuthority {
    // Weak: the host's release counts every strong factory alias, and this
    // authority can outlive the store inside retained lease owners.
    factory: std::sync::Weak<lillux_sqlite::OriginalNativeFactory>,
    deadline: lillux::time::MonotonicDeadline,
}
impl TestLeaseAuthority {
    fn require_host_factory(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<()> {
        anyhow::ensure!(
            std::sync::Weak::ptr_eq(&self.factory, &Arc::downgrade(factory)),
            "test lease authority refuses a foreign factory"
        );
        Ok(())
    }
}
impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for TestLeaseAuthority {
    fn require_original(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        _cleanup: bool,
    ) -> Result<()> {
        self.require_host_factory(factory)
    }
    fn fork_deadline(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        _cleanup: bool,
    ) -> Result<lillux::time::MonotonicDeadline> {
        self.require_host_factory(factory)?;
        Ok(self.deadline)
    }
}

/// Projection repair requests of a test State are surfaced by the writes
/// that raise them, so the sink keeps none.
struct TestRepairSink;
impl crate::ProjectionRepairSink for TestRepairSink {
    fn request_repair(&self, _request: crate::ProjectionRepairRequest) {}
}

/// A test node has no Runtime store, so no chain has live recovery state.
struct NoLiveRuntime;
impl RuntimeLivenessInspector for NoLiveRuntime {
    fn chain_has_live_recovery_state(&self, _chain_root_id: &str) -> Result<bool> {
        Ok(false)
    }
}

const OPEN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Open, creating it if absent, the node State under `runtime_state_dir` the
/// way a live node opens it, on `host`: the namespace pinned and locked, the
/// Operational store and the persistent projection on the host's tickets.
/// Close it with [`retire_live_state`]. A refused open keeps its partial
/// custody inside the returned error; settle it with [`settle_refused_open`].
pub fn open_live_state(
    host: &TestNativeHost,
    runtime_state_dir: &Path,
    trust: Arc<TrustStore>,
) -> Result<StateDb> {
    open_live_state_with_repair_sink(host, runtime_state_dir, trust, Arc::new(TestRepairSink))
}

/// [`open_live_state`] with the caller's projection repair sink.
pub fn open_live_state_with_repair_sink(
    host: &TestNativeHost,
    runtime_state_dir: &Path,
    trust: Arc<TrustStore>,
    repair_sink: Arc<dyn crate::ProjectionRepairSink>,
) -> Result<StateDb> {
    let directory = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
        .context("establish test runtime state root")?;
    // A live node establishes both lock anchors before its State opens.
    crate::recovery::CasMutationGuard::ensure_anchor(runtime_state_dir)?;
    crate::gc::GcLock::ensure_anchor(runtime_state_dir)?;
    let namespace = directory
        .lock_exclusive()
        .context("lock test runtime state namespace")?;
    let deadline = lillux::time::MonotonicDeadline::after(OPEN_DEADLINE);
    // SAFETY: the test host is the sole issuer of this factory under the
    // process-wide test serial guard; the namespace lock taken above is held
    // by the State for its life, and the lease authority admits only leases
    // of that same factory.
    unsafe {
        StateDb::open_live_with_recovery_runtime_liveness_and_namespace_authority(
            runtime_state_dir,
            directory,
            namespace,
            repair_sink,
            trust,
            None,
            &NoLiveRuntime,
            Arc::clone(host.factory()),
            host.destination(),
            Arc::new(TestLeaseAuthority {
                factory: Arc::downgrade(host.factory()),
                deadline,
            }),
            deadline,
        )
    }
}

/// Close a State opened by [`open_live_state`] into `host`'s census. A
/// refused close keeps its custody inside the returned error.
pub fn retire_live_state(host: &TestNativeHost, state: StateDb) -> Result<()> {
    let guard = state.pinned_authority()?.acquire_exclusive_guard(false)?;
    let census = state
        .close_live_census(Arc::clone(host.factory()), &guard)
        .map_err(anyhow::Error::new)?;
    drop(guard);
    let crate::state_db::LiveStateCensus {
        terminals,
        leases,
        lease_retirement,
        prior_refusal,
    } = census;
    retain_census(host, terminals, leases, lease_retirement);
    match prior_refusal {
        Some(refusal) => {
            Err(refusal.context("test State close completed after an earlier refusal"))
        }
        None => Ok(()),
    }
}

/// Record a closed State's terminals and leases in `host`'s census, and
/// retire its generation leases once the host finishes its backend.
fn retain_census(
    host: &TestNativeHost,
    terminals: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    leases: Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>,
    lease_retirement: crate::state_db::GenerationLeaseRetirement,
) {
    for terminal in terminals {
        host.retain_terminal(terminal);
    }
    for lease in leases {
        host.retain_lease(lease);
    }
    host.after_backend(Box::new(move |backend| {
        lease_retirement.retire_after_backend(backend)
    }));
}

/// Settle a refused live State opening into `host`'s census and return its
/// refusal text. A refused State opening and a refused Operational opening
/// beneath it each settle through `close_retained`, then their failed-start
/// native terminals and generation-lease terminals join the census. A refusal
/// raised before any native owner existed carries no custody.
pub fn settle_refused_open(host: &TestNativeHost, error: anyhow::Error) -> Result<String> {
    let message = format!("{error:#}");
    let error = match error.downcast::<crate::state_db::LiveStateDatabaseOpenFailure>() {
        Ok(failure) => {
            let released = failure.close_retained().map_err(|failure| {
                anyhow::anyhow!("refused live State close retained: {failure}")
            })?;
            let census = match released.into_census(host.factory()) {
                Ok(census) => census,
                Err((_released, error)) => return Err(error.context("refused live State census")),
            };
            // The prior refusal is the open refusal reported as `message`.
            let crate::state_db::LiveStateCensus {
                terminals,
                leases,
                lease_retirement,
                prior_refusal: _,
            } = census;
            retain_census(host, terminals, leases, lease_retirement);
            return Ok(message);
        }
        Err(error) => error,
    };
    if let Ok(failure) = error.downcast::<crate::operational::LiveOperationalOpenFailure>() {
        let released = failure.close_retained().map_err(|failure| {
            anyhow::anyhow!("refused live Operational close retained: {failure}")
        })?;
        let terminals = match released.into_census(host.factory()) {
            Ok(terminals) => terminals,
            Err((_released, error)) => return Err(error.context("refused live Operational census")),
        };
        for terminal in terminals {
            host.retain_terminal(terminal);
        }
    }
    Ok(message)
}

/// The refusal of a live open of the node State under `runtime_state_dir` on
/// `host`; the refused opening settles into the census. Panics if the open is
/// admitted, after closing the admitted State into the census.
pub fn live_state_open_refusal(
    host: &TestNativeHost,
    runtime_state_dir: &Path,
    trust: Arc<TrustStore>,
) -> String {
    match open_live_state(host, runtime_state_dir, trust) {
        Ok(state) => {
            retire_live_state(host, state).expect("close the admitted test State");
            panic!(
                "live State open of {} was admitted",
                runtime_state_dir.display()
            );
        }
        Err(error) => {
            settle_refused_open(host, error).expect("settle the refused live State opening")
        }
    }
}

/// Envelope of the shared per-thread test host: room for the several
/// stores (States, Runtime stores) a test keeps live at once on one arena.
pub const TEST_STATE_ENVELOPE: lillux_sqlite::test_host::TestNativeEnvelope =
    lillux_sqlite::test_host::TestNativeEnvelope {
        storage_bytes: 128 * 1024 * 1024,
        allocator_steps: 4_000_000_000,
        tickets: 64,
    };

thread_local! {
    static SHARED_TEST_HOST: std::cell::RefCell<std::rc::Weak<TestNativeHost>> =
        const { std::cell::RefCell::new(std::rc::Weak::new()) };
}

/// The test host shared by every store live on this thread, acquired by the
/// first. Stores of different crates (State, Runtime) share it, so a test can
/// hold several at once without a second host waiting on the process-wide
/// serial guard. Return it with [`release_shared_test_host`].
pub fn shared_test_host() -> std::rc::Rc<TestNativeHost> {
    SHARED_TEST_HOST.with(|slot| {
        if let Some(host) = slot.borrow().upgrade() {
            return host;
        }
        let host = std::rc::Rc::new(TestNativeHost::acquire_with(TEST_STATE_ENVELOPE));
        *slot.borrow_mut() = std::rc::Rc::downgrade(&host);
        host
    })
}

/// Give back one handle of the shared host; the last handle on the thread
/// releases it through its census.
pub fn release_shared_test_host(host: std::rc::Rc<TestNativeHost>) {
    if let Ok(host) = std::rc::Rc::try_unwrap(host) {
        drop(host.release());
    }
}

/// A node State opened the way a live node opens it, on this thread's shared
/// test host. It dereferences to the store; dropping it closes the store into
/// the host's census, and the last store on the thread releases the host.
/// The host holds the process-wide test serial guard, so a [`TestState`] is
/// used from its opening thread only.
pub struct TestState {
    // Field order is drop order: the store closes before the host releases.
    state: Option<StateDb>,
    host: Option<std::rc::Rc<TestNativeHost>>,
}

impl TestState {
    /// Open, creating it if absent, the node State under `runtime_state_dir`.
    pub fn open(runtime_state_dir: &Path, trust: Arc<TrustStore>) -> Result<Self> {
        let host = shared_test_host();
        match open_live_state(&host, runtime_state_dir, trust) {
            Ok(state) => Ok(Self {
                state: Some(state),
                host: Some(host),
            }),
            Err(error) => {
                // A refused opening settles into the census, so the host
                // still releases its arena for the tests after this one.
                let message = settle_refused_open(&host, error)?;
                release_shared_test_host(host);
                anyhow::bail!("test live State open refused: {message}")
            }
        }
    }

    /// [`Self::open`] with the caller's projection repair sink.
    pub fn open_with_repair_sink(
        runtime_state_dir: &Path,
        trust: Arc<TrustStore>,
        repair_sink: Arc<dyn crate::ProjectionRepairSink>,
    ) -> Result<Self> {
        let host = shared_test_host();
        match open_live_state_with_repair_sink(&host, runtime_state_dir, trust, repair_sink) {
            Ok(state) => Ok(Self {
                state: Some(state),
                host: Some(host),
            }),
            Err(error) => {
                let message = settle_refused_open(&host, error)?;
                release_shared_test_host(host);
                anyhow::bail!("test live State open refused: {message}")
            }
        }
    }

    /// The host this State's tickets came from.
    pub fn host(&self) -> &TestNativeHost {
        self.host.as_ref().expect("test State host present")
    }

    /// Close the store into the census and give back the host, reporting a
    /// refused close.
    pub fn close(mut self) -> Result<()> {
        let (Some(state), Some(host)) = (self.state.take(), self.host.take()) else {
            anyhow::bail!("test State already closed");
        };
        retire_live_state(&host, state)?;
        release_shared_test_host(host);
        Ok(())
    }
}

impl std::ops::DerefMut for TestState {
    fn deref_mut(&mut self) -> &mut StateDb {
        self.state.as_mut().expect("test State open")
    }
}

impl std::ops::Deref for TestState {
    type Target = StateDb;
    fn deref(&self) -> &StateDb {
        self.state.as_ref().expect("test State open")
    }
}

impl Drop for TestState {
    fn drop(&mut self) {
        let (Some(state), Some(host)) = (self.state.take(), self.host.take()) else {
            return;
        };
        // A failed test still closes its store into the census so the arena
        // is released for the tests after it; a refused close is left to the
        // host's own leak report rather than a second panic.
        match retire_live_state(&host, state) {
            Ok(()) => release_shared_test_host(host),
            Err(error) if !std::thread::panicking() => {
                panic!("test State close refused: {error:#}")
            }
            Err(_) => drop(host),
        }
    }
}

/// A current-schema projection on a transient native ticket of this
/// thread's shared test host. It dereferences to the store; dropping it
/// closes the store into the host's census. Nothing reaches the filesystem.
pub struct TestProjection {
    // Field order is drop order: the store closes before the host returns.
    db: Option<crate::projection::ProjectionDb>,
    host: Option<std::rc::Rc<TestNativeHost>>,
}

impl TestProjection {
    pub fn open() -> Self {
        let host = shared_test_host();
        // SAFETY: test-only issuance under this host's own funded factory.
        let inputs = unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::StateTransient,
                host.destination(),
            )
        }
        .expect("transient projection inputs");
        let db = match crate::projection::ProjectionDb::open_original_transient(
            Arc::clone(host.factory()),
            inputs,
        ) {
            Ok(db) => db,
            Err(error) => panic!("transient test projection refused: {error:#}"),
        };
        Self {
            db: Some(db),
            host: Some(host),
        }
    }
}

impl std::ops::Deref for TestProjection {
    type Target = crate::projection::ProjectionDb;
    fn deref(&self) -> &crate::projection::ProjectionDb {
        self.db.as_ref().expect("test projection open")
    }
}

impl std::ops::DerefMut for TestProjection {
    fn deref_mut(&mut self) -> &mut crate::projection::ProjectionDb {
        self.db.as_mut().expect("test projection open")
    }
}

impl Drop for TestProjection {
    fn drop(&mut self) {
        let (Some(db), Some(host)) = (self.db.take(), self.host.take()) else {
            return;
        };
        match db.close_original(host.factory()) {
            Ok(closed) => {
                let (outside, proof) = closed.into_outside_custody();
                // A transient projection has no filesystem custody.
                drop(outside);
                host.retain_terminal(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(
                    proof,
                ));
                release_shared_test_host(host);
            }
            Err(failure) if !std::thread::panicking() => {
                panic!("test projection close refused: {failure}")
            }
            Err(_) => drop(host),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_opens_live_and_reopens_established() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_state_dir = dir.path().join("runtime-state");
        let state = TestState::open(&runtime_state_dir, Arc::new(TrustStore::new())).unwrap();
        assert!(
            state
                .projection()
                .projection_instance_id()
                .unwrap()
                .is_some()
        );
        state.close().unwrap();
        assert!(runtime_state_dir.join(OPERATIONAL_DB_FILENAME).is_file());
        let reopened = TestState::open(&runtime_state_dir, Arc::new(TrustStore::new())).unwrap();
        drop(reopened);
    }

    #[test]
    fn refused_live_open_settles_into_the_census() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_state_dir = dir.path().join("runtime-state");
        TestState::open(&runtime_state_dir, Arc::new(TrustStore::new()))
            .unwrap()
            .close()
            .unwrap();
        std::fs::remove_file(runtime_state_dir.join(OPERATIONAL_DB_FILENAME)).unwrap();
        let host = TestNativeHost::acquire_with(TEST_STATE_ENVELOPE);
        // Read under the host's serial guard, so no other host test can
        // abandon an opening between the two reads.
        let before = crate::state_db::abandoned_live_openings();
        let refusal =
            live_state_open_refusal(&host, &runtime_state_dir, Arc::new(TrustStore::new()));
        assert!(
            refusal.contains("established live Operational database is absent"),
            "{refusal}"
        );
        let (issued, recorded) = host.ticket_census();
        assert_eq!(
            issued as usize, recorded,
            "every issued ticket reaches the census"
        );
        drop(host.release());
        assert_eq!(
            crate::state_db::abandoned_live_openings(),
            before,
            "nothing was abandoned"
        );
    }
}

/// A stopped-node State on this thread's shared test host, opened the way an
/// offline maintenance command opens it: the established authority roots
/// under the caller's runtime namespace lock, with a transient projection on
/// a host ticket. It dereferences to the store; dropping it closes the store
/// into the host's census. The caller holds `namespace` for the life of the
/// store and drops it after this fixture.
pub struct TestOfflineState {
    // Field order is drop order: the store closes before the host releases,
    // and the lease authority outlives both.
    state: Option<StateDb>,
    host: Option<std::rc::Rc<TestNativeHost>>,
    _lease_authority: Option<Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>>,
}

impl TestOfflineState {
    /// Open the authority roots with a transient projection only.
    pub fn open(
        runtime_state_dir: &Path,
        namespace: &lillux::PinnedDirectoryLock,
        trust: Arc<TrustStore>,
    ) -> Result<Self> {
        let directory = lillux::PinnedDirectory::open(runtime_state_dir)?
            .context("test runtime state directory is absent")?;
        let host = shared_test_host();
        // SAFETY: the shared test host funds this factory under the
        // process-wide test serial guard; the caller holds `namespace` for
        // the life of the store, and the host destination admits only its own
        // factory.
        let opened = unsafe {
            StateDb::open_offline_native(
                directory,
                namespace,
                trust,
                host.factory(),
                host.destination(),
            )
        };
        Self::admitted(host, None, opened)
    }

    /// Open the authority roots with a projection build context on the same
    /// host, so the store can rebuild and publish a projection generation.
    pub fn open_for_rebuild(
        runtime_state_dir: &Path,
        namespace: &lillux::PinnedDirectoryLock,
        trust: Arc<TrustStore>,
    ) -> Result<Self> {
        let directory = lillux::PinnedDirectory::open(runtime_state_dir)?
            .context("test runtime state directory is absent")?;
        let host = shared_test_host();
        let lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> =
            Arc::new(TestLeaseAuthority {
                factory: Arc::downgrade(host.factory()),
                deadline: lillux::time::MonotonicDeadline::after(OPEN_DEADLINE),
            });
        // SAFETY: as in `open`; this fixture retains `lease_authority` until
        // after the host is given back, and it admits only leases of the
        // host's own factory.
        let opened = unsafe {
            StateDb::open_offline_native_for_rebuild(
                directory,
                namespace,
                trust,
                host.factory(),
                Arc::clone(&lease_authority),
            )
        };
        Self::admitted(host, Some(lease_authority), opened)
    }

    fn admitted(
        host: std::rc::Rc<TestNativeHost>,
        lease_authority: Option<Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>>,
        opened: Result<StateDb>,
    ) -> Result<Self> {
        match opened {
            Ok(state) => Ok(Self {
                state: Some(state),
                host: Some(host),
                _lease_authority: lease_authority,
            }),
            Err(error) => {
                // The refusal may carry native owners of this host; they stay
                // in the returned error. The host handle is dropped without a
                // release, so the host panics with its leak census if any
                // owner is never settled.
                drop(host);
                Err(error.context("test offline State open refused"))
            }
        }
    }

    /// Close the store into the census and give back the host, reporting a
    /// refused close.
    pub fn close(mut self) -> Result<()> {
        let (Some(state), Some(host)) = (self.state.take(), self.host.take()) else {
            anyhow::bail!("test offline State already closed");
        };
        retire_offline_state(&host, state)?;
        release_shared_test_host(host);
        Ok(())
    }
}

/// Close a store opened by [`TestOfflineState`] into `host`'s census. A
/// refused close keeps the store inside the returned error.
fn retire_offline_state(host: &TestNativeHost, state: StateDb) -> Result<()> {
    let census = match state.close_offline_native(host.factory()) {
        Ok(census) => census,
        Err((Some(state), error)) => {
            return Err(crate::operational::CarriedOwners::carry(error, state));
        }
        Err((None, error)) => return Err(error),
    };
    let crate::state_db::LiveStateCensus {
        terminals,
        leases,
        lease_retirement,
        prior_refusal,
    } = census;
    retain_census(host, terminals, leases, lease_retirement);
    match prior_refusal {
        Some(refusal) => {
            Err(refusal.context("test offline State close completed after an earlier refusal"))
        }
        None => Ok(()),
    }
}

impl std::ops::DerefMut for TestOfflineState {
    fn deref_mut(&mut self) -> &mut StateDb {
        self.state.as_mut().expect("test offline State open")
    }
}

impl std::ops::Deref for TestOfflineState {
    type Target = StateDb;
    fn deref(&self) -> &StateDb {
        self.state.as_ref().expect("test offline State open")
    }
}

impl Drop for TestOfflineState {
    fn drop(&mut self) {
        let (Some(state), Some(host)) = (self.state.take(), self.host.take()) else {
            return;
        };
        match retire_offline_state(&host, state) {
            Ok(()) => release_shared_test_host(host),
            Err(error) if !std::thread::panicking() => {
                panic!("test offline State close refused: {error:#}")
            }
            Err(_) => drop(host),
        }
    }
}

/// Serializes tests that compare the process-wide abandoned-custody counts
/// before and after dropping an owner, so parallel tests of the same kind
/// cannot shift each other's counts.
pub fn abandoned_custody_serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
