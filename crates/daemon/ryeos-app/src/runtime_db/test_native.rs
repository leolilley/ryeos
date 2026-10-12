//! Test opening of the Runtime store through the native gate, the way a live
//! node opens it: a bound ticket on a [`TestNativeHost`], validated against
//! the native schema reference, and closed into the host's census.
use super::*;
use lillux_sqlite::test_host::TestNativeHost;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeBoundMode, OriginalNativeConnectionTerminal,
    OriginalNativeDomain, OriginalNativeTransientInputs,
};
use std::sync::Arc;

/// The closed native Runtime schema reference, on its own host ticket.
pub(crate) fn runtime_reference(host: &TestNativeHost) -> sqlite_schema::ClosedNativeSchemaSqlReference {
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
pub(crate) fn bound(
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
pub(crate) fn open_live(host: &TestNativeHost, path: &Path) -> RuntimeDb {
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
pub(crate) fn retire(host: &TestNativeHost, db: RuntimeDb) -> Vec<OriginalNativeDomain> {
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
pub(crate) fn read_existing<T>(path: &Path, work: impl for<'c> FnOnce(&'c rusqlite::Connection) -> Result<T>) -> T {
    let root = Arc::new(
        lillux::PinnedDirectory::open(path.parent().expect("Runtime database parent"))
            .expect("pin Runtime parent")
            .expect("Runtime parent present"),
    );
    let name = path.file_name().expect("Runtime database basename");
    with_shared_host(|host| host.with_existing(root, name, OriginalNativeDomain::NodeRuntime, work))
}

/// The refusal of a live open of the Runtime store at `path`, on this
/// thread's shared test host; the refused acquisition is released into the
/// census.
pub(crate) fn live_open_refusal(path: &Path) -> String {
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

/// Establish a node's State and, with `runtime`, its current-schema Runtime
/// store the way its first live boot does, on this thread's shared test
/// host, released on return unless a live store still holds it. A test then running code that issues its own production host,
/// such as the offline reset, holds `lillux_sqlite::test_host::serial()`.
pub(crate) fn seed_node(config: &ryeos_node_runtime::config::Config, runtime: bool) {
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
fn with_shared_host<T>(work: impl FnOnce(&TestNativeHost) -> T) -> T {
    let host = shared_host();
    let value = work(&host);
    if let Ok(host) = std::rc::Rc::try_unwrap(host) {
        drop(host.release());
    }
    value
}

/// A Runtime store opened the way a live node opens it, on a test host
/// shared by every such store live on this thread. It dereferences to the
/// store; dropping it closes the store into the host's census, and the last
/// store on the thread releases the host. The host holds the process-wide
/// serial guard while any store is live.
pub(crate) struct TestRuntime {
    // Field order is drop order: the store closes before the host releases.
    db: Option<RuntimeDb>,
    host: Option<std::rc::Rc<TestNativeHost>>,
}

/// Envelope of a shared host: room for the several stores a test keeps
/// live at once on one arena.
const SHARED_ENVELOPE: lillux_sqlite::test_host::TestNativeEnvelope = lillux_sqlite::test_host::TestNativeEnvelope {
    storage_bytes: 128 * 1024 * 1024,
    allocator_steps: 4_000_000_000,
    tickets: 64,
};

thread_local! {
    static SHARED_HOST: std::cell::RefCell<std::rc::Weak<TestNativeHost>> =
        const { std::cell::RefCell::new(std::rc::Weak::new()) };
}

/// The test host of this thread's live stores, acquired by the first.
fn shared_host() -> std::rc::Rc<TestNativeHost> {
    SHARED_HOST.with(|slot| {
        if let Some(host) = slot.borrow().upgrade() {
            return host;
        }
        let host = std::rc::Rc::new(TestNativeHost::acquire_with(SHARED_ENVELOPE));
        *slot.borrow_mut() = std::rc::Rc::downgrade(&host);
        host
    })
}

impl TestRuntime {
    /// Open, creating it if absent, the current-schema store at `path`.
    pub(crate) fn open(path: &Path) -> Self {
        let host = shared_host();
        let db = open_live(&host, path);
        Self { db: Some(db), host: Some(host) }
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
            if let Ok(host) = std::rc::Rc::try_unwrap(host) {
                drop(host.release());
            }
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
