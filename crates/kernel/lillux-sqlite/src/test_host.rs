//! Test-only native host. Each acquisition installs one real arena through
//! the same budget, region, install and initialization sequence a production
//! issuer runs, and tears it down through the exhaustive terminal census.
//!
//! The arena is a process singleton, so hosts serialize on a process-wide
//! mutex. The host holds that mutex guard and is therefore not `Send`: use it
//! from the acquiring thread only, with current-thread runtimes. Test binaries
//! that use it need nextest or `--test-threads=1`. Acquire the host before any
//! fork lease and never wait for it while holding one.
use crate::offline_host::{
    OfflineNativeAuthority, OfflineNativeHost, OfflineNativeHostFailure,
    offline_native_required_cleanup_steps,
};
use crate::{
    OriginalNativeBoundInputs, OriginalNativeBoundMode, OriginalNativeBudget,
    OriginalNativeConnection, OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority,
    OriginalNativeDomain, OriginalNativeFactory, OriginalNativeOpenFailure,
    OriginalNativeTransientInputs,
};
use anyhow::Result;
use lillux::PinnedDirectory;
use std::ffi::OsStr;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;
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
/// Hosts whose teardown refused, kept owned for the life of the process.
static RETAINED: Mutex<Vec<OfflineNativeHost>> = Mutex::new(Vec::new());

/// The process-wide serial guard alone, for every test of this crate that
/// does not install a host. Such tests still touch process-wide native state
/// (raw budgets, regions, the native process marker, the physical lock
/// registry, stock SQLite connections): a host's install and teardown move
/// that state through windows where a concurrent test observes a closing
/// process, and a concurrent stock connection holds a host's arena release
/// pending. So they serialize with every host. Never hold it while acquiring
/// a [`TestNativeHost`] on the same thread.
#[cfg(test)]
pub(crate) fn serial_guard() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Record a refused teardown and keep its host; returns the refusal text.
fn retain_leaked(retained: crate::offline_host::OfflineNativeRetained) -> String {
    let census = format!("{:#}", retained.error);
    *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(census.clone());
    RETAINED.lock().unwrap_or_else(PoisonError::into_inner).push(retained.host);
    census
}

fn current_holder() -> String {
    let thread = std::thread::current();
    format!("{} ({:?})", thread.name().unwrap_or("<unnamed>"), thread.id())
}

/// Test authority: the serial guard is the only exclusion and has no deadline.
struct TestAuthority;
impl OfflineNativeAuthority for TestAuthority {
    fn checkpoint(&self, _cleanup: bool) -> Result<()> {
        Ok(())
    }
    fn require_runtime(&self, _runtime: &PinnedDirectory) -> Result<()> {
        Ok(())
    }
    /// Test hosts carry no SQL quota of their own; the raw budget the host
    /// debits after this call is their only SQL meter.
    fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
        Ok(())
    }
}

/// Serialized owner of one real native arena at a time, issued through the
/// production [`OfflineNativeHost`] with test funding.
pub struct TestNativeHost {
    envelope: TestNativeEnvelope,
    live: Option<OfflineNativeHost>,
    /// Retirements to run against the finished backend terminal before the
    /// host releases, such as closed generation leases.
    after_backend: std::cell::RefCell<Vec<AfterBackend>>,
    _serial: MutexGuard<'static, ()>,
    _thread: PhantomData<*const ()>,
}

/// One retirement run against the finished backend terminal.
pub type AfterBackend = Box<dyn FnOnce(&crate::OriginalNativeTerminal) -> Result<()>>;

/// Finish the backend, run every after-backend retirement against its
/// terminal, then release. A refusal keeps the host.
fn finish_with(
    mut live: OfflineNativeHost,
    after_backend: Vec<AfterBackend>,
) -> std::result::Result<crate::offline_host::OfflineNativeReleased, crate::offline_host::OfflineNativeRetained> {
    let backend = match live.finish_backend() {
        Ok(backend) => backend,
        Err(error) => return Err(crate::offline_host::OfflineNativeRetained { host: live, error }),
    };
    for retire in after_backend {
        if let Err(error) = retire(&backend) {
            drop(backend);
            return Err(crate::offline_host::OfflineNativeRetained { host: live, error });
        }
    }
    drop(backend);
    live.release()
}

fn install(envelope: TestNativeEnvelope) -> std::result::Result<OfflineNativeHost, String> {
    install_with(envelope, Arc::new(TestAuthority))
}

/// A refused install whose partial host also refuses its release keeps that
/// host in [`RETAINED`] and records the refusal as leaked.
fn install_with(
    envelope: TestNativeEnvelope,
    authority: Arc<dyn OfflineNativeAuthority>,
) -> std::result::Result<OfflineNativeHost, String> {
    let cleanup = offline_native_required_cleanup_steps(envelope.storage_bytes, envelope.tickets)
        .map_err(|error| format!("cleanup quote refused: {error:#}"))?;
    // SAFETY: test-only funding; this host is the sole issuer of the envelope.
    let raw = unsafe {
        OriginalNativeBudget::prepaid_by_original_issuer(
            envelope.storage_bytes, envelope.allocator_steps, envelope.tickets, cleanup,
        )
    }
    .map_err(|refusal| format!("funding refused: {refusal}"))?;
    // SAFETY: the process-wide serial guard held by every caller of this
    // function excludes any other host; tests use private directories.
    unsafe { OfflineNativeHost::issue(raw, authority) }.map_err(|failure| {
        let message = format!("install refused: {:#}", failure.error);
        match failure.release() {
            Ok(_) => message,
            Err(OfflineNativeHostFailure { host, error }) => {
                let census = format!("{message}; partial host retained: {error:#}");
                *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(census.clone());
                if let Some(host) = host {
                    RETAINED.lock().unwrap_or_else(PoisonError::into_inner).push(host);
                }
                census
            }
        }
    })
}

/// Take the process-wide serial guard for one host, panicking naming the
/// current holder past the acquire deadline, or with the leaked census if a
/// previous host could not release its arena.
fn acquire_serial() -> MutexGuard<'static, ()> {
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
    serial
}

/// The serial guard of a production host whose teardown a consumer's own
/// controller performs. Keep it until that host is released.
pub struct TestHostSerial {
    _serial: MutexGuard<'static, ()>,
    _thread: PhantomData<*const ()>,
}
impl Drop for TestHostSerial {
    fn drop(&mut self) {
        *HOLDER.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// The serial guard alone, for a test whose code under test issues its own
/// production native host, such as an offline reset. It excludes every
/// [`TestNativeHost`] and every other holder for its lifetime, and panics
/// like [`TestNativeHost::acquire`] if a previous host leaked its arena.
/// Never hold it while acquiring a host on the same thread.
pub fn serial() -> TestHostSerial {
    TestHostSerial { _serial: acquire_serial(), _thread: PhantomData }
}

/// Issue a production [`OfflineNativeHost`] under the serial guard with the
/// caller's authority, for a consumer whose controller owns the host and
/// its staged teardown. Panics on a refused install, like
/// [`TestNativeHost::acquire_with`].
pub fn issue_offline_host(
    envelope: TestNativeEnvelope,
    authority: Arc<dyn OfflineNativeAuthority>,
) -> (OfflineNativeHost, TestHostSerial) {
    let serial = acquire_serial();
    match install_with(envelope, authority) {
        Ok(host) => (host, TestHostSerial { _serial: serial, _thread: PhantomData }),
        Err(census) => {
            *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(census.clone());
            panic!("offline test host {census}");
        }
    }
}

/// Environment variable naming the one test a re-executed child runs.
const ISOLATED_TEST: &str = "LILLUX_SQLITE_ISOLATED_TEST";
/// Stdout prefix of the outcome line an isolated child reports.
const ISOLATED_OUTCOME: &str = "lillux-sqlite isolated outcome: ";

/// The harness name of `test`, given as `concat!(module_path!(), "::name")`:
/// the harness names tests without the crate segment.
fn harness_test_name(test: &str) -> &str {
    test.split_once("::").map_or(test, |(_, name)| name)
}

/// Whether this process is the isolated child [`run_isolated`] started for
/// `test`.
pub fn is_isolated_child(test: &str) -> bool {
    std::env::var_os(ISOLATED_TEST)
        .is_some_and(|running| running.as_os_str() == OsStr::new(harness_test_name(test)))
}

/// Run `test` alone in a re-executed child of this test binary and return
/// the outcome it reported through [`end_isolated_process`]. For a scenario
/// that leaves a native owner no close can settle, such as a keeper poisoned
/// by a replaced database: that owner ends with the child's process, as it
/// ends with a supervisor's, and this process's arena never holds it. Call
/// it while holding no host. Panics if the child fails or reports nothing.
pub fn run_isolated(test: &str) -> String {
    let name = harness_test_name(test);
    let binary = std::env::current_exe().expect("isolated test binary path");
    let output = std::process::Command::new(binary)
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(ISOLATED_TEST, name)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn isolated test child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "isolated test {name} failed ({}):\n{stdout}\n{stderr}",
        output.status
    );
    // The harness exits successfully when its filter matches nothing, so an
    // outcome line is the proof that the scenario ran.
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(ISOLATED_OUTCOME))
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("isolated test {name} reported no outcome:\n{stdout}\n{stderr}"))
}

/// Report `outcome` and end this isolated child's process with `retained`
/// still owned: the host, and every native owner whose close was refused.
/// Process exit is the only end such an owner has; nothing here closes,
/// drops or forgets it. Panics in any process [`run_isolated`] did not start,
/// so an ordinary test can never end the shared test binary.
pub fn end_isolated_process<R>(retained: R, outcome: &str) -> ! {
    assert!(
        std::env::var_os(ISOLATED_TEST).is_some(),
        "only a child started by run_isolated may end its process"
    );
    let mut stdout = std::io::stdout().lock();
    std::io::Write::write_all(
        &mut stdout,
        // The harness has already printed this test's name without a line
        // break; start a fresh line so the outcome is a line of its own.
        format!("\n{ISOLATED_OUTCOME}{}\n", outcome.replace('\n', " ")).as_bytes(),
    )
    .and_then(|()| std::io::Write::flush(&mut stdout))
    .expect("report isolated outcome");
    let _retained = retained;
    std::process::exit(0)
}

/// Stdout prefix of the line an isolated peer prints once it holds what
/// its parent waits on.
const ISOLATED_READY: &str = "lillux-sqlite isolated peer ready";

/// A re-executed child of this test binary that runs `test` alone beside
/// its parent: started by [`spawn_isolated_peer`], it holds something the
/// parent tests against, such as a database writer lock, until the parent
/// lets it go through [`IsolatedPeer::finish`].
pub struct IsolatedPeer {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

/// Start `test` in a re-executed child, hand it `argument` as its first
/// stdin line, and return once it reports ready through
/// [`isolated_peer_ready`]. Panics if the child exits before it is ready.
pub fn spawn_isolated_peer(test: &str, argument: &str) -> IsolatedPeer {
    use std::io::{BufRead, Write};
    let name = harness_test_name(test);
    let binary = std::env::current_exe().expect("isolated peer binary path");
    let mut child = std::process::Command::new(binary)
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(ISOLATED_TEST, name)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn isolated peer");
    let mut stdin = child.stdin.take().expect("isolated peer stdin");
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("isolated peer stdout"));
    writeln!(stdin, "{}", argument.replace('\n', " ")).and_then(|()| stdin.flush()).expect("hand the isolated peer its argument");
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        let read = stdout.read_line(&mut line).expect("read isolated peer stdout");
        if read == 0 {
            let status = child.wait().expect("wait for isolated peer");
            panic!("isolated peer {name} exited ({status}) before it was ready:\n{seen}");
        }
        if line.trim_end().ends_with(ISOLATED_READY) {
            return IsolatedPeer { child, stdin, stdout };
        }
        seen.push_str(&line);
    }
}

impl IsolatedPeer {
    /// Let the peer go: close its stdin, then return the outcome it reported
    /// through [`isolated_peer_outcome`] once it exits. Panics if the peer
    /// fails or reports nothing.
    pub fn finish(self) -> String {
        use std::io::Read;
        let Self { mut child, stdin, mut stdout } = self;
        drop(stdin);
        let mut rest = String::new();
        stdout.read_to_string(&mut rest).expect("read isolated peer stdout");
        let status = child.wait().expect("wait for isolated peer");
        assert!(status.success(), "isolated peer failed ({status}):\n{rest}");
        rest.lines()
            .find_map(|line| line.strip_prefix(ISOLATED_OUTCOME))
            .map(str::to_owned)
            .unwrap_or_else(|| panic!("isolated peer reported no outcome:\n{rest}"))
    }
}

/// The argument [`spawn_isolated_peer`] handed this peer.
pub fn isolated_peer_argument() -> String {
    assert!(std::env::var_os(ISOLATED_TEST).is_some(), "only an isolated peer reads a peer argument");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).expect("read isolated peer argument");
    line.trim_end_matches('\n').to_owned()
}

/// Report this peer ready, then wait until its parent lets it go.
pub fn isolated_peer_ready() {
    use std::io::{Read, Write};
    assert!(std::env::var_os(ISOLATED_TEST).is_some(), "only an isolated peer reports ready");
    let mut stdout = std::io::stdout().lock();
    // The harness has already printed this test's name without a line break.
    write!(stdout, "\n{ISOLATED_READY}\n").and_then(|()| stdout.flush()).expect("report isolated peer ready");
    drop(stdout);
    let mut rest = Vec::new();
    std::io::stdin().read_to_end(&mut rest).expect("wait for the isolated peer's release");
}

/// Report this peer's outcome; the test then returns normally.
pub fn isolated_peer_outcome(outcome: &str) {
    use std::io::Write;
    assert!(std::env::var_os(ISOLATED_TEST).is_some(), "only an isolated peer reports a peer outcome");
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\n{ISOLATED_OUTCOME}{}\n", outcome.replace('\n', " "))
        .and_then(|()| stdout.flush())
        .expect("report isolated peer outcome");
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
        let serial = acquire_serial();
        let live = match install(envelope) {
            Ok(live) => live,
            Err(census) => {
                *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(census.clone());
                panic!("TestNativeHost {census}");
            }
        };
        Self {
            envelope,
            live: Some(live),
            after_backend: std::cell::RefCell::new(Vec::new()),
            _serial: serial,
            _thread: PhantomData,
        }
    }


    fn live(&self) -> &OfflineNativeHost {
        self.live.as_ref().expect("live test host present outside teardown")
    }

    pub fn factory(&self) -> &Arc<OriginalNativeFactory> {
        self.live().factory().expect("test host factory admitting")
    }

    /// Destination authority for opens this host issues directly.
    pub fn destination(&self) -> Arc<dyn OriginalNativeDestinationAuthority> {
        self.live().destination().expect("test host destination")
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

    /// Create the database `name` beneath `root` on one bound ticket of
    /// this host, run `sql` on it, and close it into the census: a test
    /// source established through the native gate.
    pub fn create_bound(&self, root: Arc<PinnedDirectory>, name: &OsStr, domain: OriginalNativeDomain, sql: &str) {
        let connection = match self.open_bound(root, name, OriginalNativeBoundMode::CreateExclusive, domain) {
            Ok(connection) => connection,
            Err(failure) => {
                let error = failure.to_string();
                self.retire_open_failure(failure);
                panic!("test bound create refused: {error}");
            }
        };
        let executed = connection.with_sql(|connection| Ok(connection.execute_batch(sql)?));
        self.close(connection);
        if let Err(error) = executed {
            panic!("test bound create SQL refused: {error:#}");
        }
    }

    /// Run `work` on the existing database `name` beneath `root` on one
    /// bound ticket of this host, then close it into the census: a test
    /// read of a store through the native gate.
    pub fn with_existing<T>(
        &self,
        root: Arc<PinnedDirectory>,
        name: &OsStr,
        domain: OriginalNativeDomain,
        work: impl for<'c> FnOnce(&'c rusqlite::Connection) -> Result<T>,
    ) -> T {
        let connection = match self.open_bound(root, name, OriginalNativeBoundMode::Existing, domain) {
            Ok(connection) => connection,
            Err(failure) => {
                let error = failure.to_string();
                self.retire_open_failure(failure);
                panic!("test bound open refused: {error}");
            }
        };
        let result = connection.with_sql(work);
        self.close(connection);
        result.unwrap_or_else(|error| panic!("test bound SQL refused: {error:#}"))
    }

    /// Native work that owns no ticket of its own, such as a stopped store's
    /// observations and role mutations, inside an allocator scope of this
    /// host's arena.
    pub fn scope<T>(&self, work: impl FnOnce() -> T) -> T {
        self.factory().raw_scope_for_test(work).expect("raw test allocator scope")
    }
    /// Native cleanup work, such as stopped intake, in a cleanup scope of
    /// this host's arena, the way a node runs it inside its original
    /// cleanup scope.
    pub fn cleanup_scope<T>(&self, work: impl FnOnce() -> T) -> T {
        self.factory().raw_cleanup_scope_for_test(work).expect("raw test cleanup scope")
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
        if let Err((_terminal, error)) = self.live().retain_terminal(terminal) {
            panic!("terminal does not belong to this test host: {error:#}");
        }
    }

    /// Record a closed projection lease for the final census.
    pub fn retain_lease(&self, lease: Arc<crate::ClosedOriginalProjectionLease>) {
        if let Err((_lease, error)) = self.live().retain_lease(lease) {
            panic!("lease does not belong to this test host: {error:#}");
        }
    }

    /// Run `retire` against the finished backend terminal when this host
    /// finishes, before it releases. A refusal leaks the host like any other
    /// refused teardown.
    pub fn after_backend(&self, retire: AfterBackend) {
        self.after_backend.borrow_mut().push(retire);
    }

    /// Issued ticket count and recorded terminals plus leases.
    pub fn ticket_census(&self) -> (u32, usize) {
        self.live().ticket_census().unwrap_or((u32::MAX, 0))
    }

    /// Tear this host down and return its release: the finished backend's
    /// plain facts and the region release fact.
    pub fn release(mut self) -> crate::offline_host::OfflineNativeReleased {
        let live = self.live.take().expect("live test host present");
        let finished = finish_with(live, self.after_backend.take());
        *HOLDER.lock().unwrap_or_else(PoisonError::into_inner) = None;
        match finished {
            Ok(released) => released,
            Err(retained) => {
                let census = retain_leaked(retained);
                panic!("TestNativeHost release leaked its arena: {census}");
            }
        }
    }

    /// Tear down this arena and install a fresh one under the same serial
    /// guard, for suites that need more than one envelope of tickets.
    pub fn cycle(&mut self) {
        let live = self.live.take().expect("live test host present");
        if let Err(retained) = finish_with(live, self.after_backend.take()) {
            let census = retain_leaked(retained);
            panic!("TestNativeHost cycle leaked its arena: {census}");
        }
        match install(self.envelope) {
            Ok(live) => self.live = Some(live),
            Err(census) => {
                *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = Some(census.clone());
                panic!("TestNativeHost {census}");
            }
        }
    }
}

impl Drop for TestNativeHost {
    fn drop(&mut self) {
        let Some(live) = self.live.take() else { return };
        let finished = finish_with(live, self.after_backend.take());
        *HOLDER.lock().unwrap_or_else(PoisonError::into_inner) = None;
        if let Err(retained) = finished {
            let census = retain_leaked(retained);
            if !std::thread::panicking() {
                panic!("TestNativeHost leaked its arena: {census}");
            }
        }
    }
}

#[cfg(test)]
impl TestNativeHost {
    /// The live production host, for this crate's host tests.
    pub(crate) fn live_host(&self) -> &OfflineNativeHost {
        self.live()
    }
    pub(crate) fn live_host_mut(&mut self) -> &mut OfflineNativeHost {
        self.live.as_mut().expect("live test host present outside teardown")
    }
    /// Arm one ticket around a raw open in this crate's tests.
    pub(crate) fn armed<T: crate::native::RawTestConnection>(
        &self,
        open: impl FnOnce() -> T,
    ) -> (T, crate::native::RawTestTicket) {
        self.factory().arm_raw_for_test(self.destination(), open).expect("armed raw test ticket")
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
    use anyhow::ensure;

    /// Refuses every checkpoint while `withdrawn` is set.
    struct SwitchAuthority {
        withdrawn: std::sync::atomic::AtomicBool,
    }
    impl OfflineNativeAuthority for SwitchAuthority {
        fn checkpoint(&self, _cleanup: bool) -> Result<()> {
            ensure!(!self.withdrawn.load(std::sync::atomic::Ordering::Acquire), "test authority withdrawn");
            Ok(())
        }
        fn require_runtime(&self, _runtime: &PinnedDirectory) -> Result<()> {
            Ok(())
        }
        // No SQL quota of its own; the host's raw budget meters SQL.
        fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn refused_partial_host_release_is_retained_not_dropped() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let authority = Arc::new(SwitchAuthority { withdrawn: std::sync::atomic::AtomicBool::new(true) });
        let before = RETAINED.lock().unwrap_or_else(PoisonError::into_inner).len();
        let census = install_with(TestNativeEnvelope::default(), authority.clone())
            .err()
            .expect("withdrawn authority refuses the install and its release");
        assert!(census.contains("partial host retained"), "{census}");
        assert_eq!(
            LEAKED.lock().unwrap_or_else(PoisonError::into_inner).as_deref(),
            Some(census.as_str()),
            "the refused release is recorded as leaked"
        );
        let host = {
            let mut retained = RETAINED.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!(retained.len(), before + 1, "the partial host is kept, not dropped");
            retained.pop().expect("retained partial host")
        };
        // Restore the authority and release the retained host, so the
        // process-wide arena and the leak record are clear for later tests.
        authority.withdrawn.store(false, std::sync::atomic::Ordering::Release);
        let released = host.release().expect("restored authority releases the partial host");
        assert!(released.facts.is_none() && released.region.is_none());
        *LEAKED.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    #[test]
    fn unarmed_open_is_refused_with_misuse() {
        let _serial = crate::test_host::serial_guard();
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
        // The synced connection's durability pins end typed in the release.
        let released = host.release();
        assert!(
            released.durability_pins.closed >= 1 && released.durability_pins.consumed_with_error == 0,
            "release ended census durability pins {:?}",
            released.durability_pins
        );
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

    /// Window-switchable authority for the production offline host.
    struct Windows {
        operation: std::sync::atomic::AtomicBool,
    }
    impl OfflineNativeAuthority for Windows {
        fn checkpoint(&self, cleanup: bool) -> Result<()> {
            ensure!(cleanup || self.operation.load(std::sync::atomic::Ordering::Acquire),
                "operation window closed");
            Ok(())
        }
        fn require_runtime(&self, _runtime: &PinnedDirectory) -> Result<()> {
            Ok(())
        }
        // No SQL quota of its own; the host's raw budget meters SQL.
        fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
            Ok(())
        }
    }

    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn raw(storage: u64, tickets: u32, cleanup: u64) -> OriginalNativeBudget {
        // SAFETY: test-only funding of one host under the serial guard.
        unsafe { OriginalNativeBudget::prepaid_by_original_issuer(storage, 50_000_000, tickets, cleanup) }.unwrap()
    }

    fn issue(raw: OriginalNativeBudget, authority: Arc<dyn OfflineNativeAuthority>)
        -> std::result::Result<OfflineNativeHost, crate::offline_host::OfflineNativeHostFailure> {
        // SAFETY: each test holds the process-wide serial guard and uses only
        // private temporary state, so the exclusion is structural.
        unsafe { OfflineNativeHost::issue(raw, authority) }
    }

    fn retained(host: &OfflineNativeHost, terminal: OriginalNativeConnectionTerminal) {
        if let Err((_terminal, error)) = host.retain_terminal(terminal) {
            panic!("terminal refused: {error:#}");
        }
    }

    /// Refuses the `n`th operation-window checkpoint, then admits again.
    struct RefuseNth {
        n: usize,
        seen: std::sync::atomic::AtomicUsize,
    }
    impl OfflineNativeAuthority for RefuseNth {
        fn checkpoint(&self, cleanup: bool) -> Result<()> {
            if cleanup {
                return Ok(());
            }
            let seen = self.seen.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            ensure!(seen != self.n, "injected refusal at checkpoint {seen}");
            Ok(())
        }
        fn require_runtime(&self, _runtime: &PinnedDirectory) -> Result<()> {
            Ok(())
        }
        // No SQL quota of its own; the host's raw budget meters SQL.
        fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
            Ok(())
        }
    }

    /// Every refusal point of issuance, including those after the factory
    /// is installed, leaves a partial host that releases its whole arena.
    #[test]
    fn every_issuance_refusal_point_releases_the_partial_host() {
        let _serial = serial();
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let mut stages = Vec::new();
        for n in 1..200 {
            let authority = Arc::new(RefuseNth { n, seen: std::sync::atomic::AtomicUsize::new(0) });
            match issue(raw(16 * MIB, 4, cleanup), authority) {
                Ok(host) => {
                    host.finish().unwrap();
                    break;
                }
                Err(failure) => {
                    let error = failure.release().unwrap_or_else(|retained| {
                        panic!("refusal at checkpoint {n} retained its partial host: {retained}")
                    });
                    stages.push(format!("{error:#}"));
                }
            }
        }
        assert!(!stages.is_empty() && stages.len() < 199, "{stages:?}");
        assert!(
            stages.iter().any(|stage| stage.contains("install refused") || stage.contains("initialization refused")),
            "no refusal after the factory install was exercised: {stages:#?}"
        );
    }

    fn transient(host: &OfflineNativeHost) -> std::result::Result<OriginalNativeConnection, OriginalNativeOpenFailure> {
        // SAFETY: test-only issuance under this host's own funded factory.
        let inputs = unsafe {
            OriginalNativeTransientInputs::issued(OriginalNativeDomain::StateTransient, host.destination().unwrap())
        }
        .unwrap();
        host.factory().unwrap().open_transient(inputs)
    }

    #[test]
    fn offline_host_refuses_underfunded_cleanup_and_releases_the_partial_host() {
        let _serial = serial();
        let required = offline_native_required_cleanup_steps(32 * MIB, 8).unwrap();
        assert!(required < 400_000, "shipped replay-indexes cleanup quota must cover {required}");
        let failure = match issue(raw(32 * MIB, 8, required - 1), Arc::new(TestAuthority)) {
            Ok(_) => panic!("underfunded cleanup must refuse"),
            Err(failure) => failure,
        };
        let error = failure.release().unwrap_or_else(|retained| panic!("partial host retained: {retained}"));
        assert!(format!("{error:#}").contains("cleanup steps"), "{error:#}");
        let host = issue(raw(32 * MIB, 8, required), Arc::new(TestAuthority)).unwrap();
        let facts = host.finish().unwrap().facts.expect("ready host facts");
        assert_eq!((facts.tickets_issued, facts.terminals), (0, 0));
    }

    #[test]
    fn offline_host_window_closes_admission_while_cleanup_completes() {
        let _serial = serial();
        let authority = Arc::new(Windows { operation: std::sync::atomic::AtomicBool::new(true) });
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let host = issue(raw(16 * MIB, 4, cleanup), authority.clone()).unwrap();
        let connection = transient(&host).unwrap_or_else(|failure| panic!("open refused: {failure}"));
        let value: i64 = connection.with_sql(|sql| Ok(sql.query_row("SELECT 7", [], |row| row.get(0))?)).unwrap();
        assert_eq!(value, 7);
        authority.operation.store(false, std::sync::atomic::Ordering::Release);
        assert!(connection.with_sql(|sql| Ok(sql.execute_batch("SELECT 1")?)).is_err(),
            "SQL refused once the operation window closes");
        let closed = connection.close().unwrap_or_else(|failure| panic!("close in cleanup window refused: {failure}"));
        retained(&host, OriginalNativeConnectionTerminal::Closed(closed));
        match transient(&host) {
            Ok(_) => panic!("admission must refuse after the operation window"),
            Err(failure) => {
                let released = failure.retry_close().unwrap_or_else(|failure| panic!("release retained: {failure}"));
                retained(&host, OriginalNativeConnectionTerminal::Released(released));
            }
        }
        let facts = host.finish().unwrap().facts.expect("ready host facts");
        assert_eq!(facts.tickets_issued as usize, facts.terminals);
    }

    /// Revalidates a published witness at every checkpoint, as a host
    /// authority that rereads its charged reservation does. `scoped` reads it
    /// through the host's own bound factory.
    struct WitnessCheckpoint {
        witness: Arc<crate::RetainedOriginalWitnessFile>,
        factory: std::sync::OnceLock<std::sync::Weak<OriginalNativeFactory>>,
        scoped: std::sync::atomic::AtomicBool,
    }
    impl OfflineNativeAuthority for WitnessCheckpoint {
        fn checkpoint(&self, cleanup: bool) -> Result<()> {
            let bound = self.factory.get().and_then(std::sync::Weak::upgrade);
            let bytes = match bound {
                Some(factory) if self.scoped.load(std::sync::atomic::Ordering::Acquire) => {
                    factory.read_original_witness(&self.witness, 64, cleanup)?
                }
                Some(_) | None => self.witness.read_original(64)?,
            };
            ensure!(bytes == b"charged witness", "witness bytes changed");
            Ok(())
        }
        fn require_runtime(&self, _runtime: &PinnedDirectory) -> Result<()> {
            Ok(())
        }
        // No SQL quota of its own; the host's raw budget meters SQL.
        fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
            Ok(())
        }
    }

    /// A host authority that rereads its witness on every checkpoint refuses
    /// every open once the host holds a projection lease, when that reread
    /// runs outside a native scope: the lease census has no owner to answer
    /// for it. Read through the host's own factory, the same open, the lease
    /// close and the host teardown all complete.
    #[test]
    fn witness_revalidation_admits_opens_under_the_hosts_own_lease_only_when_scoped() {
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let root = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let name = OsStr::new("reserved.witness");
        let witness = Arc::new(crate::RetainedOriginalWitnessFile::created(
            root.atomic_create_regular(name, b"charged witness", 0o600, crate::SQLITE_TEMP_PURPOSE)
                .unwrap()
                .unwrap(),
            root.identity().unwrap(),
            name.to_os_string(),
        ));
        let authority = Arc::new(WitnessCheckpoint {
            witness,
            factory: std::sync::OnceLock::new(),
            scoped: std::sync::atomic::AtomicBool::new(false),
        });
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let host = issue(raw(16 * MIB, 4, cleanup), authority.clone()).unwrap();
        assert!(authority.factory.set(Arc::downgrade(host.factory().unwrap())).is_ok());
        std::fs::File::create(tmp.path().join("generated.sqlite3.lease")).unwrap();
        let metadata = root.open_mount_entry(OsStr::new("generated.sqlite3.lease")).unwrap().unwrap();
        let lease_authority: Arc<dyn crate::OriginalNativeProjectionLeaseAuthority> = Arc::new(AdmittedLease);
        // SAFETY: test-only lease under this host's own funded factory; the
        // metadata inode is a fresh regular file no SQLite role uses.
        let inputs = unsafe {
            crate::OriginalProjectionLeaseInputs::issued(
                Arc::clone(&root), metadata, crate::OriginalProjectionLeaseMode::Shared, lease_authority,
            )
        };
        let lease = match host.factory().unwrap().acquire_projection_lease(inputs) {
            Ok(lease) => lease,
            Err(failure) => panic!("test lease refused: {failure}"),
        };
        let refused = match transient(&host) {
            Ok(_) => panic!("an unscoped witness reread must refuse under the held lease"),
            Err(failure) => failure,
        };
        let error = format!("{refused}");
        authority.scoped.store(true, std::sync::atomic::Ordering::Release);
        let released = refused.retry_close().unwrap_or_else(|failure| panic!("release retained: {failure}"));
        retained(&host, OriginalNativeConnectionTerminal::Released(released));
        assert!(error.contains("guarded SQLite evidence read refused (5)"), "{error}");
        let connection = transient(&host).unwrap_or_else(|failure| panic!("scoped reread refused the open: {failure}"));
        let value: i64 = connection.with_sql(|sql| Ok(sql.query_row("SELECT 7", [], |row| row.get(0))?)).unwrap();
        assert_eq!(value, 7);
        let closed = connection.close().unwrap_or_else(|failure| panic!("close refused: {failure}"));
        retained(&host, OriginalNativeConnectionTerminal::Closed(closed));
        match lease.close() {
            Ok(crate::OriginalProjectionLeaseDisposition::Closed(closed)) => {
                if let Err((_lease, error)) = host.retain_lease(closed) {
                    panic!("lease refused: {error:#}");
                }
            }
            Ok(_) => panic!("held lease closed as never acquired"),
            Err(failure) => panic!("test lease close retained: {failure}"),
        }
        let facts = host.finish().unwrap().facts.expect("ready host facts");
        assert_eq!((facts.tickets_issued, facts.terminals, facts.ticketless, facts.leases), (2, 1, 1, 1));
    }

    /// Charges SQL VM steps to its own ceiling and stops SQL at its own
    /// in-memory deadline, as a caller's SQL quota does.
    struct SqlQuota {
        used: std::sync::atomic::AtomicU64,
        limit: u64,
        deadline: Instant,
    }
    impl OfflineNativeAuthority for SqlQuota {
        fn checkpoint(&self, _cleanup: bool) -> Result<()> {
            Ok(())
        }
        fn require_runtime(&self, _runtime: &PinnedDirectory) -> Result<()> {
            Ok(())
        }
        fn charge_sql_vm_steps(&self, steps: u64) -> Result<()> {
            use std::sync::atomic::Ordering;
            ensure!(Instant::now() < self.deadline, "test SQL window elapsed");
            self.used
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    used.checked_add(steps).filter(|total| *total <= self.limit)
                })
                .map(|_| ())
                .map_err(|_| anyhow::anyhow!("test SQL ceiling exhausted"))
        }
    }

    const LONG_STATEMENT: &str =
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000000) SELECT count(*) FROM n";

    fn run_until_refused(authority: Arc<SqlQuota>) -> (String, u64) {
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let host = issue(raw(16 * MIB, 4, cleanup), authority.clone()).unwrap();
        let connection = transient(&host).unwrap_or_else(|failure| panic!("open refused: {failure}"));
        let refused = connection
            .with_sql(|sql| Ok(sql.query_row(LONG_STATEMENT, [], |row| row.get::<_, i64>(0))?))
            .expect_err("the statement must be stopped by the caller's SQL quota");
        let closed = connection.close().unwrap_or_else(|failure| panic!("close refused: {failure}"));
        retained(&host, OriginalNativeConnectionTerminal::Closed(closed));
        let facts = host.finish().unwrap().facts.expect("ready host facts");
        assert_eq!(facts.tickets_issued as usize, facts.terminals);
        (format!("{refused:#}"), authority.used.load(std::sync::atomic::Ordering::Acquire))
    }

    #[test]
    fn host_sql_is_charged_to_the_caller_ceiling_before_its_budget() {
        let _serial = serial();
        let authority = Arc::new(SqlQuota {
            used: std::sync::atomic::AtomicU64::new(0),
            limit: 10_000,
            deadline: Instant::now() + Duration::from_secs(3600),
        });
        let (refused, used) = run_until_refused(authority);
        assert!(used <= 10_000 && used > 0, "charged {used} of a 10000-step ceiling");
        assert!(refused.to_lowercase().contains("interrupt"), "{refused}");
    }

    #[test]
    fn host_sql_stops_when_the_caller_window_elapses_mid_statement() {
        let _serial = serial();
        let authority = Arc::new(SqlQuota {
            used: std::sync::atomic::AtomicU64::new(0),
            limit: u64::MAX,
            deadline: Instant::now() + Duration::from_millis(200),
        });
        let started = Instant::now();
        let (refused, used) = run_until_refused(authority);
        let elapsed = started.elapsed();
        assert!(used > 0 && elapsed >= Duration::from_millis(200), "the statement ran until the window elapsed");
        // The raw budget funds 50,000,000 steps; stopping well short of it
        // shows the window, not the budget, ended the statement.
        assert!(used < 40_000_000, "charged {used} steps; the budget, not the window, stopped it");
        assert!(refused.to_lowercase().contains("interrupt"), "{refused}");
    }

    /// Refuses cleanup-window checkpoints while `refuse` is set.
    struct CleanupGate {
        refuse: std::sync::atomic::AtomicBool,
    }
    impl OfflineNativeAuthority for CleanupGate {
        fn checkpoint(&self, cleanup: bool) -> Result<()> {
            ensure!(!(cleanup && self.refuse.load(std::sync::atomic::Ordering::Acquire)), "cleanup window refused");
            Ok(())
        }
        fn require_runtime(&self, _runtime: &PinnedDirectory) -> Result<()> {
            Ok(())
        }
        // No SQL quota of its own; the host's raw budget meters SQL.
        fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn offline_host_release_refuses_while_a_factory_alias_survives_and_resumes() {
        let _serial = serial();
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let mut host = issue(raw(16 * MIB, 4, cleanup), Arc::new(TestAuthority)).unwrap();
        assert!(host.post_backend_factory().is_err(), "post-backend factory before the backend finished");
        drop(host.finish_backend().unwrap());
        assert!(host.factory().is_err() && host.destination().is_err(), "admission ends with the backend");
        let alias = host.post_backend_factory().unwrap().clone();
        let facts = host.facts().unwrap();
        let retained = host.release().expect_err("release must refuse while a factory alias survives");
        let message = format!("{:#}", retained.error);
        assert!(message.contains("factory strong count 3 (host holds 2)"), "{message}");
        let host = retained.host;
        assert_eq!(host.facts().unwrap(), facts);
        assert!(Arc::ptr_eq(host.post_backend_factory().unwrap(), &alias), "refusal keeps the factory");
        drop(alias);
        let released = host.release().unwrap();
        assert_eq!(released.facts, Some(facts));
    }

    #[test]
    fn offline_host_release_refuses_while_the_backend_terminal_survives_and_resumes() {
        let _serial = serial();
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let mut host = issue(raw(16 * MIB, 4, cleanup), Arc::new(TestAuthority)).unwrap();
        let terminal = host.finish_backend().unwrap();
        let retained = host.release().expect_err("release must refuse while the backend terminal survives");
        let message = format!("{:#}", retained.error);
        assert!(message.contains("backend terminal strong count 2 (host holds 1)")
            && message.contains("factory strong count 2 (host holds 2)"), "{message}");
        let mut host = retained.host;
        let again = host.finish_backend().unwrap();
        assert!(Arc::ptr_eq(&again, &terminal), "refusal keeps the backend terminal");
        drop(again);
        drop(terminal);
        host.release().unwrap();
    }

    #[test]
    fn offline_host_finish_backend_and_release_resume_after_refusal() {
        let _serial = serial();
        let authority = Arc::new(CleanupGate { refuse: std::sync::atomic::AtomicBool::new(false) });
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let mut host = issue(raw(16 * MIB, 4, cleanup), authority.clone()).unwrap();
        authority.refuse.store(true, std::sync::atomic::Ordering::Release);
        assert!(host.finish_backend().is_err(), "backend finish checks the cleanup window");
        assert!(host.facts().is_err() && host.post_backend_factory().is_err());
        authority.refuse.store(false, std::sync::atomic::Ordering::Release);
        let first = host.finish_backend().unwrap();
        let second = host.finish_backend().unwrap();
        assert!(Arc::ptr_eq(&first, &second), "a finished backend returns its cached terminal");
        drop((first, second));
        authority.refuse.store(true, std::sync::atomic::Ordering::Release);
        let retained = host.release().expect_err("release checks the cleanup window");
        assert!(format!("{:#}", retained.error).contains("cleanup window refused"));
        authority.refuse.store(false, std::sync::atomic::Ordering::Release);
        retained.host.release().unwrap();
    }

    #[test]
    fn offline_host_release_requires_the_finished_backend() {
        let _serial = serial();
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let host = issue(raw(16 * MIB, 4, cleanup), Arc::new(TestAuthority)).unwrap();
        let retained = host.release().expect_err("release before the backend finished");
        assert!(format!("{:#}", retained.error).contains("backend has not finished"));
        retained.host.finish().unwrap();
    }

    #[test]
    fn offline_host_facts_match_the_census_and_the_region_release() {
        let _serial = serial();
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let mut host = issue(raw(16 * MIB, 4, cleanup), Arc::new(TestAuthority)).unwrap();
        for _ in 0..2 {
            let connection = transient(&host).unwrap_or_else(|failure| panic!("open refused: {failure}"));
            let closed = connection.close().unwrap_or_else(|failure| panic!("close refused: {failure}"));
            retained(&host, OriginalNativeConnectionTerminal::Closed(closed));
        }
        let (issued, recorded) = host.ticket_census().unwrap();
        assert_eq!((issued, recorded), (2, 2));
        drop(host.finish_backend().unwrap());
        let facts = host.facts().unwrap();
        assert_eq!(facts.tickets_issued, issued);
        assert_eq!(facts.budget.connections_issued, issued);
        assert_eq!(facts.terminals + facts.leases, recorded);
        assert_eq!((facts.ticketless, facts.storage_bytes), (0, 16 * MIB));
        assert_eq!(facts.arena, crate::OriginalNativeArenaDisposition::Released);
        assert_eq!(facts.process, std::process::id());
        assert!(facts.region_bytes > 0 && facts.budget.allocator_steps > 0);
        let released = host.release().unwrap();
        assert_eq!(released.facts.as_ref(), Some(&facts));
        let region = released.region.expect("admitted region release fact");
        assert_eq!(region.generation().unwrap(), facts.generation);
        assert_eq!(region.bytes().unwrap() as u64, facts.region_bytes);
    }

    #[test]
    fn offline_host_finish_retains_custody_while_an_alias_survives_and_resumes() {
        let _serial = serial();
        let cleanup = offline_native_required_cleanup_steps(16 * MIB, 4).unwrap();
        let host = issue(raw(16 * MIB, 4, cleanup), Arc::new(TestAuthority)).unwrap();
        let alias = host.factory().unwrap().clone();
        let retained = host.finish().expect_err("finish must refuse while a factory alias survives");
        assert!(format!("{:#}", retained.error).contains("aliases survive"), "{:#}", retained.error);
        drop(alias);
        retained.host.finish().unwrap();
    }

    /// Two threads entering unticketed allocator scopes at once contend on
    /// the native owner mutex. Contention waits; below every limit no entry
    /// may refuse, and every scope is left so the arena census ends at zero.
    #[test]
    fn concurrent_scope_entries_wait_instead_of_refusing() {
        const THREADS: usize = 2;
        const ENTRIES: usize = 20_000;
        let host = TestNativeHost::acquire();
        let factory = host.factory().clone();
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let (factory, barrier) = (factory.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..ENTRIES {
                        factory.raw_scope_for_test(|| ()).unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        // Every scope was left: a cleanup scope still opens on this thread.
        factory.raw_cleanup_scope_for_test(|| ()).unwrap();
        drop(factory);
    }

    /// An unlinked original output retires after the native backend shut
    /// SQLite down and released the arena. While SQLite is live, an inode
    /// alias of an open connection refuses unconsumed; once the host is
    /// finished, the inactive path still refuses a linked file and a foreign
    /// identity unconsumed, then consumes the exact unlinked coordinate.
    #[test]
    fn original_output_retires_after_native_shutdown_and_refuses_while_aliased() {
        use std::os::fd::{AsRawFd, IntoRawFd};
        use std::os::unix::fs::MetadataExt;
        let (host, _serial) = issue_offline_host(TestNativeEnvelope::default(), Arc::new(TestAuthority));
        let factory = host.factory().unwrap().clone();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("aliased.sqlite3");
        let (connection, ticket) = factory
            .arm_raw_for_test(host.destination().unwrap(), || rusqlite::Connection::open(&path).unwrap())
            .unwrap();
        factory
            .raw_scope_for_test(|| connection.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES(1);").unwrap())
            .unwrap();
        let output = std::fs::File::open(&path).unwrap();
        let (dev, ino) = { let metadata = output.metadata().unwrap(); (metadata.dev(), metadata.ino()) };
        std::fs::remove_file(&path).unwrap();
        let aliased = crate::binding::close_original_output(output.as_raw_fd(), dev, ino, 1 << 20);
        assert!(!aliased.consumed);
        assert_eq!(aliased.code, libsqlite3_sys::SQLITE_BUSY);
        factory.raw_scope_for_test(|| connection.close().unwrap()).unwrap();
        let terminal = factory.retire_raw_for_test(ticket).unwrap();
        if let Err((_terminal, error)) = host.retain_terminal(terminal) {
            panic!("terminal does not belong to this host: {error:#}");
        }
        drop(factory);
        if let Err(retained) = host.finish() {
            panic!("host finish retained: {:#}", retained.error);
        }

        let linked = std::fs::File::create(tmp.path().join("linked")).unwrap();
        let metadata = linked.metadata().unwrap();
        let refused = crate::binding::close_original_output(linked.as_raw_fd(), metadata.dev(), metadata.ino(), 1023);
        assert!(!refused.consumed);
        assert_eq!(refused.code, libsqlite3_sys::SQLITE_MISMATCH);
        let foreign = crate::binding::close_original_output(output.as_raw_fd(), dev, ino.wrapping_add(1), 1023);
        assert!(!foreign.consumed);
        assert_eq!(foreign.code, libsqlite3_sys::SQLITE_MISMATCH);
        assert_eq!(output.metadata().unwrap().ino(), ino);
        let retired = crate::binding::close_original_output(output.into_raw_fd(), dev, ino, 1023);
        assert!(retired.consumed);
        assert_eq!((retired.code, retired.observed_errno), (libsqlite3_sys::SQLITE_OK, 0));
        drop(linked);
    }
}
