//! Test opening of the Runtime store through the native gate, the way a live
//! node opens it: a bound ticket on a [`TestNativeHost`], validated against
//! the native schema reference, and closed into the host's census. Public
//! under the `test-support` feature for other crates' tests.
use super::*;
use lillux_sqlite::test_host::TestNativeHost;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeBoundMode, OriginalNativeConnectionTerminal,
    OriginalNativeDomain, OriginalNativeTransientInputs,
};
use std::sync::Arc;

/// The closed native Runtime schema reference, on its own host ticket.
pub fn runtime_reference(host: &TestNativeHost) -> sqlite_schema::ClosedNativeSchemaSqlReference {
    // SAFETY: test-only issuance under this host's own funded factory.
    let inputs = unsafe {
        OriginalNativeTransientInputs::issued(
            OriginalNativeDomain::NodeRuntimeSchemaReference,
            host.destination(),
        )
    }
    .expect("Runtime reference inputs");
    match sqlite_schema::prepare_native_schema_sql_reference(
        Arc::clone(host.factory()),
        inputs,
        OriginalNativeDomain::NodeRuntimeSchemaReference,
        &runtime_schema_sql(),
    ) {
        Ok(reference) => reference,
        Err(failure) => panic!("Runtime schema reference refused under the gate: {failure}"),
    }
}

/// Bound inputs for the Runtime database named by `path` beneath `root`.
pub fn bound(
    host: &TestNativeHost,
    root: &Arc<lillux::PinnedDirectory>,
    path: &Path,
    mode: OriginalNativeBoundMode,
    domain: OriginalNativeDomain,
) -> OriginalNativeBoundInputs {
    let name = path.file_name().expect("Runtime database basename");
    // SAFETY: test-only issuance under this host's own funded factory.
    unsafe {
        OriginalNativeBoundInputs::issued(Arc::clone(root), name, mode, domain, host.destination(), None)
    }
    .expect("Runtime bound inputs")
}

/// Open, creating it if absent, the current-schema Runtime store at `path`
/// the way a live node does, beneath its pinned and locked parent.
pub fn open_live(host: &TestNativeHost, path: &Path) -> RuntimeDb {
    let parent = path.parent().expect("Runtime database parent");
    let root = Arc::new(lillux::PinnedDirectory::open_or_create(parent).expect("pin Runtime parent"));
    let namespace = root.lock_exclusive().expect("lock Runtime parent");
    match RuntimeDb::open_live_with_namespace_authority(
        path,
        Arc::clone(&root),
        namespace,
        Arc::clone(host.factory()),
        bound(host, &root, path, OriginalNativeBoundMode::OpenOrCreate, OriginalNativeDomain::NodeRuntime),
        lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(60)),
        runtime_reference(host),
    ) {
        Ok(db) => db,
        Err(failure) => {
            // Release the refused acquisition into the census before
            // failing, so the host still releases its arena.
            let message = format!("{failure:#}");
            if let Ok(released) = failure.retry_close() {
                if let Ok(terminals) = released.native_terminals(host.factory()) {
                    for terminal in terminals {
                        host.retain_terminal(terminal);
                    }
                }
            }
            panic!("live Runtime refused under the gate: {message}")
        }
    }
}

/// Close a natively opened Runtime store and hand every terminal to the
/// host's census; returns their domains.
pub fn retire(host: &TestNativeHost, db: RuntimeDb) -> Vec<OriginalNativeDomain> {
    let closed = match db.close_original_runtime(Arc::clone(host.factory())) {
        Ok(closed) => closed,
        Err(failure) => panic!("Runtime close refused: {failure}"),
    };
    let terminals = closed.native_terminals().expect("closed Runtime census");
    let domains = terminals
        .iter()
        .map(|terminal| match terminal {
            OriginalNativeConnectionTerminal::Closed(owner) => owner.domain(),
            OriginalNativeConnectionTerminal::Released(_) => panic!("closed Runtime yields closed terminals"),
        })
        .collect();
    for terminal in terminals {
        host.retain_terminal(terminal);
    }
    drop(closed);
    domains
}

/// Run `work` on the existing Runtime store at `path` through the native
/// gate, on this thread's shared test host, without the live open's
/// validation.
pub fn read_existing<T>(path: &Path, work: impl for<'c> FnOnce(&'c rusqlite::Connection) -> Result<T>) -> T {
    with_bound_path(path, OriginalNativeBoundMode::Existing, work)
}

/// Create the database at `path` through the native gate, on this thread's
/// shared test host, and run `work` on it before it closes: a store some
/// other writer left behind, established without the live open's schema.
pub fn with_created<T>(path: &Path, work: impl for<'c> FnOnce(&'c rusqlite::Connection) -> Result<T>) -> T {
    with_bound_path(path, OriginalNativeBoundMode::CreateExclusive, work)
}

/// Run `work` on one bound ticket for the database at `path`, opened in
/// `mode`. The connection closes into the host's census even when `work`
/// panics, so a failing test leaves no arena behind.
fn with_bound_path<T>(
    path: &Path,
    mode: OriginalNativeBoundMode,
    work: impl for<'c> FnOnce(&'c rusqlite::Connection) -> Result<T>,
) -> T {
    let root = Arc::new(
        lillux::PinnedDirectory::open(path.parent().expect("test database parent"))
            .expect("pin test database parent")
            .expect("test database parent present"),
    );
    let name = path.file_name().expect("test database basename");
    with_shared_host(|host| {
        let connection = match host.open_bound(root, name, mode, OriginalNativeDomain::NodeRuntime) {
            Ok(connection) => connection,
            Err(failure) => {
                let error = format!("{failure:#}");
                host.retire_open_failure(failure);
                panic!("test bound open of {} refused: {error}", path.display());
            }
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| connection.with_sql(work)));
        host.close(connection);
        match outcome {
            Ok(result) => result.unwrap_or_else(|error| panic!("test bound SQL refused: {error:#}")),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// The refusal of a live open of the Runtime store at `path`, on this
/// thread's shared test host; the refused acquisition is released into the
/// census.
pub fn live_open_refusal(path: &Path) -> String {
    with_shared_host(|host| {
        let parent = path.parent().expect("Runtime database parent");
        let root = Arc::new(lillux::PinnedDirectory::open_or_create(parent).expect("pin Runtime parent"));
        let namespace = root.lock_exclusive().expect("lock Runtime parent");
        let failure = match RuntimeDb::open_live_with_namespace_authority(
            path,
            Arc::clone(&root),
            namespace,
            Arc::clone(host.factory()),
            bound(host, &root, path, OriginalNativeBoundMode::OpenOrCreate, OriginalNativeDomain::NodeRuntime),
            lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(60)),
            runtime_reference(host),
        ) {
            Ok(db) => {
                retire(host, db);
                panic!("live Runtime open of {} was admitted", path.display());
            }
            Err(failure) => failure,
        };
        let message = format!("{failure:#}");
        let released = match failure.retry_close() {
            Ok(released) => released,
            Err(failure) => panic!("refused Runtime acquisition release retained: {failure:#}"),
        };
        for terminal in released.native_terminals(host.factory()).expect("released Runtime census") {
            host.retain_terminal(terminal);
        }
        drop(released);
        message
    })
}

/// Open the existing Runtime store at `path` through the original
/// history-reset mutation path on `host`, beneath `root` held by
/// `namespace`, refusing an unreadable lifetime fence. A refused
/// acquisition is released into the census before the panic.
pub fn open_original_mutation(
    host: &TestNativeHost,
    root: &Arc<lillux::PinnedDirectory>,
    namespace: &lillux::PinnedDirectoryLock,
    path: &Path,
) -> RuntimeDb {
    match original_mutation(host, root, namespace, path) {
        Ok(db) => db,
        Err(failure) => {
            let message = settle_original_mutation_refusal(host, failure);
            panic!("original Runtime mutation refused under the gate: {message}")
        }
    }
}

/// The refusal of an original history-reset mutation open of the Runtime
/// store at `path` on `host`; the refused acquisition is released into the
/// census. Panics if the open is admitted, after closing it into the census.
pub fn original_mutation_refusal(
    host: &TestNativeHost,
    root: &Arc<lillux::PinnedDirectory>,
    namespace: &lillux::PinnedDirectoryLock,
    path: &Path,
) -> String {
    match original_mutation(host, root, namespace, path) {
        Ok(db) => {
            retire(host, db);
            panic!("original Runtime mutation of {} was admitted", path.display());
        }
        Err(failure) => settle_original_mutation_refusal(host, failure),
    }
}

fn original_mutation(
    host: &TestNativeHost,
    root: &Arc<lillux::PinnedDirectory>,
    namespace: &lillux::PinnedDirectoryLock,
    path: &Path,
) -> std::result::Result<RuntimeDb, OriginalRuntimeAcquisitionFailure> {
    RuntimeDb::open_original_history_reset_mutation(
        path,
        Arc::clone(root),
        namespace.clone(),
        Arc::clone(host.factory()),
        bound(host, root, path, OriginalNativeBoundMode::Existing, OriginalNativeDomain::RuntimeMutation),
        runtime_reference(host),
        UnreadableLifetimeFence::Refuse,
    )
}

/// Release a refused original mutation acquisition into `host`'s census and
/// return its refusal text.
fn settle_original_mutation_refusal(host: &TestNativeHost, failure: OriginalRuntimeAcquisitionFailure) -> String {
    let message = format!("{failure:#}");
    let released = match failure.retry_close() {
        Ok(released) => released,
        Err(failure) => panic!("refused original Runtime mutation release retained: {failure:#}"),
    };
    for terminal in released.native_terminals(host.factory()).expect("released Runtime census") {
        host.retain_terminal(terminal);
    }
    drop(released);
    message
}

/// Establish a node's State and, with `runtime`, its current-schema Runtime
/// store the way its first live boot does, on this thread's shared test
/// host, released on return unless a live store still holds it. A test then running code that issues its own production host,
/// such as the offline reset, holds `lillux_sqlite::test_host::serial()`.
pub fn seed_node(config: &ryeos_node_runtime::config::Config, runtime: bool) {
    with_shared_host(|host| {
        ryeos_state::test_support::seed_node_state(host, &config.runtime_state_dir())
            .expect("seed State through the gate");
        if runtime {
            retire(host, open_live(host, &config.db_path));
        }
    });
}

/// Run `work` on this thread's shared test host, releasing the host after
/// it unless a live store still holds it.
/// Run `work` on one in-memory connection, a fresh transient ticket of this
/// thread's shared test host. The connection closes into the host's census
/// even when `work` panics, so a failing test leaves no arena behind.
pub fn with_transient<T>(work: impl for<'c> FnOnce(&'c rusqlite::Connection) -> Result<T>) -> T {
    with_shared_host(|host| {
        let connection = host.open_transient();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| connection.with_sql(work)));
        host.close(connection);
        match outcome {
            Ok(result) => result.unwrap_or_else(|error| panic!("test transient SQL refused: {error:#}")),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

fn with_shared_host<T>(work: impl FnOnce(&TestNativeHost) -> T) -> T {
    let host = shared_host();
    let value = work(&host);
    ryeos_state::test_support::release_shared_test_host(host);
    value
}

/// A Runtime store opened the way a live node opens it, on a test host
/// shared by every such store live on this thread. It dereferences to the
/// store; dropping it closes the store into the host's census, and the last
/// store on the thread releases the host. The host holds the process-wide
/// serial guard while any store is live.
pub struct TestRuntime {
    // Field order is drop order: the store closes before the host releases,
    // and a temporary directory goes last.
    db: Option<RuntimeDb>,
    host: Option<std::rc::Rc<TestNativeHost>>,
    #[cfg(test)]
    _directory: Option<tempfile::TempDir>,
}

/// The test host of this thread's live stores, shared with every State and
/// Runtime store live on the thread.
fn shared_host() -> std::rc::Rc<TestNativeHost> {
    ryeos_state::test_support::shared_test_host()
}

impl TestRuntime {
    /// Open, creating it if absent, the current-schema store at `path`.
    pub fn open(path: &Path) -> Self {
        let host = shared_host();
        let db = open_live(&host, path);
        Self {
            db: Some(db),
            host: Some(host),
            #[cfg(test)]
            _directory: None,
        }
    }

    /// Open a fresh current-schema store in a private temporary directory
    /// that lives as long as the store.
    #[cfg(test)]
    pub fn open_temporary() -> Self {
        let directory = tempfile::tempdir().expect("temporary Runtime directory");
        let mut runtime = Self::open(&directory.path().join("runtime.sqlite3"));
        runtime._directory = Some(directory);
        runtime
    }
}

impl std::ops::Deref for TestRuntime {
    type Target = RuntimeDb;
    fn deref(&self) -> &RuntimeDb {
        self.db.as_ref().expect("test Runtime open")
    }
}

impl std::ops::DerefMut for TestRuntime {
    fn deref_mut(&mut self) -> &mut RuntimeDb {
        self.db.as_mut().expect("test Runtime open")
    }
}

impl Drop for TestRuntime {
    fn drop(&mut self) {
        let (Some(db), Some(host)) = (self.db.take(), self.host.take()) else { return };
        if !std::thread::panicking() {
            retire(&host, db);
            ryeos_state::test_support::release_shared_test_host(host);
            return;
        }
        // A failed test still closes its store into the census, so the
        // arena is released for the tests after it. A refusal here is left
        // to the host's own leak report rather than a second panic.
        if let Ok(closed) = db.close_original_runtime(Arc::clone(host.factory())) {
            if let Ok(terminals) = closed.native_terminals() {
                for terminal in terminals {
                    host.retain_terminal(terminal);
                }
            }
        }
        drop(host);
    }
}
