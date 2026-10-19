//! Test opening of the whole StateStore through the native gate, the way a
//! live node opens it: Runtime, State, Operational and the projection on
//! tickets of a [`TestNativeHost`], under the App root's operator lock, and
//! closed into the host's census. Public under the `test-support` feature,
//! so other crates' tests never open a stock connection.
//!
//! Every Store a test thread holds at once shares that thread's one host:
//! the native arena is a process singleton. The host is installed with the
//! thread's first open Store and released when its last one closes.
use super::*;
use lillux_sqlite::test_host::{TestNativeEnvelope, TestNativeHost};

/// Envelope of the shared test host: room for a few Stores at once, each
/// with Runtime and its schema reference, Operational and its reference, the
/// persistent projection with its generation leases, and the tests' own SQL.
pub const TEST_STATE_STORE_ENVELOPE: TestNativeEnvelope = TestNativeEnvelope {
    storage_bytes: 128 * 1024 * 1024,
    allocator_steps: 8_000_000_000,
    tickets: 64,
};

const OPEN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);
const RUNTIME_DB_FILENAME: &str = "runtime.sqlite3";

/// This thread's shared host and the number of open Stores on it.
struct SharedHost {
    host: TestNativeHost,
    stores: usize,
}

thread_local! {
    static SHARED_HOST: std::cell::RefCell<Option<SharedHost>> = const { std::cell::RefCell::new(None) };
}

/// The shared host's factory, installing the host on first use.
fn acquire_shared_factory() -> Arc<lillux_sqlite::OriginalNativeFactory> {
    SHARED_HOST.with(|shared| {
        let mut shared = shared.borrow_mut();
        let entry = shared.get_or_insert_with(|| SharedHost {
            host: TestNativeHost::acquire_with(TEST_STATE_STORE_ENVELOPE),
            stores: 0,
        });
        entry.stores += 1;
        Arc::clone(entry.host.factory())
    })
}

/// Move this thread's shared host out of its slot, for an isolated child
/// that ends its process with a Store whose close was refused: exit runs the
/// slot's destructor, which would otherwise try to release the host.
#[cfg(test)]
pub(crate) fn take_shared_host() -> Option<TestNativeHost> {
    SHARED_HOST.with(|shared| shared.borrow_mut().take().map(|SharedHost { host, stores: _ }| host))
}

/// Run `work` with this thread's shared host, for a test that opens a
/// native owner of its own beside the Store it holds and closes that owner
/// into the same census before the Store closes. The native arena admits one
/// host per process, so such an owner cannot take a host of its own while a
/// Store is open. Panics unless a Store on this thread holds the host.
pub fn with_store_host<T>(work: impl FnOnce(&TestNativeHost) -> T) -> T {
    SHARED_HOST.with(|shared| {
        let shared = shared.borrow();
        work(&shared.as_ref().expect("a live test StateStore holds the shared host").host)
    })
}

/// Run `record` against the shared host, then drop one Store's share and
/// release the host after the last one.
fn with_shared_host_release(record: impl FnOnce(&TestNativeHost)) {
    let released = SHARED_HOST.with(|shared| {
        let mut shared = shared.borrow_mut();
        let entry = shared.as_mut().expect("test StateStore host present");
        record(&entry.host);
        entry.stores -= 1;
        if entry.stores == 0 { shared.take() } else { None }
    });
    if let Some(SharedHost { host, stores: _ }) = released {
        let (issued, recorded) = host.ticket_census();
        if issued as usize != recorded {
            eprintln!("test StateStore host census incomplete: {issued} tickets issued, {recorded} recorded");
        }
        // While a failed test unwinds, a release refusal must not panic a
        // second time: the host's own drop then retains a leaked arena.
        if std::thread::panicking() {
            drop(host);
        } else {
            drop(host.release());
        }
    }
}

/// Record a closed Store's terminals and leases in `host`'s census, and
/// retire its generation lease owners once the host finishes its backend.
fn record_census(
    host: &TestNativeHost,
    terminals: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    leases: Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>,
    lease_retirements: Vec<ryeos_state::state_db::GenerationLeaseRetirement>,
) {
    for terminal in terminals {
        host.retain_terminal(terminal);
    }
    for lease in leases {
        host.retain_lease(lease);
    }
    for retirement in lease_retirements {
        host.after_backend(Box::new(move |backend| retirement.retire_after_backend(backend)));
    }
}

/// Stack of the threads that open and close a test Store. The open, the
/// failed-start settlement and the close chain keep several large owners on
/// the stack at once; unoptimized test builds exceed the default test-thread
/// stack there.
const DEEP_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Close `store` into its census on a deep-stack thread. Only the Store and
/// a factory alias cross; the census comes back to be recorded here.
fn close_on_deep_stack(
    store: Arc<StateStore>,
    factory: Arc<lillux_sqlite::OriginalNativeFactory>,
) -> Result<StateStoreCensus> {
    on_deep_stack("test-store-close", move || {
        store
            .close_in_process_census(factory)
            .map_err(|failure| anyhow!("{failure:#}"))
    })?
}

/// Run `work` on a scoped thread with [`DEEP_STACK_BYTES`] of stack and
/// return its value. The shared test host never crosses.
fn on_deep_stack<T: Send>(name: &str, work: impl FnOnce() -> T + Send) -> Result<T> {
    std::thread::scope(|scope| {
        let running = std::thread::Builder::new()
            .name(name.to_owned())
            .stack_size(DEEP_STACK_BYTES)
            .spawn_scoped(scope, work)
            .context("spawn a deep-stack test StateStore thread")?;
        running.join().map_err(|_| anyhow!("deep-stack test StateStore thread panicked"))
    })
}

/// Lease authority of the test host: admits generation leases of its own
/// factory only, with a fixed deadline.
struct TestStoreLeaseAuthority {
    factory: std::sync::Weak<lillux_sqlite::OriginalNativeFactory>,
}
impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for TestStoreLeaseAuthority {
    fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>, _cleanup: bool) -> Result<()> {
        let issued = self.factory.upgrade().context("test Store factory absent")?;
        anyhow::ensure!(Arc::ptr_eq(&issued, factory), "test Store lease selects a foreign factory");
        Ok(())
    }
    fn fork_deadline(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>, cleanup: bool)
        -> Result<lillux::time::MonotonicDeadline> {
        lillux_sqlite::OriginalNativeProjectionLeaseAuthority::require_original(self, factory, cleanup)?;
        Ok(lillux::time::MonotonicDeadline::after(OPEN_DEADLINE))
    }
}

/// A node StateStore opened the way a live node opens it, on its thread's
/// shared test host, under its App root's operator lock. It dereferences to
/// the shared Store. Dropping it closes the Store into the host's census;
/// every other `Arc` of the Store must be gone first. It stays on the thread
/// that opened it.
pub struct TestStateStore {
    // Field order is drop order: the Store closes before the operator lock.
    store: Option<Arc<StateStore>>,
    // A factory alias; dropped before the shared host can release, since
    // the host refuses to finish while any alias survives.
    factory: Option<Arc<lillux_sqlite::OriginalNativeFactory>>,
    lock: Arc<ryeos_node_runtime::state_lock::StateLock>,
    _thread: std::marker::PhantomData<*const ()>,
}

impl TestStateStore {
    /// Open, creating it if absent, the node Store of `app_root`: State
    /// under `<app_root>/.ai/state` and Runtime at its `runtime.sqlite3`.
    pub fn open(
        app_root: &Path,
        signer: Arc<dyn Signer>,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
    ) -> Result<Self> {
        Self::open_with_barrier(app_root, signer, WriteBarrier::new(), head_trust)
    }

    /// [`Self::open`] with the caller's write barrier.
    pub fn open_with_barrier(
        app_root: &Path,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
    ) -> Result<Self> {
        Self::open_with_bundle_admission_trust(
            app_root, signer, write_barrier, head_trust, BundleAdmissionTrust::unbound(),
        )
    }

    /// [`Self::open_with_barrier`] bound to the Bundle admission trust a node
    /// binds, so retained Bundle executors revalidate as they do live. The
    /// other opens are unbound and refuse every retained Bundle executor.
    pub fn open_with_bundle_admission_trust(
        app_root: &Path,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
        bundle_admission_trust: BundleAdmissionTrust,
    ) -> Result<Self> {
        let runtime_state_dir = Self::runtime_state_dir(app_root);
        lillux::PinnedDirectory::open_or_create(&runtime_state_dir)
            .context("establish test State root")?;
        let lock = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(app_root),
        )
        .context("acquire test operator lock")?;
        let factory = acquire_shared_factory();
        let lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> =
            Arc::new(TestStoreLeaseAuthority { factory: Arc::downgrade(&factory) });
        let lease = Arc::new(lock.retain());
        let open_factory = Arc::clone(&factory);
        let app_root = app_root.to_path_buf();
        // The open, and a refused open's settlement, run on a deep-stack
        // thread like the close; the host stays on this thread.
        let opened = on_deep_stack("test-store-open", move || {
            // SAFETY: the shared test host is the sole issuer of this factory
            // under the process-wide test serial guard and is finished after
            // its last Store closes; the lease authority admits only that
            // factory; the operator lock of this exact App root is held by
            // the opening owner for the Store's whole life.
            let opened = unsafe {
                StateStore::new_in_process_native_with_bundle_admission_trust(
                    app_root,
                    runtime_state_dir.clone(),
                    runtime_state_dir.join(RUNTIME_DB_FILENAME),
                    signer,
                    write_barrier,
                    head_trust,
                    bundle_admission_trust,
                    lease,
                    Arc::clone(&open_factory),
                    lease_authority,
                    lillux::time::MonotonicDeadline::after(OPEN_DEADLINE),
                )
            };
            match opened {
                Ok(store) => Ok(store),
                Err(error) => {
                    // A refused opening keeps its native owners inside the
                    // error; they settle through the failed-start census. A
                    // refusal raised before any native owner existed has none.
                    let message = format!("{error:#}");
                    let settled = match error.downcast::<LiveStateStoreOpenFailure>() {
                        Ok(failure) => failure
                            .close_retained()
                            .map_err(|failure| anyhow!("refused StateStore start release retained: {failure}"))
                            .and_then(|released| {
                                released
                                    .into_census(open_factory)
                                    .map_err(|failure| anyhow!("refused StateStore start census retained: {failure}"))
                            })
                            .map(Some),
                        Err(_unowned) => Ok(None),
                    };
                    Err((message, settled))
                }
            }
        })?;
        match opened {
            Ok(store) => Ok(Self {
                store: Some(Arc::new(store)),
                factory: Some(factory),
                lock: Arc::new(lock),
                _thread: std::marker::PhantomData,
            }),
            Err((message, settled)) => {
                drop(factory);
                let mut settlement = Ok(());
                with_shared_host_release(|host| match settled {
                    Ok(Some(StateStoreCensus { terminals, leases, lease_retirements, prior_refusal: _ })) => {
                        record_census(host, terminals, leases, lease_retirements);
                    }
                    Ok(None) => {}
                    Err(error) => settlement = Err(error),
                });
                settlement.context("test StateStore open refused and was not settled")?;
                anyhow::bail!("test StateStore open refused: {message}")
            }
        }
    }

    /// The refusal of opening the node Store of `app_root`, after its
    /// refused start settled into the census. Panics if the open is admitted,
    /// after closing the admitted Store.
    pub fn open_refusal(
        app_root: &Path,
        signer: Arc<dyn Signer>,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
    ) -> String {
        match Self::open(app_root, signer, head_trust) {
            Ok(store) => {
                store.close().expect("close the admitted test StateStore");
                panic!("test StateStore open of {} was admitted", app_root.display());
            }
            Err(error) => format!("{error:#}"),
        }
    }

    /// The State root of a test node under `app_root`.
    pub fn runtime_state_dir(app_root: &Path) -> PathBuf {
        ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).state()
    }

    /// The shared Store. Clones must be dropped before this owner closes.
    pub fn store(&self) -> &Arc<StateStore> {
        self.store.as_ref().expect("test StateStore open")
    }

    /// A lease on the operator lock this owner holds, for the controller
    /// lifetime of a test application built over the Store. A second
    /// acquisition of the same lock would conflict with this owner.
    pub fn controller_lifetime(&self) -> Arc<ryeos_node_runtime::state_lock::StateLockLease> {
        Arc::new(self.lock.retain())
    }

    /// The operator lock this owner holds, for a test application that
    /// models standalone dispatch: standalone dispatch owns this exact lock
    /// for as long as its Store is open, and lends it to the services it runs.
    pub fn standalone_lock(&self) -> Arc<ryeos_node_runtime::state_lock::StateLock> {
        Arc::clone(&self.lock)
    }

    /// Close the Store into the census, reporting a refused close. Like a
    /// daemon exit, attachment admission closes and every in-memory registry
    /// the close audit checks ends: attachments, workers, launch tasks and
    /// owners, in-process handlers and selected invocations. Their durable
    /// rows remain for the next opening's rebuild.
    pub fn close(mut self) -> Result<()> {
        self.close_inner()
    }

    fn close_inner(&mut self) -> Result<()> {
        let store = self.store.take().context("test StateStore already closed")?;
        let factory = self.factory.take().context("test StateStore factory alias absent")?;
        let prepared = end_process_registries(&store);
        // A refused close keeps every native owner inside its failure; the
        // host then reports its arena as leaked when it is released.
        let census = match prepared {
            Ok(()) => close_on_deep_stack(store, Arc::clone(&factory)),
            Err(error) => {
                drop(store);
                Err(error)
            }
        };
        drop(factory);
        let mut outcome = Ok(());
        with_shared_host_release(|host| match census {
            Ok(StateStoreCensus { terminals, leases, lease_retirements, prior_refusal }) => {
                record_census(host, terminals, leases, lease_retirements);
                if let Some(error) = prior_refusal {
                    outcome = Err(error.context("test StateStore close recorded an earlier refusal"));
                }
            }
            Err(error) => {
                eprintln!("test StateStore close refused: {error:#}");
                outcome = Err(error.context("test StateStore close refused"))
            }
        });
        outcome
    }
}

/// End every in-memory registry the close audit checks, the way a daemon
/// exit does: attachment admission, attachments, workers, launch tasks and
/// owners, in-process handlers and selected invocations. Their durable rows
/// remain for the next opening's rebuild.
fn end_process_registries(store: &StateStore) -> Result<()> {
    (|| -> Result<()> {
            store.close_process_attachment_admission()?;
            store.attached_process_registry().clear();
            store.live_worker_registry().clear();
            // A test ends its launch tasks the way a daemon exit does: the
            // in-memory owners and abort handles end, while every durable
            // claim stays for the next opening's recovery.
            store
                .launch_task_abort_handles
                .lock()
                .map_err(|_| anyhow!("launch registry poisoned"))?
                .clear();
            store
                .inner
                .lock()
                .map_err(|_| anyhow!("Store owner poisoned"))?
                .active_launch_owners
                .clear();
            // Recorded in-process handlers lose their owner at a daemon exit:
            // every waiter observes the loss and the volatile map ends, while
            // each handler's durable rows stay for the next opening.
            let handlers = std::mem::take(
                &mut *store
                    .active_in_process_handlers
                    .lock()
                    .map_err(|_| anyhow!("handler registry poisoned"))?,
            );
            for control in handlers.into_values() {
                if control.completion().is_none() {
                    control.mark_completed(super::InProcessHandlerCompletion::OwnerLostUnsettled);
                }
            }
            // Selected-invocation metadata is process memory as well.
            store
                .selected_invocations
                .lock()
                .map_err(|_| anyhow!("selected registry poisoned"))?
                .end_process_records();
            Ok(())
    })()
}

/// Everything an isolated child keeps beside a closed Store frame it never
/// settles: the factory alias, the operator lock and the shared host, which
/// would otherwise try to release an arena the frame still holds.
#[cfg(test)]
pub(crate) type RetainedTestStoreContext = (
    Arc<lillux_sqlite::OriginalNativeFactory>,
    Arc<ryeos_node_runtime::state_lock::StateLock>,
    Option<TestNativeHost>,
);

/// Open a fresh node Store under `app_root` and close it into its closed
/// frame, not its census, for a test that retains or abandons that frame in
/// an isolated child process. The frame keeps the Store's namespace
/// exclusion and native context, so the child must end with it.
#[cfg(test)]
pub(crate) fn closed_store_for_test(app_root: &Path) -> (ClosedLiveStateStore, RetainedTestStoreContext) {
    let identity = ryeos_node_runtime::identity::NodeIdentity::create(&app_root.join("node-key.pem"))
        .expect("test node identity");
    let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(identity.fingerprint().to_string(), *identity.verifying_key());
    let mut opened = TestStateStore::open(app_root, signer, Arc::new(head_trust)).expect("test StateStore opens");
    let store = opened.store.take().expect("test StateStore open");
    let factory = opened.factory.take().expect("test StateStore factory alias");
    let lock = Arc::clone(&opened.lock);
    end_process_registries(&store).expect("test StateStore registries end");
    let close_factory = Arc::clone(&factory);
    let closed = on_deep_stack("test-store-close-live", move || {
        store.close_live(close_factory).map_err(|failure| anyhow!("{failure:#}"))
    })
    .and_then(|closed| closed)
    .expect("test StateStore closes into its frame");
    (closed, (factory, lock, take_shared_host()))
}

impl std::ops::Deref for TestStateStore {
    type Target = Arc<StateStore>;
    fn deref(&self) -> &Arc<StateStore> {
        self.store()
    }
}

impl Drop for TestStateStore {
    fn drop(&mut self) {
        if self.store.is_none() {
            return;
        }
        // A failed test still closes its Store into the census so the arena
        // is released for the tests after it; a refused close is reported
        // unless the test is already panicking.
        if let Err(error) = self.close_inner() {
            if std::thread::panicking() {
                eprintln!("test StateStore close refused while unwinding: {error:#}");
            } else {
                panic!("{error:#}");
            }
        }
    }
}

/// An accounting ledger opened on this thread's shared test host the way a
/// live node opens it: the fixed ledger role and its schema reference on the
/// host's tickets, closed into the host's census. It dereferences to the
/// ledger. Every `Arc` handed out by [`Self::shared`] must be gone before it
/// closes; it stays on the thread that opened it.
pub struct TestAccountingDb {
    // Field order is drop order: the SQL alias goes before the native owner.
    db: Option<Arc<crate::accounting_db::AccountingDb>>,
    ledger: Option<Arc<crate::accounting_db::AccountingNativeLedger>>,
    factory: Option<Arc<lillux_sqlite::OriginalNativeFactory>>,
    _thread: std::marker::PhantomData<*const ()>,
}

impl TestAccountingDb {
    /// Open, creating it if absent, the ledger in `runtime_state_dir`.
    pub fn open(runtime_state_dir: &Path) -> Result<Self> {
        let directory = Arc::new(
            lillux::PinnedDirectory::open_or_create(runtime_state_dir)
                .context("pin test accounting directory")?,
        );
        let factory = acquire_shared_factory();
        let destination = SHARED_HOST.with(|shared| {
            shared.borrow().as_ref().map(|entry| entry.host.destination())
        });
        let mut owner = Self { db: None, ledger: None, factory: Some(Arc::clone(&factory)), _thread: std::marker::PhantomData };
        let Some(destination) = destination else {
            return Err(owner.refused(anyhow!("test accounting host absent")));
        };
        let opened = crate::accounting_db::AccountingDb::open_native_for_test(factory, &directory, destination);
        match opened {
            Ok((ledger, Ok(db))) => {
                owner.ledger = Some(ledger);
                owner.db = Some(Arc::new(db));
                Ok(owner)
            }
            Ok((ledger, Err(error))) => {
                owner.ledger = Some(ledger);
                Err(owner.refused(error.context("open test accounting ledger")))
            }
            Err(error) => Err(owner.refused(error.context("issue test accounting ledger inputs"))),
        }
    }

    /// Settle a refused open into the census and return its refusal.
    fn refused(mut self, error: anyhow::Error) -> anyhow::Error {
        match self.close_inner() {
            Ok(()) => error,
            Err(settlement) => error.context(format!("refused open not settled: {settlement:#}")),
        }
    }

    /// A shared alias of the ledger, for an application state. It must be
    /// dropped before this owner closes.
    pub fn shared(&self) -> Arc<crate::accounting_db::AccountingDb> {
        Arc::clone(self.db.as_ref().expect("test accounting ledger open"))
    }

    /// Close the ledger into the census, reporting a refused close.
    pub fn close(mut self) -> Result<()> {
        self.close_inner()
    }

    fn close_inner(&mut self) -> Result<()> {
        let Some(factory) = self.factory.take() else { return Ok(()) };
        if let Some(db) = self.db.take() {
            if Arc::strong_count(&db) != 1 {
                self.db = Some(db);
                self.factory = Some(factory);
                anyhow::bail!("test accounting ledger still has shared aliases");
            }
            drop(db);
        }
        let terminals = match self.ledger.take() {
            None => Ok(Vec::new()),
            Some(ledger) => match ledger.close() {
                Err(error) => {
                    self.ledger = Some(ledger);
                    Err(error)
                }
                Ok(()) => match ledger.into_census(&factory) {
                    Ok(terminals) => Ok(terminals),
                    Err((ledger, error)) => {
                        self.ledger = Some(ledger);
                        Err(error)
                    }
                },
            },
        };
        let terminals = match terminals {
            Ok(terminals) => terminals,
            Err(error) => {
                self.factory = Some(factory);
                return Err(error.context("test accounting ledger close refused"));
            }
        };
        drop(factory);
        with_shared_host_release(|host| {
            for terminal in terminals {
                host.retain_terminal(terminal);
            }
        });
        Ok(())
    }
}

impl std::ops::Deref for TestAccountingDb {
    type Target = crate::accounting_db::AccountingDb;
    fn deref(&self) -> &crate::accounting_db::AccountingDb {
        self.db.as_ref().expect("test accounting ledger open")
    }
}

impl Drop for TestAccountingDb {
    fn drop(&mut self) {
        if self.factory.is_none() {
            return;
        }
        // A refused close keeps its owners; the host then reports its arena
        // as leaked when it is released.
        if let Err(error) = self.close_inner() {
            if std::thread::panicking() {
                eprintln!("test accounting ledger close refused while unwinding: {error:#}");
            } else {
                panic!("{error:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer_and_trust(root: &Path) -> (Arc<dyn Signer>, Arc<ryeos_state::refs::TrustStore>) {
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&root.join("node-key.pem")).unwrap();
        let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_string(), *identity.verifying_key());
        (signer, Arc::new(trust))
    }

    fn census() -> (u32, usize) {
        SHARED_HOST.with(|shared| {
            shared.borrow().as_ref().map_or((0, 0), |entry| entry.host.ticket_census())
        })
    }

    /// A refused start settles every ticket it issued into the census, so
    /// the host still releases after the Stores it holds close.
    #[test]
    fn refused_start_settles_every_issued_ticket_into_the_census() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let (signer, trust) = signer_and_trust(first_root.path());
        let first = TestStateStore::open(first_root.path(), Arc::clone(&signer), Arc::clone(&trust)).unwrap();
        let (issued_before, recorded_before) = census();
        let (second_signer, second_trust) = signer_and_trust(second_root.path());
        let refused = TestStateStore::open(second_root.path(), second_signer, second_trust);
        let (issued_after, recorded_after) = census();
        let issued = issued_after - issued_before;
        let recorded = recorded_after - recorded_before;
        eprintln!("refused={:?} issued={issued} recorded={recorded}", refused.as_ref().err().map(|e| format!("{e:#}")));
        drop(refused);
        assert_eq!(issued as usize, recorded, "every ticket the refused start issued reaches the census");
        first.close().unwrap();
    }
}
