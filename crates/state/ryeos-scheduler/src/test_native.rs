//! Scheduler stores for this crate's tests, opened on the thread's shared
//! native test host the way a live node opens one: the scheduler's own
//! closed schema reference, then the fixed `scheduler.sqlite3` coordinate
//! on one bound ticket. No stock connection is opened. Dropping a store
//! closes it and hands every terminal to the host's census; the last handle
//! on the thread releases the host.
use std::ffi::OsStr;
use std::ops::Deref;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use lillux_sqlite::test_host::TestNativeHost;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeBoundMode, OriginalNativeConnection,
    OriginalNativeDomain, OriginalNativeFactory, OriginalNativeTransientInputs,
    OriginalSchedulerRetirementAuthority, OriginalSchedulerRetirementEffect,
    OriginalSchedulerRetirementInputs,
};
use ryeos_state::test_support::{release_shared_test_host, shared_test_host};

use crate::db::{SchedulerDb, SchedulerDbOpenFailure};

/// Open `scheduler.sqlite3` under `runtime` in `mode`, on its own schema
/// reference paid by the host's factory. A refused reference cannot reach
/// a store, so it fails the test.
fn open_on_host(
    host: &TestNativeHost,
    runtime: Arc<lillux::PinnedDirectory>,
    mode: OriginalNativeBoundMode,
) -> std::result::Result<SchedulerDb, SchedulerDbOpenFailure> {
    // SAFETY: test-only issuance under the shared host's own funded factory.
    let reference_inputs = unsafe {
        OriginalNativeTransientInputs::issued(
            OriginalNativeDomain::NodeSchedulerSchemaReference,
            host.destination(),
        )
    }
    .expect("test scheduler schema reference inputs");
    let reference = match SchedulerDb::prepare_scheduler_schema_reference(
        Arc::clone(host.factory()),
        reference_inputs,
    ) {
        Ok(reference) => reference,
        Err(failure) => panic!("test scheduler schema reference refused: {failure}"),
    };
    // SAFETY: test-only issuance under the shared host's own funded factory.
    let inputs = unsafe {
        OriginalNativeBoundInputs::issued(
            runtime,
            OsStr::new("scheduler.sqlite3"),
            mode,
            OriginalNativeDomain::NodeScheduler,
            host.destination(),
            None,
        )
    }
    .expect("test scheduler inputs");
    SchedulerDb::open_original_scheduler(Arc::clone(host.factory()), inputs, reference)
}

/// Close a store the test has ended, file its terminals in the census, and
/// end the store's own aliases of the host's factory so the host can release.
fn settle_on_host(host: &TestNativeHost, db: SchedulerDb) -> Result<()> {
    db.close().map_err(|error| anyhow::anyhow!("{error}"))?;
    for terminal in db.retained_native_terminals()? {
        host.retain_terminal(terminal);
    }
    db.release_settled_into_census()
        .map_err(|(_db, error)| error)
}

/// Settle the one remaining handle of a shared store.
fn settle_shared_on_host(host: &TestNativeHost, db: Arc<SchedulerDb>) -> Result<()> {
    let db = Arc::try_unwrap(db).map_err(|db| {
        anyhow::anyhow!(
            "test scheduler store still has {} other aliases",
            Arc::strong_count(&db) - 1
        )
    })?;
    settle_on_host(host, db)
}

/// Close a refused opening through its same owner and file its terminals.
fn settle_refusal_on_host(host: &TestNativeHost, failure: SchedulerDbOpenFailure) -> Result<()> {
    let settled = failure
        .retry_close()
        .map_err(|failure| anyhow::anyhow!("refused scheduler open did not close: {failure}"))?;
    for terminal in settled.retained_native_terminals()? {
        host.retain_terminal(terminal);
    }
    settled
        .release_settled_for_test()
        .map_err(|(_failure, error)| error)
}

/// Open `directory/scheduler.sqlite3`, creating it if absent, on a host the
/// caller already holds, such as the one under a test's open node Store. The
/// store goes back through [`settle_on`] with that same host.
pub fn open_at_on(
    host: &TestNativeHost,
    directory: &Path,
) -> std::result::Result<SchedulerDb, SchedulerDbOpenFailure> {
    let runtime = Arc::new(
        lillux::PinnedDirectory::open(directory)
            .expect("pin test scheduler runtime directory")
            .expect("test scheduler runtime directory present"),
    );
    open_on_host(host, runtime, OriginalNativeBoundMode::OpenOrCreate)
}

/// Close a store opened by [`open_at_on`], once every other alias of it has
/// gone, and file its terminals in the census of the host it was opened on.
pub fn settle_on(host: &TestNativeHost, db: Arc<SchedulerDb>) {
    if let Err(error) = settle_shared_on_host(host, db) {
        panic!("test scheduler store did not settle into the census: {error:#}");
    }
}

/// Close a refused [`open_at_on`] and file its terminals in the census of the
/// host it was opened on.
pub fn settle_refusal_on(host: &TestNativeHost, failure: SchedulerDbOpenFailure) {
    if let Err(error) = settle_refusal_on_host(host, failure) {
        panic!("refused test scheduler open did not settle into the census: {error:#}");
    }
}

/// Retirement authority for tests: it admits every effect of the one
/// generation it was issued for and funds nothing beyond the test host.
struct TestRetirementAuthority {
    generation: [u8; 16],
}

// SAFETY: test-only; the test host funds every native effect, and this
// authority only checks that each effect names its issued generation.
unsafe impl OriginalSchedulerRetirementAuthority for TestRetirementAuthority {
    fn require_original(
        &self,
        _factory: &Arc<OriginalNativeFactory>,
        generation: [u8; 16],
        _cleanup: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            generation == self.generation,
            "test retirement names another generation"
        );
        Ok(())
    }
    fn prepay_effect(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        generation: [u8; 16],
        _effect: OriginalSchedulerRetirementEffect,
        cleanup: bool,
    ) -> Result<()> {
        self.require_original(factory, generation, cleanup)
    }
}

/// One thread's handle on the shared test host, for tests that open stores
/// at a directory they choose, reopen them, or keep a refused opening. Each
/// store or refusal the test ends goes back through [`Self::settle`] or
/// [`Self::settle_refusal`] so the host's census stays complete; dropping
/// the handle gives the host back.
pub struct TestSchedulerHost {
    host: Option<Rc<TestNativeHost>>,
}

impl TestSchedulerHost {
    pub fn acquire() -> Self {
        Self {
            host: Some(shared_test_host()),
        }
    }

    fn host(&self) -> &TestNativeHost {
        self.host.as_ref().expect("live test scheduler host")
    }

    /// Open `directory/scheduler.sqlite3`, creating it if absent.
    pub fn open_at(
        &self,
        directory: &Path,
    ) -> std::result::Result<SchedulerDb, SchedulerDbOpenFailure> {
        let runtime = Arc::new(
            lillux::PinnedDirectory::open(directory)
                .expect("pin test scheduler runtime directory")
                .expect("test scheduler runtime directory present"),
        );
        open_on_host(self.host(), runtime, OriginalNativeBoundMode::OpenOrCreate)
    }

    /// Run `work` on the database `name` beneath `directory` through one
    /// bound native ticket of this host, then close it into the census: how
    /// a test seeds or alters a store's files without the scheduler's own
    /// schema checks. A missing database is created only when `create`.
    pub fn with_fixture_sql<T>(
        &self,
        directory: &Path,
        name: &str,
        create: bool,
        work: impl for<'c> FnOnce(&'c rusqlite::Connection) -> Result<T>,
    ) -> T {
        let connection = self.open_fixture(directory, name, create);
        let result = connection.with_sql(work);
        self.host().close(connection);
        result.unwrap_or_else(|error| panic!("test fixture SQL refused: {error:#}"))
    }

    /// Open the existing database `name` beneath `directory` as a separate
    /// native peer the test holds across scheduler work; it goes back
    /// through [`Self::close_fixture`].
    pub fn open_fixture_peer(&self, directory: &Path, name: &str) -> OriginalNativeConnection {
        self.open_fixture(directory, name, false)
    }

    /// Close a peer from [`Self::open_fixture_peer`] into the census.
    pub fn close_fixture(&self, connection: OriginalNativeConnection) {
        self.host().close(connection);
    }

    fn open_fixture(&self, directory: &Path, name: &str, create: bool) -> OriginalNativeConnection {
        let runtime = Arc::new(
            lillux::PinnedDirectory::open(directory)
                .expect("pin test fixture directory")
                .expect("test fixture directory present"),
        );
        let mode = if create {
            OriginalNativeBoundMode::CreateExclusive
        } else {
            OriginalNativeBoundMode::Existing
        };
        match self.host().open_bound(
            runtime,
            OsStr::new(name),
            mode,
            OriginalNativeDomain::NodeScheduler,
        ) {
            Ok(connection) => connection,
            Err(failure) => {
                let error = failure.to_string();
                self.host().retire_open_failure(failure);
                panic!("test fixture open refused: {error}");
            }
        }
    }

    /// Native work that holds no ticket of its own, such as a stopped
    /// store's observations and role mutations, in an allocator scope of
    /// this host's arena.
    pub fn scope<T>(&self, work: impl FnOnce() -> T) -> T {
        self.host().scope(work)
    }

    /// Stopped intake in a cleanup scope of this host's arena, the way a
    /// node runs it inside its original cleanup scope.
    pub fn cleanup_scope<T>(&self, work: impl FnOnce() -> T) -> T {
        self.host().cleanup_scope(work)
    }

    /// This host's native factory, for tests that enter its production
    /// stopped-intake and service scopes.
    pub fn factory(&self) -> &Arc<OriginalNativeFactory> {
        self.host().factory()
    }

    /// Retirement inputs for `generation`, admitted by a test authority.
    pub fn retirement_inputs(&self, generation: [u8; 16]) -> OriginalSchedulerRetirementInputs {
        // SAFETY: test-only issuance; the authority is funded by the test host.
        unsafe {
            OriginalSchedulerRetirementInputs::issued(
                generation,
                Arc::new(TestRetirementAuthority { generation }),
            )
        }
    }

    /// Close a store the test has ended and file its terminals in the census.
    pub fn settle(&self, db: SchedulerDb) {
        if let Err(error) = settle_on_host(self.host(), db) {
            panic!("test scheduler store did not settle into the census: {error:#}");
        }
    }

    /// Settle a shared store once every other alias of it has gone.
    pub fn settle_shared(&self, db: Arc<SchedulerDb>) {
        if let Err(error) = settle_shared_on_host(self.host(), db) {
            panic!("test scheduler store did not settle into the census: {error:#}");
        }
    }

    /// Close a refused opening and file its terminals in the census.
    pub fn settle_refusal(&self, failure: SchedulerDbOpenFailure) {
        if let Err(error) = settle_refusal_on_host(self.host(), failure) {
            panic!("refused test scheduler open did not settle into the census: {error:#}");
        }
    }
}

impl Drop for TestSchedulerHost {
    fn drop(&mut self) {
        if let Some(host) = self.host.take() {
            release_shared_test_host(host);
        }
    }
}

/// One test scheduler store in its own temporary runtime directory. It holds
/// the shared host, so it stays on its opening thread; hand
/// [`Self::shared`] to code that needs a `Send` store.
pub struct TestSchedulerDb {
    // Field order is drop order: the store closes before the host releases
    // and before its directory is removed.
    db: Option<Arc<SchedulerDb>>,
    host: Option<Rc<TestNativeHost>>,
    _root: tempfile::TempDir,
}

impl TestSchedulerDb {
    /// A store whose fire projection is published current, as a node's is
    /// after its startup replay.
    pub fn open() -> Self {
        let store = Self::open_fresh();
        store
            .finish_fire_projection_rebuild()
            .expect("publish the fresh test fire projection");
        store
    }

    /// A store as first created: its fire projection awaits a rebuild.
    pub fn open_fresh() -> Self {
        let root = tempfile::tempdir().expect("test scheduler runtime directory");
        let runtime = Arc::new(
            lillux::PinnedDirectory::open(root.path())
                .expect("pin test scheduler runtime directory")
                .expect("test scheduler runtime directory present"),
        );
        let host = shared_test_host();
        match open_on_host(&host, runtime, OriginalNativeBoundMode::CreateExclusive) {
            Ok(db) => Self {
                db: Some(Arc::new(db)),
                host: Some(host),
                _root: root,
            },
            Err(failure) => {
                let error = failure.to_string();
                // The refused opening settles into the census through its
                // same owner, so the host still releases its arena.
                let _ = settle_refusal_on_host(&host, failure);
                release_shared_test_host(host);
                panic!("test scheduler open refused: {error}");
            }
        }
    }

    /// The store itself, for code that holds it across threads or tasks.
    /// The store closes when this owner drops, whoever else still holds it.
    pub fn shared(&self) -> Arc<SchedulerDb> {
        Arc::clone(self.db.as_ref().expect("live test scheduler store"))
    }
}

impl Deref for TestSchedulerDb {
    type Target = SchedulerDb;
    fn deref(&self) -> &SchedulerDb {
        self.db.as_ref().expect("live test scheduler store")
    }
}

impl Drop for TestSchedulerDb {
    fn drop(&mut self) {
        let (Some(db), Some(host)) = (self.db.take(), self.host.take()) else {
            return;
        };
        if let Err(error) = settle_shared_on_host(&host, db)
            && !std::thread::panicking()
        {
            panic!("test scheduler store did not settle into the census: {error:#}");
        }
        release_shared_test_host(host);
    }
}
