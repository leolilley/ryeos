use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::process::{self, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};

use clap::Subcommand;

// ---------------------------------------------------------------------------
// Library types — clean Rust API, no JSON
// ---------------------------------------------------------------------------

/// Request to run a subprocess synchronously.
///
/// Env handling: `envs` is **authoritative**. The runner clears the
/// subprocess environment with `Command::env_clear()` before applying
/// `envs`, so callers MUST populate every env var the subprocess
/// needs. Inheriting parent env is not supported. This contract
/// closes the secret-leak hole where shell-exported variables on the
/// daemon process bypassed `required_secrets` scoping.
pub struct SubprocessRequest {
    pub cmd: String,
    /// Optional `argv[0]` spelling distinct from the executable path.
    /// Descriptor-backed launchers use this to execute immutable content while
    /// preserving virtual-environment and multi-call binary semantics.
    pub argv0: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub envs: Vec<(String, String)>,
    pub stdin_data: Option<String>,
    pub timeout: f64,
    /// Optional limits installed in the child immediately before `exec`.
    pub limits: Option<SubprocessLimits>,
    /// Open descriptors intentionally kept alive and inherited through exec.
    /// Lillux retains the handles and clears `FD_CLOEXEC` only in the forked
    /// child. Trusted launchers use these for descriptor-backed authorities.
    pub inherited_fds: Vec<std::sync::Arc<std::fs::File>>,
    /// Optional trusted launcher status channel. When present, Lillux waits for
    /// the launcher to report the host PID of its target and supervises that
    /// target's process group in addition to the outer launcher process.
    pub supervised_status: Option<SupervisedProcessStatus>,
}

/// Resource limits applied to a spawned subprocess.
///
/// Limits are fail-closed: a configured limit that is unsupported, invalid,
/// or cannot be installed prevents the subprocess from spawning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubprocessLimits {
    /// Maximum number of file descriptors the subprocess may open.
    pub max_open_files: Option<u64>,
    /// Maximum stdout bytes retained by the node. Lillux continues draining
    /// the pipe after this threshold, but terminates the supervised workload
    /// and reports an explicit output-limit outcome.
    pub max_stdout_bytes: Option<u64>,
    /// Maximum stderr bytes retained by the node. Semantics match
    /// [`Self::max_stdout_bytes`].
    pub max_stderr_bytes: Option<u64>,
}

/// Safe retained-output fallback for callers that do not supply a tighter
/// limit. `None` means this bound, never unbounded daemon memory growth.
pub const DEFAULT_MAX_CAPTURE_BYTES: u64 = 8 * 1024 * 1024;

/// Which captured stream crossed its node-owned retention limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLimitExceeded {
    Stdout,
    Stderr,
    Both,
}

impl OutputLimitExceeded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Both => "stdout_and_stderr",
        }
    }
}

/// Parent end of a trusted launcher's target-status channel.
///
/// Construct this only through [`supervised_launcher_status_pipe`]. That
/// factory and the parser form one protocol: the paired writer must be
/// inherited by the launcher, which reports the target's host PID in a bounded
/// `{"child-pid": <u32>}` JSON document. The target must remain in the
/// launcher's Lillux-owned process group. Retaining the outer child then keeps
/// that PGID owned until Lillux has terminated every remaining group member.
pub enum SupervisedProcessStatus {
    Run {
        reader: std::fs::File,
    },
    AwaitingAttachment {
        reader: std::fs::File,
        /// Parent-owned release end of the attachment boundary installed by
        /// the trusted launcher. The supervised target has been created and
        /// reported but cannot exec user code until the daemon explicitly
        /// releases this authority after durable process attachment.
        attachment_release: ProcessAttachmentRelease,
    },
}

/// Parent-owned release end of a trusted launcher's attachment boundary.
///
/// The read end is inherited by the trusted launcher and consumed by its
/// backend immediately before target exec. Dropping this value without
/// release closes the pipe; [`RunningProcess::drop`] then terminates the whole
/// supervised group, so a failed durable attachment can never leak a runnable
/// target.
pub struct ProcessAttachmentRelease {
    writer: Option<std::fs::File>,
}

/// Both ends needed to connect Lillux supervision to a trusted launcher.
pub struct SupervisedLauncherStatusPipe {
    pub reader: SupervisedProcessStatus,
    pub writer: Arc<std::fs::File>,
}

/// Exact status and release authorities for a supervised target that must
/// remain blocked until durable process attachment.
pub struct SupervisedLauncherAttachmentStatusPipe {
    pub reader: SupervisedProcessStatus,
    pub writer: Arc<std::fs::File>,
    /// Read end inherited by the trusted launcher and bound to its final
    /// target-exec boundary.
    pub attachment_release_reader: Arc<std::fs::File>,
    /// Child-side duplicate of the release writer. The trusted launcher keeps
    /// it open while blocked so parent death cannot turn pipe EOF into a
    /// release.
    pub attachment_release_keepalive_writer: Arc<std::fs::File>,
}

impl SupervisedLauncherStatusPipe {
    /// Raw descriptor to pass to the trusted launcher.
    #[cfg(unix)]
    pub fn writer_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.writer.as_raw_fd()
    }

    #[cfg(not(unix))]
    pub fn writer_fd(&self) -> i32 {
        // Construction fails on non-Linux platforms, so this value is never
        // handed to a child. Keeping the method in the cross-platform API lets
        // shared launcher plumbing compile without platform-specific branches.
        -1
    }
}

impl SupervisedLauncherAttachmentStatusPipe {
    /// Raw status descriptor to pass to the trusted launcher.
    #[cfg(unix)]
    pub fn writer_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.writer.as_raw_fd()
    }

    #[cfg(not(unix))]
    pub fn writer_fd(&self) -> i32 {
        -1
    }
}

/// Create an atomically-CLOEXEC status pipe for trusted-launcher supervision.
///
/// The writer remains CLOEXEC in the multithreaded parent. Lillux clears that
/// bit only in the forked child through `inherited_fds`, avoiding descriptor
/// leaks into unrelated concurrent spawns.
#[cfg(target_os = "linux")]
pub fn supervised_launcher_status_pipe() -> Result<SupervisedLauncherStatusPipe, String> {
    use std::os::fd::FromRawFd as _;

    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "create supervised-launcher status pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: pipe2 initialized both owned descriptors on success. Each is
    // transferred into exactly one File below.
    let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok(SupervisedLauncherStatusPipe {
        reader: SupervisedProcessStatus::Run { reader },
        writer: Arc::new(writer),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn supervised_launcher_status_pipe() -> Result<SupervisedLauncherStatusPipe, String> {
    Err("supervised-launcher status is supported only on Linux".to_string())
}

/// Create the target-status channel together with an explicit pre-exec
/// attachment boundary for a trusted launcher.
///
/// Unlike stopping in `pre_exec`, the backend-owned boundary does not deadlock
/// `Command::spawn`: the launcher execs normally, creates and reports its
/// target, and that target blocks at the final backend boundary until the
/// parent releases the writer retained in [`SupervisedProcessStatus`].
#[cfg(target_os = "linux")]
pub fn supervised_launcher_attachment_status_pipe(
) -> Result<SupervisedLauncherAttachmentStatusPipe, String> {
    use std::os::fd::FromRawFd as _;

    let mut status_fds = [-1; 2];
    if unsafe { libc::pipe2(status_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "create supervised-launcher status pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut gate_fds = [-1; 2];
    if unsafe { libc::pipe2(gate_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(status_fds[0]);
            libc::close(status_fds[1]);
        }
        return Err(format!(
            "create supervised-launcher attachment boundary: {error}"
        ));
    }

    // SAFETY: both pipe2 calls initialized uniquely owned descriptors. Each is
    // transferred into exactly one File below.
    let status_reader = unsafe { std::fs::File::from_raw_fd(status_fds[0]) };
    let status_writer = unsafe { std::fs::File::from_raw_fd(status_fds[1]) };
    let gate_reader = unsafe { std::fs::File::from_raw_fd(gate_fds[0]) };
    let gate_writer = unsafe { std::fs::File::from_raw_fd(gate_fds[1]) };
    let gate_keepalive_writer =
        Arc::new(gate_writer.try_clone().map_err(|error| {
            format!("duplicate supervised-launcher attachment keepalive: {error}")
        })?);
    Ok(SupervisedLauncherAttachmentStatusPipe {
        reader: SupervisedProcessStatus::AwaitingAttachment {
            reader: status_reader,
            attachment_release: ProcessAttachmentRelease {
                writer: Some(gate_writer),
            },
        },
        writer: Arc::new(status_writer),
        attachment_release_reader: Arc::new(gate_reader),
        attachment_release_keepalive_writer: gate_keepalive_writer,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn supervised_launcher_attachment_status_pipe(
) -> Result<SupervisedLauncherAttachmentStatusPipe, String> {
    Err("supervised-launcher attachment boundaries are supported only on Linux".to_string())
}

/// Create an immutable, rewound anonymous file for descriptor-backed protocol
/// data.
///
/// The returned descriptor is always above stdio, retains `FD_CLOEXEC`, and
/// carries all four write-prevention seals. Callers explicitly inherit it only
/// for the child exec that consumes the data.
#[cfg(target_os = "linux")]
pub fn sealed_memfd(name: &std::ffi::CStr, bytes: &[u8]) -> Result<Arc<std::fs::File>, String> {
    sealed_memfd_with_flags(name, bytes, libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
}

/// Create a sealed anonymous file that the supported Linux kernel may execute.
/// This is separate from protocol-data memfds so hardened `memfd_noexec`
/// policies cannot silently turn an exact executable capture into a noexec fd.
#[cfg(target_os = "linux")]
pub fn sealed_executable_memfd(
    name: &std::ffi::CStr,
    bytes: &[u8],
) -> Result<Arc<std::fs::File>, String> {
    // MFD_EXEC was added in Linux 6.3; RyeOS requires Linux 6.9 or newer.
    const MFD_EXEC: libc::c_uint = 0x0010;
    sealed_memfd_with_flags(
        name,
        bytes,
        libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | MFD_EXEC,
    )
}

#[cfg(target_os = "linux")]
fn sealed_memfd_with_flags(
    name: &std::ffi::CStr,
    bytes: &[u8],
    flags: libc::c_uint,
) -> Result<Arc<std::fs::File>, String> {
    use std::io::{Seek as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let mut fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    if fd < 0 {
        return Err(format!(
            "create sealed memfd: {}",
            std::io::Error::last_os_error()
        ));
    }
    if fd <= libc::STDERR_FILENO {
        let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        let duplicate_error = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        if duplicated < 0 {
            return Err(format!(
                "move sealed memfd descriptor above stdio: {duplicate_error}"
            ));
        }
        fd = duplicated;
    }

    // SAFETY: memfd_create or F_DUPFD_CLOEXEC returned this uniquely owned fd.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes)
        .map_err(|error| format!("write sealed memfd: {error}"))?;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| format!("rewind sealed memfd: {error}"))?;

    let required_seals =
        libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, required_seals) } < 0 {
        return Err(format!("seal memfd: {}", std::io::Error::last_os_error()));
    }
    let observed_seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if observed_seals < 0 {
        return Err(format!(
            "inspect sealed memfd seals: {}",
            std::io::Error::last_os_error()
        ));
    }
    if observed_seals & required_seals != required_seals {
        return Err(format!(
            "sealed memfd is missing required seals (observed {observed_seals:#x})"
        ));
    }

    Ok(Arc::new(file))
}

#[cfg(not(target_os = "linux"))]
pub fn sealed_memfd(_name: &std::ffi::CStr, _bytes: &[u8]) -> Result<Arc<std::fs::File>, String> {
    Err("sealed memfd is supported only on Linux".to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn sealed_executable_memfd(
    _name: &std::ffi::CStr,
    _bytes: &[u8],
) -> Result<Arc<std::fs::File>, String> {
    Err("sealed executable memfd is supported only on Linux".to_string())
}

/// Result of a synchronous subprocess execution.
#[derive(Debug)]
pub struct SubprocessResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: f64,
    pub pid: u32,
    pub timed_out: bool,
    /// Canonical isolation-layer diagnostic emitted by a trusted launcher
    /// before target exec. Lillux validates only the strict outer envelope.
    pub launcher_refusal: Option<String>,
    /// Set when a node-owned stdout/stderr retention limit was crossed. This
    /// outcome always makes `success` false, independently of the exit status.
    pub output_limit_exceeded: Option<OutputLimitExceeded>,
    /// Whether bytes beyond the retained stdout prefix were drained/discarded.
    pub stdout_truncated: bool,
    /// Whether bytes beyond the retained stderr prefix were drained/discarded.
    pub stderr_truncated: bool,
}

#[derive(Debug)]
enum InitialLauncherStatus {
    Target(u32),
    Refused(String),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherTargetDocument {
    #[serde(rename = "child-pid")]
    child_pid: u32,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherRefusalDocument {
    refused: Box<serde_json::value::RawValue>,
}

/// Validate retained descriptors and make them inheritable only inside this
/// command's forked child. Callers must keep the Arc handles alive through
/// `spawn`/`status`.
pub fn configure_inherited_fds(
    command: &mut process::Command,
    inherited_fds: &[std::sync::Arc<std::fs::File>],
) -> Result<(), String> {
    #[cfg(not(unix))]
    {
        let _ = command;
        if inherited_fds.is_empty() {
            return Ok(());
        }
        return Err("inherited descriptors are unsupported on this platform".to_string());
    }

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;

        let mut raw = Vec::with_capacity(inherited_fds.len());
        for file in inherited_fds {
            let fd = file.as_raw_fd();
            if fd <= libc::STDERR_FILENO {
                return Err(format!("inherited descriptor {fd} overlaps stdio"));
            }
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 {
                return Err(format!(
                    "inherited descriptor {fd} cannot be inspected: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if flags & libc::FD_CLOEXEC == 0 {
                return Err(format!(
                    "inherited descriptor {fd} is not protected by FD_CLOEXEC"
                ));
            }
            raw.push(fd);
        }
        unsafe {
            command.pre_exec(move || {
                for fd in &raw {
                    let flags = libc::fcntl(*fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(())
    }
}

/// Result of a detached spawn.
pub struct SpawnResult {
    pub pid: u32,
}

#[derive(Debug, Clone, Copy)]
enum CapturedStream {
    Stdout,
    Stderr,
}

#[derive(Default)]
struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

type SharedCapture = Arc<Mutex<BoundedCapture>>;

#[derive(Debug, Clone, Copy)]
struct ProcessIdentity {
    pid: u32,
    pgid: i64,
}

enum WrapperPoll {
    Running,
    /// Linux `waitid(WNOWAIT)` observed termination while preserving the
    /// wrapper PID/PGID for one final identity-checked group cleanup.
    ExitedUnreaped,
    /// Non-Linux fallback where `Child::try_wait` necessarily reaped first.
    #[cfg(not(target_os = "linux"))]
    ExitedReaped(process::ExitStatus),
}

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const POST_STOP_DRAIN_READS: usize = 1024;
const SUPERVISED_STATUS_SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACHMENT_ABORT_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const SUPERVISED_STATUS_MAX_LINE_BYTES: usize = 64 * 1024;
const ATTACHMENT_READY_MAGIC: [u8; 4] = *b"LAR1";
const ATTACHMENT_READY_RECORD_BYTES: usize = 16;
const ATTACHMENT_IDENTITY_PHASE: u32 = 1;
const ATTACHMENT_READY_PHASE: u32 = 2;
const ATTACHMENT_RELEASE_TOKEN: u8 = 1;

/// Process-wide lease for descriptors whose inherited open-file descriptions
/// carry authority across `fork(2)` (notably advisory file locks).
///
/// A direct attachment launch deliberately remains between fork and exec while
/// RyeOS persists its exact identity. `FD_CLOEXEC` cannot help during that
/// interval: the child has not executed yet. Callers that hold fork-sensitive
/// descriptor authority retain this shared lease for the same lexical scope.
/// Lillux takes the exclusive side only across the direct fork/readiness
/// window, proving that a held child did not inherit one of those transient
/// authorities. The lease is released before durable attachment, target
/// release, or the runtime's lifetime, so independent executions remain
/// concurrent.
static DIRECT_ATTACHMENT_FORK_BARRIER: OnceLock<DescriptorForkBarrier> = OnceLock::new();

#[derive(Default)]
struct DescriptorForkBarrierState {
    retained_scopes: usize,
    retained_scope_owners: HashMap<thread::ThreadId, usize>,
    waiting_forks: usize,
    fork_quiesced: bool,
    pending_fork_control_fds: BTreeSet<i32>,
}

struct DescriptorForkBarrier {
    state: Mutex<DescriptorForkBarrierState>,
    changed: Condvar,
    waiting_control_closers: AtomicUsize,
}

fn direct_attachment_fork_barrier() -> &'static DescriptorForkBarrier {
    DIRECT_ATTACHMENT_FORK_BARRIER.get_or_init(|| DescriptorForkBarrier {
        state: Mutex::new(DescriptorForkBarrierState::default()),
        changed: Condvar::new(),
        waiting_control_closers: AtomicUsize::new(0),
    })
}

/// Shared proof that the current scope may own descriptor-backed authority
/// which a pre-exec attachment child must not inherit.
pub struct ForkSensitiveDescriptorLease {
    owner: thread::ThreadId,
    retained: bool,
    _not_send: PhantomData<Rc<()>>,
}

/// Retain the process-wide fork-sensitive descriptor lease.
///
/// Acquire this before opening or locking descriptor-backed authority and keep
/// it until those descriptors/locks have been released. Acquisition is
/// intentionally infallible after poisoning: the barrier protects process
/// topology, not data whose consistency could be invalidated by a panic.
pub fn retain_fork_sensitive_descriptors() -> ForkSensitiveDescriptorLease {
    let barrier = direct_attachment_fork_barrier();
    let owner = thread::current().id();
    let mut state = barrier
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while state.fork_quiesced
        || (state.waiting_forks != 0 && !state.retained_scope_owners.contains_key(&owner))
    {
        state = barrier
            .changed
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    state.retained_scopes = state
        .retained_scopes
        .checked_add(1)
        .expect("fork-sensitive descriptor lease count overflow");
    let owner_scopes = state.retained_scope_owners.entry(owner).or_default();
    *owner_scopes = owner_scopes
        .checked_add(1)
        .expect("fork-sensitive descriptor owner count overflow");
    ForkSensitiveDescriptorLease {
        owner,
        retained: true,
        _not_send: PhantomData,
    }
}

impl Drop for ForkSensitiveDescriptorLease {
    fn drop(&mut self) {
        if !self.retained {
            return;
        }
        let barrier = direct_attachment_fork_barrier();
        let mut state = barrier
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.retained_scopes = state
            .retained_scopes
            .checked_sub(1)
            .expect("fork-sensitive descriptor lease count underflow");
        let owner_scopes = state
            .retained_scope_owners
            .get_mut(&self.owner)
            .expect("fork-sensitive descriptor owner was not registered");
        *owner_scopes = owner_scopes
            .checked_sub(1)
            .expect("fork-sensitive descriptor owner count underflow");
        if *owner_scopes == 0 {
            state.retained_scope_owners.remove(&self.owner);
        }
        self.retained = false;
        if state.retained_scopes == 0 {
            barrier.changed.notify_all();
        }
    }
}

struct QuiescedForkSensitiveDescriptors;

impl QuiescedForkSensitiveDescriptors {
    fn pending_fork_control_fds(&self) -> Vec<i32> {
        let barrier = direct_attachment_fork_barrier();
        let state = barrier
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(state.fork_quiesced);
        state.pending_fork_control_fds.iter().copied().collect()
    }

    fn register_pending_fork_control(
        &self,
        release_writer: std::fs::File,
    ) -> PendingForkControlDescriptor {
        debug_assert!(
            direct_attachment_fork_barrier()
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .fork_quiesced
        );
        register_pending_fork_control_file(release_writer)
    }
}

struct PendingForkControlDescriptor {
    fd: i32,
    writer: Option<std::fs::File>,
}

impl PendingForkControlDescriptor {
    fn write_release(&mut self) -> std::io::Result<()> {
        self.writer
            .as_mut()
            .expect("pending fork-control descriptor is present")
            .write_all(&[ATTACHMENT_RELEASE_TOKEN])
    }
}

impl Drop for PendingForkControlDescriptor {
    fn drop(&mut self) {
        let barrier = direct_attachment_fork_barrier();
        barrier
            .waiting_control_closers
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_add(1)
            })
            .expect("pending fork-control closer count overflow");
        let mut state = barrier
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while state.fork_quiesced {
            state = barrier
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        assert!(
            state.pending_fork_control_fds.remove(&self.fd),
            "pending fork-control descriptor was not registered"
        );
        // Close while the barrier state remains locked. A new fork cannot
        // observe the descriptor absent from the registry while it is still
        // open in the parent and therefore inheritable.
        drop(self.writer.take());
        let previous_closers = barrier
            .waiting_control_closers
            .fetch_sub(1, Ordering::SeqCst);
        assert_ne!(
            previous_closers, 0,
            "pending fork-control closer count underflow"
        );
        barrier.changed.notify_all();
    }
}

fn register_pending_fork_control_file(file: std::fs::File) -> PendingForkControlDescriptor {
    let fd = file.as_raw_fd();
    let barrier = direct_attachment_fork_barrier();
    let mut state = barrier
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        state.pending_fork_control_fds.insert(fd),
        "pending fork-control descriptor was already registered"
    );
    PendingForkControlDescriptor {
        fd,
        writer: Some(file),
    }
}

impl Drop for QuiescedForkSensitiveDescriptors {
    fn drop(&mut self) {
        let barrier = direct_attachment_fork_barrier();
        let mut state = barrier
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(state.fork_quiesced);
        state.fork_quiesced = false;
        barrier.changed.notify_all();
    }
}

fn quiesce_fork_sensitive_descriptors() -> Result<QuiescedForkSensitiveDescriptors, String> {
    let barrier = direct_attachment_fork_barrier();
    let owner = thread::current().id();
    let mut state = barrier
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.retained_scope_owners.contains_key(&owner) {
        return Err(
            "direct attachment fork requested while the calling thread retains fork-sensitive descriptor authority"
                .to_string(),
        );
    }
    state.waiting_forks = state
        .waiting_forks
        .checked_add(1)
        .expect("fork-sensitive descriptor waiter count overflow");
    while state.fork_quiesced
        || state.retained_scopes != 0
        || barrier.waiting_control_closers.load(Ordering::SeqCst) != 0
    {
        state = barrier
            .changed
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    state.waiting_forks = state
        .waiting_forks
        .checked_sub(1)
        .expect("fork-sensitive descriptor waiter count underflow");
    state.fork_quiesced = true;
    Ok(QuiescedForkSensitiveDescriptors)
}
#[cfg(unix)]
const ATTACHMENT_ABORT_SIGNAL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const ATTACHMENT_ABORT_SIGNAL: i32 = 9;

#[cfg(target_os = "linux")]
struct AttachmentWorkerGate {
    status_writer: std::fs::File,
    release_reader: std::fs::File,
    cwd_directory: Option<std::fs::File>,
    child_status_reader_fd: i32,
    child_release_writer_fd: i32,
    inherited_pending_control_fds: Vec<i32>,
}

/// A running subprocess that can be waited on later.
pub struct RunningProcess {
    /// Identity of the supervised command. For a direct launch this is the
    /// spawned child; for a trusted launcher it is the target reported over
    /// the status channel. Supervised targets share the outer launcher's PGID,
    /// which remains reserved by the retained [`process::Child`] even if the
    /// reported target exits before its same-group descendants.
    pub pid: u32,
    pub pgid: i64,
    /// The outer process is retained separately so timeout/overflow cleanup
    /// always reaps the launcher as well as the target process group.
    wrapper_pid: u32,
    wrapper_pgid: i64,
    child: process::Child,
    stdin_thread: Option<thread::JoinHandle<()>>,
    stdout_thread: Option<thread::JoinHandle<()>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
    status_thread: Option<thread::JoinHandle<()>>,
    stdout_capture: SharedCapture,
    stderr_capture: SharedCapture,
    drain_stop: Arc<AtomicBool>,
    output_overflow_rx: std::sync::mpsc::Receiver<CapturedStream>,
    start: Instant,
    timeout: f64,
    /// Present only for a trusted-launcher spawn whose target is blocked at
    /// the backend's final pre-exec boundary. Authoritative lifecycle callers
    /// release it only after persisting the exact reported process identity.
    attachment_release: Option<ProcessAttachmentRelease>,
    groups_terminated: bool,
    wrapper_reaped: bool,
}

/// A subprocess whose exact target identity exists, but whose target program
/// cannot execute until the caller durably attaches that identity.
///
/// This is a linear lifecycle state. It deliberately exposes neither `wait`
/// nor the underlying child handle. Callers must consume it by releasing only
/// after attachment, or by explicitly aborting and reaping it.
pub struct ProcessAwaitingAttachment {
    pid: u32,
    pgid: i64,
    owner: Option<AttachmentPendingOwner>,
    #[cfg(target_os = "linux")]
    pidfd: OwnedFd,
    request_deadline: Option<Instant>,
}

enum AttachmentPendingOwner {
    Direct {
        worker: thread::JoinHandle<Result<RunningProcess, SubprocessResult>>,
        release_registration: PendingForkControlDescriptor,
    },
    Supervised {
        running: Box<RunningProcess>,
    },
}

/// Proof that an attachment-pending process was explicitly aborted and its
/// `Command::spawn` worker settled without allowing target execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbortedProcess {
    pub pid: u32,
    pub pgid: i64,
}

/// Failure while crossing the attachment-to-running lifecycle boundary.
///
/// Before this is returned, the pending process and its process group are
/// proved quiescent and the exact child is reaped. No live process authority
/// is hidden inside the error.
#[derive(Debug)]
pub struct AttachmentReleaseError {
    pub phase: &'static str,
    pub result: SubprocessResult,
}

impl std::fmt::Display for AttachmentReleaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.phase, self.result.stderr)
    }
}

impl std::error::Error for AttachmentReleaseError {}

/// Failure of the caller-owned cleanup attempt. This error is returned only
/// after the attachment boundary has been revoked and exact cleanup has been
/// proved synchronously.
#[derive(Debug)]
pub struct AttachmentAbortError {
    pub pid: u32,
    pub detail: String,
}

impl std::fmt::Display for AttachmentAbortError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "abort process {} awaiting attachment: {}",
            self.pid, self.detail
        )
    }
}

impl std::error::Error for AttachmentAbortError {}

impl ProcessAwaitingAttachment {
    /// Exact PID reported while the child was held after session creation.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Exact process group, proved to be led by [`Self::pid`].
    pub fn pgid(&self) -> i64 {
        self.pgid
    }

    /// Borrow the already-pinned exact process identity. Durable lifecycle
    /// code must capture identity through this descriptor rather than reopen a
    /// potentially recycled numeric PID.
    #[cfg(target_os = "linux")]
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }

    /// Release the child only after its exact identity has been durably
    /// attached, then recover the ordinary `RunningProcess` produced by
    /// `Command::spawn` after exec crosses Rust's normal error boundary.
    pub fn release_after_attachment(mut self) -> Result<RunningProcess, AttachmentReleaseError> {
        if self
            .request_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let cleanup = self
                .abort_and_reap_inner()
                .map(|_| ())
                .map_err(|error| error.to_string());
            let cleanup = self.cleanup_failure_detail(cleanup);
            let result = spawn_failure(
                Instant::now(),
                format!(
                    "release after attachment refused: request deadline expired before release{cleanup}",
                ),
            );
            return Err(AttachmentReleaseError {
                phase: "release after attachment",
                result,
            });
        }
        if let Err(error) = self.check_exact_process_alive() {
            let cleanup = self
                .abort_and_reap_inner()
                .map(|_| ())
                .map_err(|error| error.to_string());
            let cleanup = self.cleanup_failure_detail(cleanup);
            let result = spawn_failure(
                Instant::now(),
                format!("release after attachment refused: {error}{cleanup}"),
            );
            return Err(AttachmentReleaseError {
                phase: "release after attachment",
                result,
            });
        }

        let owner = self.owner.take().expect("attachment owner is present");
        match owner {
            AttachmentPendingOwner::Direct {
                worker,
                mut release_registration,
            } => {
                if let Err(error) = release_registration.write_release() {
                    drop(release_registration);
                    let settlement = prove_attachment_cleanup(
                        self.pidfd.as_raw_fd(),
                        settle_direct_attachment_worker(self.pid, worker),
                    );
                    let detail = self.cleanup_failure_detail(settlement);
                    return Err(AttachmentReleaseError {
                        phase: "release after attachment",
                        result: spawn_failure(
                            Instant::now(),
                            format!("release after attachment failed: {error}{detail}"),
                        ),
                    });
                }
                drop(release_registration);
                match worker.join() {
                    Ok(Ok(running)) => Ok(running),
                    Ok(Err(result)) => {
                        let cleanup = wait_pidfd_exit(
                            self.pidfd.as_raw_fd(),
                            ATTACHMENT_ABORT_SETTLE_TIMEOUT,
                        );
                        let detail = self.cleanup_failure_detail(cleanup);
                        Err(AttachmentReleaseError {
                            phase: "exec after attachment release",
                            result: if detail.is_empty() {
                                result
                            } else {
                                spawn_failure(Instant::now(), format!("{}{detail}", result.stderr))
                            },
                        })
                    }
                    Err(_) => {
                        let cleanup = cleanup_direct_after_release_worker_panic(
                            self.pid,
                            self.pgid,
                            self.pidfd.as_raw_fd(),
                        );
                        let detail = self.cleanup_failure_detail(cleanup);
                        Err(AttachmentReleaseError {
                            phase: "exec after attachment release",
                            result: spawn_failure(
                                Instant::now(),
                                format!("attachment spawn worker panicked after release{detail}"),
                            ),
                        })
                    }
                }
            }
            AttachmentPendingOwner::Supervised { mut running } => {
                if let Err(error) = running.validate_attachment_release_ready() {
                    let cleanup = prove_attachment_cleanup(
                        self.pidfd.as_raw_fd(),
                        running.abort_and_reap_checked(),
                    );
                    let detail = self.cleanup_failure_detail(cleanup);
                    return Err(AttachmentReleaseError {
                        phase: "release after attachment",
                        result: spawn_failure(
                            Instant::now(),
                            format!(
                                "supervised target was not releasable after attachment: {error}{detail}",
                            ),
                        ),
                    });
                }
                match running.release_attachment_boundary() {
                    Ok(()) => Ok(*running),
                    Err(error) => {
                        let cleanup = prove_attachment_cleanup(
                            self.pidfd.as_raw_fd(),
                            running.abort_and_reap_checked(),
                        );
                        let detail = self.cleanup_failure_detail(cleanup);
                        Err(AttachmentReleaseError {
                            phase: "release after attachment",
                            result: spawn_failure(
                                Instant::now(),
                                format!(
                                    "release supervised target after attachment: {error}{detail}",
                                ),
                            ),
                        })
                    }
                }
            }
        }
    }

    /// Fail closed, terminate the exact held child through its pidfd, and join
    /// the spawn worker so no child or zombie remains owned by this handle.
    pub fn abort_and_reap(mut self) -> Result<AbortedProcess, AttachmentAbortError> {
        match self.abort_and_reap_inner() {
            Ok(aborted) => Ok(aborted),
            Err(_error) => {
                #[cfg(target_os = "linux")]
                {
                    // Do not unwind into a durable lifecycle owner while an
                    // exact process can still be live. Keeping this call
                    // synchronous also keeps the caller's durable attachment
                    // row authoritative if the daemon dies during cleanup.
                    complete_attachment_cleanup(self.pidfd.as_raw_fd(), self.pgid);
                    Ok(AbortedProcess {
                        pid: self.pid,
                        pgid: self.pgid,
                    })
                }
                #[cfg(not(target_os = "linux"))]
                Err(_error)
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn cleanup_failure_detail(&self, cleanup: Result<(), String>) -> String {
        match cleanup {
            Ok(()) => String::new(),
            Err(error) => {
                // A release error may escape only after exact cleanup proof;
                // otherwise RyeOS could compare-clear the durable attachment
                // while this process remained live.
                complete_attachment_cleanup(self.pidfd.as_raw_fd(), self.pgid);
                format!("; initial cleanup proof failed: {error}; cleanup completed synchronously")
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn cleanup_failure_detail(&self, cleanup: Result<(), String>) -> String {
        cleanup
            .err()
            .map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
    }

    fn abort_and_reap_inner(&mut self) -> Result<AbortedProcess, AttachmentAbortError> {
        let Some(owner) = self.owner.take() else {
            return Ok(AbortedProcess {
                pid: self.pid,
                pgid: self.pgid,
            });
        };
        let mut cleanup_errors = Vec::new();
        if let Err(error) = self.signal_exact_process(ATTACHMENT_ABORT_SIGNAL) {
            cleanup_errors.push(error);
        }
        let result = match owner {
            AttachmentPendingOwner::Direct {
                worker,
                release_registration,
            } => {
                // EOF is refusal, never release. Closing this authority also
                // wakes a child that raced with the exact signal.
                drop(release_registration);
                settle_direct_attachment_worker(self.pid, worker)
            }
            AttachmentPendingOwner::Supervised { running } => running.abort_and_reap_checked(),
        };
        match result {
            Ok(()) => {
                // The structured owner proves both group quiescence and
                // leader reaping. A preceding signal error is immaterial
                // once that stronger proof exists, and retrying by numeric
                // PGID after reap would itself be unsafe.
                return Ok(AbortedProcess {
                    pid: self.pid,
                    pgid: self.pgid,
                });
            }
            Err(error) => cleanup_errors.push(error),
        }
        #[cfg(target_os = "linux")]
        match force_attachment_cleanup(
            self.pgid,
            self.pidfd.as_raw_fd(),
            ATTACHMENT_ABORT_SETTLE_TIMEOUT,
        ) {
            Ok(()) => {
                return Ok(AbortedProcess {
                    pid: self.pid,
                    pgid: self.pgid,
                });
            }
            Err(error) => cleanup_errors.push(error),
        }
        #[cfg(not(target_os = "linux"))]
        if cleanup_errors.is_empty() {
            return Ok(AbortedProcess {
                pid: self.pid,
                pgid: self.pgid,
            });
        }
        if !cleanup_errors.is_empty() {
            return Err(AttachmentAbortError {
                pid: self.pid,
                detail: cleanup_errors.join("; "),
            });
        }
        Ok(AbortedProcess {
            pid: self.pid,
            pgid: self.pgid,
        })
    }

    #[cfg(target_os = "linux")]
    fn check_exact_process_alive(&self) -> Result<(), String> {
        pidfd_send_signal(self.pidfd.as_raw_fd(), 0)?;
        let pid = i32::try_from(self.pid).map_err(|_| "PID exceeds pid_t".to_string())?;
        let observed_pgid = unsafe { libc::getpgid(pid) };
        if observed_pgid < 0 {
            return Err(format!(
                "inspect attachment process group: {}",
                std::io::Error::last_os_error()
            ));
        }
        if observed_pgid as i64 != self.pgid {
            return Err(format!(
                "process {} escaped retained attachment group {} (observed {observed_pgid})",
                self.pid, self.pgid
            ));
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn check_exact_process_alive(&self) -> Result<(), String> {
        Err("attachment-before-execution is supported only on Linux".to_string())
    }

    #[cfg(target_os = "linux")]
    fn signal_exact_process(&self, signal: i32) -> Result<(), String> {
        match pidfd_send_signal_io(self.pidfd.as_raw_fd(), signal) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            Err(error) => Err(format!("pidfd_send_signal({signal}): {error}")),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn signal_exact_process(&self, _signal: i32) -> Result<(), String> {
        Ok(())
    }
}

impl Drop for ProcessAwaitingAttachment {
    fn drop(&mut self) {
        if self.abort_and_reap_inner().is_err() {
            #[cfg(target_os = "linux")]
            {
                // Drop is also a linear lifecycle boundary. Never let an
                // attached-process guard clear durable ownership while exact
                // cleanup is merely outstanding in another in-process task.
                complete_attachment_cleanup(self.pidfd.as_raw_fd(), self.pgid);
            }
        }
    }
}

impl RunningProcess {
    fn validate_attachment_release_ready(&mut self) -> Result<(), String> {
        let stdout_truncated = self
            .stdout_capture
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .truncated;
        let stderr_truncated = self
            .stderr_capture
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .truncated;
        if stdout_truncated || stderr_truncated || self.output_overflow_rx.try_recv().is_ok() {
            return Err(
                "launcher output exceeded its configured bound while the target awaited attachment"
                    .to_string(),
            );
        }
        match poll_wrapper(&mut self.child) {
            Ok(WrapperPoll::Running) => Ok(()),
            Ok(WrapperPoll::ExitedUnreaped) => {
                Err("supervised launcher exited before target release".to_string())
            }
            #[cfg(not(target_os = "linux"))]
            Ok(WrapperPoll::ExitedReaped(_)) => {
                self.wrapper_reaped = true;
                Err("supervised launcher exited before target release".to_string())
            }
            Err(error) => Err(format!(
                "inspect supervised launcher before target release: {error}"
            )),
        }
    }

    /// Release a trusted launcher's target after durable process attachment.
    ///
    /// A failed write leaves the process fail-closed; the caller must abort or
    /// drop this handle.
    fn release_attachment_boundary(&mut self) -> Result<(), String> {
        let mut boundary = self.attachment_release.take().ok_or_else(|| {
            "supervised target attachment authority disappeared before release".to_string()
        })?;
        let Some(mut writer) = boundary.writer.take() else {
            return Err("supervised target attachment authority was already consumed".to_string());
        };
        writer
            .write_all(&[ATTACHMENT_RELEASE_TOKEN])
            .map_err(|error| format!("release supervised target after attachment: {error}"))?;
        // Closing the descriptor makes the one-shot boundary explicit and
        // prevents a retained writer from hiding backend failure.
        drop(writer);
        Ok(())
    }

    /// Terminate every supervised process group and reap the outer child.
    ///
    /// This consumes the handle so callers cannot accidentally wait on or
    /// publish an execution after aborting it. Dropping a handle without
    /// calling either `wait` or `abort` performs the same fail-safe cleanup.
    pub fn abort(mut self) {
        let _ = self.abort_and_reap_inner();
    }

    /// Abort and prove that the retained wrapper child was reaped. Lifecycle
    /// state machines use this checked form when cleanup is part of a durable
    /// transition rather than a best-effort drop backstop.
    pub fn abort_and_reap_checked(mut self) -> Result<(), String> {
        self.abort_and_reap_inner()
    }

    /// Wait for the process to finish (or time out) and return the result.
    pub fn wait(mut self) -> SubprocessResult {
        if self.attachment_release.is_some() {
            self.kill_supervised_processes();
            self.reap_wrapper();
            let (out, err) = self.finish_drains();
            return SubprocessResult {
                success: false,
                stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
                stderr: append_diagnostic(
                    &String::from_utf8_lossy(&err.bytes),
                    "Refused to wait: supervised target was not released after durable attachment",
                ),
                exit_code: -1,
                duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
                pid: self.pid,
                timed_out: false,
                launcher_refusal: None,
                output_limit_exceeded: output_limit_exceeded(&out, &err),
                stdout_truncated: out.truncated,
                stderr_truncated: err.truncated,
            };
        }
        let timeout = request_timeout_duration(self.timeout);

        loop {
            match poll_wrapper(&mut self.child) {
                Ok(WrapperPoll::ExitedUnreaped) => {
                    // Preserve the wrapper as an unreaped zombie until every
                    // owned group has been revalidated and signalled. Its PID
                    // cannot be recycled during this window.
                    self.kill_supervised_processes();
                    match self.child.wait() {
                        Ok(status) => {
                            self.wrapper_reaped = true;
                            return self.completed_result(status);
                        }
                        Err(error) => return self.wait_error_result(error),
                    }
                }
                #[cfg(not(target_os = "linux"))]
                Ok(WrapperPoll::ExitedReaped(status)) => {
                    self.wrapper_reaped = true;
                    // On targets without WNOWAIT, revalidation safely skips a
                    // vanished leader rather than signalling a recycled PGID.
                    self.kill_supervised_processes();
                    return self.completed_result(status);
                }
                Ok(WrapperPoll::Running) => {
                    if self.output_overflow_rx.try_recv().is_ok() {
                        self.kill_supervised_processes();
                        self.reap_wrapper();
                        let (out, err) = self.finish_drains();
                        let exceeded = output_limit_exceeded(&out, &err)
                            .expect("overflow notification requires a truncated capture");
                        return self.output_limit_result(out, err, exceeded);
                    }
                    if timeout.is_some_and(|limit| self.start.elapsed() >= limit) {
                        self.kill_supervised_processes();
                        self.reap_wrapper();
                        let (out, err) = self.finish_drains();
                        return self.timeout_result(out, err);
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(error) => return self.wait_error_result(error),
            }
        }
    }

    fn completed_result(&mut self, status: process::ExitStatus) -> SubprocessResult {
        let (out, err) = self.finish_drains();
        let code = status.code().unwrap_or(-1);
        if let Some(exceeded) = output_limit_exceeded(&out, &err) {
            return self.output_limit_result(out, err, exceeded);
        }
        SubprocessResult {
            success: code == 0,
            stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
            stderr: String::from_utf8_lossy(&err.bytes).into_owned(),
            exit_code: code,
            duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
            pid: self.pid,
            timed_out: false,
            launcher_refusal: None,
            output_limit_exceeded: None,
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    fn wait_error_result(&mut self, error: std::io::Error) -> SubprocessResult {
        // A failed observation/reap must not silently orphan the supervised
        // command or its launcher.
        self.kill_supervised_processes();
        self.reap_wrapper();
        let (out, err) = self.finish_drains();
        SubprocessResult {
            success: false,
            stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
            stderr: append_diagnostic(
                &String::from_utf8_lossy(&err.bytes),
                &format!("Wait failed: {error}"),
            ),
            exit_code: -1,
            duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
            pid: self.pid,
            timed_out: false,
            launcher_refusal: None,
            output_limit_exceeded: output_limit_exceeded(&out, &err),
            stdout_truncated: out.truncated,
            stderr_truncated: err.truncated,
        }
    }

    fn kill_supervised_processes(&mut self) {
        if self.groups_terminated {
            return;
        }
        #[cfg(unix)]
        {
            debug_assert_eq!(self.wrapper_pgid, self.wrapper_pid as i64);
            debug_assert_eq!(self.pgid, self.wrapper_pgid);
            // Lillux creates the outer launcher as a session/group leader and
            // retains its Child handle until this cleanup completes. The live
            // or unreaped leader therefore reserves the numeric PGID while the
            // signal is sent, even if the launcher already reaped its reported
            // target leader. Sandboxed durable launches negotiate a backend
            // that prevents descendants from escaping this process group.
            kill_owned_process_group(self.wrapper_pid, self.wrapper_pgid, !self.wrapper_reaped);
            // `Child` still owns the wrapper PID until it is reaped, so this
            // exact-PID fallback cannot hit a recycled process. It covers a
            // wrapper that moved groups or a group signal refused by the OS.
            if !self.wrapper_reaped {
                let _ = self.child.kill();
            }
        }
        #[cfg(not(unix))]
        {
            let _wrapper_pid = self.wrapper_pid;
            let _ = self.child.kill();
        }
        self.groups_terminated = true;
    }

    fn reap_wrapper(&mut self) {
        if self.wrapper_reaped {
            return;
        }
        if self.child.wait().is_ok() {
            self.wrapper_reaped = true;
        }
    }

    fn abort_and_reap_inner(&mut self) -> Result<(), String> {
        self.kill_supervised_processes();
        #[cfg(target_os = "linux")]
        let group_result = self.settle_owned_group_before_wrapper_reap();
        #[cfg(not(target_os = "linux"))]
        let group_result = Ok(());
        // The unreaped wrapper is the process-group identity fence. Reaping
        // it before group quiescence is proved would leave only a numeric
        // PGID, which may later be reused. Keep it owned across retries.
        let reap_result = match &group_result {
            Ok(()) => self.reap_wrapper_checked(),
            Err(_) => Ok(()),
        };
        let _ = self.finish_drains();
        match (group_result, reap_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(group), Ok(())) => Err(group),
            (Ok(()), Err(reap)) => Err(reap),
            (Err(group), Err(reap)) => Err(format!("{group}; {reap}")),
        }
    }

    #[cfg(target_os = "linux")]
    fn settle_owned_group_before_wrapper_reap(&mut self) -> Result<(), String> {
        let deadline = Instant::now()
            .checked_add(ATTACHMENT_ABORT_SETTLE_TIMEOUT)
            .ok_or_else(|| "process-group cleanup deadline overflow".to_string())?;
        loop {
            match poll_wrapper(&mut self.child) {
                Ok(WrapperPoll::ExitedUnreaped) => break,
                Ok(WrapperPoll::Running) => {
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "wrapper {} did not exit before cleanup deadline",
                            self.wrapper_pid
                        ));
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(error) => {
                    return Err(format!(
                        "observe wrapper {} exit before reap: {error}",
                        self.wrapper_pid
                    ));
                }
            }
        }
        wait_owned_process_group_quiescent(
            self.wrapper_pgid,
            self.wrapper_pid,
            deadline.saturating_duration_since(Instant::now()),
        )
    }

    fn reap_wrapper_checked(&mut self) -> Result<(), String> {
        if self.wrapper_reaped {
            return Ok(());
        }
        match self.child.wait() {
            Ok(_) => {
                self.wrapper_reaped = true;
                Ok(())
            }
            Err(error) => Err(format!(
                "reap supervised wrapper process {}: {error}",
                self.wrapper_pid
            )),
        }
    }

    fn finish_drains(&mut self) -> (BoundedCapture, BoundedCapture) {
        // Once the wrapper has exited (or has been killed), consume bytes that
        // are already buffered and stop at the next WouldBlock or after a
        // fixed number of post-stop reads. The latter bound prevents an
        // escaped setsid descendant that keeps writing from hanging cleanup,
        // while preserving ordinary output already present in the pipe.
        self.drain_stop.store(true, Ordering::Release);
        if let Some(handle) = self.stdin_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.status_thread.take() {
            let _ = handle.join();
        }
        (
            take_capture(&self.stdout_capture),
            take_capture(&self.stderr_capture),
        )
    }

    fn timeout_result(&self, out: BoundedCapture, err: BoundedCapture) -> SubprocessResult {
        SubprocessResult {
            success: false,
            stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
            stderr: append_diagnostic(
                &String::from_utf8_lossy(&err.bytes),
                &format!("Command timed out after {} seconds", self.timeout),
            ),
            exit_code: -1,
            duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
            pid: self.pid,
            timed_out: true,
            launcher_refusal: None,
            output_limit_exceeded: output_limit_exceeded(&out, &err),
            stdout_truncated: out.truncated,
            stderr_truncated: err.truncated,
        }
    }

    fn output_limit_result(
        &self,
        out: BoundedCapture,
        err: BoundedCapture,
        exceeded: OutputLimitExceeded,
    ) -> SubprocessResult {
        SubprocessResult {
            success: false,
            stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
            stderr: append_diagnostic(
                &String::from_utf8_lossy(&err.bytes),
                &format!(
                    "Command exceeded the node-owned {} output retention limit and was terminated",
                    exceeded.as_str()
                ),
            ),
            exit_code: -1,
            duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
            pid: self.pid,
            timed_out: false,
            launcher_refusal: None,
            output_limit_exceeded: Some(exceeded),
            stdout_truncated: out.truncated,
            stderr_truncated: err.truncated,
        }
    }
}

impl Drop for RunningProcess {
    fn drop(&mut self) {
        let _ = self.abort_and_reap_inner();
    }
}

#[cfg(target_os = "linux")]
fn poll_wrapper(child: &mut process::Child) -> std::io::Result<WrapperPoll> {
    let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return if unsafe { status.si_pid() } == 0 {
                Ok(WrapperPoll::Running)
            } else {
                Ok(WrapperPoll::ExitedUnreaped)
            };
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn poll_wrapper(child: &mut process::Child) -> std::io::Result<WrapperPoll> {
    child.try_wait().map(|status| match status {
        Some(status) => WrapperPoll::ExitedReaped(status),
        None => WrapperPoll::Running,
    })
}

// ---------------------------------------------------------------------------
// Library functions — public API for in-process callers
// ---------------------------------------------------------------------------

/// Spawn a subprocess and return a handle that can be waited on later.
pub fn lib_spawn(request: SubprocessRequest) -> Result<RunningProcess, SubprocessResult> {
    if request
        .supervised_status
        .as_ref()
        .is_some_and(|status| matches!(status, SupervisedProcessStatus::AwaitingAttachment { .. }))
    {
        return Err(spawn_failure(
            Instant::now(),
            "Failed to spawn: attachment-bearing supervision requires spawn_awaiting_attachment",
        ));
    }
    lib_spawn_with_stdio(request, false, None)
}

/// Spawn with inherited terminal stdio while retaining the same session,
/// supervised-launcher status, timeout, process-group cleanup, and wait
/// contract as captured execution.
pub fn lib_spawn_inherited_stdio(
    request: SubprocessRequest,
) -> Result<RunningProcess, SubprocessResult> {
    if request
        .supervised_status
        .as_ref()
        .is_some_and(|status| matches!(status, SupervisedProcessStatus::AwaitingAttachment { .. }))
    {
        return Err(spawn_failure(
            Instant::now(),
            "Failed to spawn: attachment-bearing supervision requires spawn_awaiting_attachment",
        ));
    }
    lib_spawn_with_stdio(request, true, None)
}

/// Spawn a Linux subprocess whose final trusted setup completes before the
/// exact target PID/PGID is returned, while its target program remains unable
/// to execute until [`ProcessAwaitingAttachment::release_after_attachment`].
///
/// Normal [`lib_spawn`] semantics are unchanged. This explicit operation is
/// reserved for daemon-owned executions that must durably persist process
/// ownership before any target code can run.
#[cfg(target_os = "linux")]
pub fn lib_spawn_awaiting_attachment(
    mut request: SubprocessRequest,
) -> Result<ProcessAwaitingAttachment, SubprocessResult> {
    let start = Instant::now();
    if let Some(status) = request.supervised_status.as_ref() {
        if !matches!(status, SupervisedProcessStatus::AwaitingAttachment { .. }) {
            return Err(spawn_failure(
                start,
                "Failed to spawn awaiting attachment: supervised backend omitted its required target attachment boundary",
            ));
        }
        let timeout = request.timeout;
        let running = lib_spawn_with_stdio(request, false, None)?;
        if running.attachment_release.is_none() {
            let error = spawn_failure(
                start,
                "Failed to spawn awaiting attachment: supervised target attachment boundary disappeared",
            );
            running.abort_and_reap_checked().map_err(|cleanup| {
                spawn_failure(
                    start,
                    format!("{}; cleanup failed: {cleanup}", error.stderr),
                )
            })?;
            return Err(error);
        }
        let observed_birth = match read_linux_process_birth(running.pid) {
            Ok(birth) => birth,
            Err(error) => {
                let cleanup = running.abort_and_reap_checked().err();
                return Err(spawn_failure(
                    start,
                    format!(
                        "Failed to inspect supervised target awaiting attachment: {error}{}",
                        cleanup
                            .map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
                    ),
                ));
            }
        };
        let pidfd = match open_pidfd(running.pid) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                let cleanup = running.abort_and_reap_checked().err();
                return Err(spawn_failure(
                    start,
                    format!(
                        "Failed to pin supervised target awaiting attachment: {error}{}",
                        cleanup
                            .map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
                    ),
                ));
            }
        };
        if let Err(error) = validate_pinned_process_birth(
            running.pid,
            running.pgid,
            None,
            &observed_birth,
            pidfd.as_raw_fd(),
        )
        .and_then(|_| {
            validate_supervised_attachment_target(running.pid, running.pgid, pidfd.as_raw_fd())
        }) {
            let cleanup = running.abort_and_reap_checked().err();
            return Err(spawn_failure(
                start,
                format!(
                    "Invalid supervised target awaiting attachment: {error}{}",
                    cleanup.map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
                ),
            ));
        }
        return Ok(ProcessAwaitingAttachment {
            pid: running.pid,
            pgid: running.pgid,
            owner: Some(AttachmentPendingOwner::Supervised {
                running: Box::new(running),
            }),
            pidfd,
            request_deadline: request_timeout_duration(timeout)
                .and_then(|duration| start.checked_add(duration)),
        });
    }
    let timeout = request.timeout;
    let cwd_directory = match request.cwd.take() {
        Some(path) => Some(open_attachment_cwd(&path, start)?),
        None => None,
    };
    // No other direct child may fork while these control pipes are created.
    // Snapshot the control descriptors of already-held children so the new
    // child can close only those known authorities at its final setup hook.
    let fork_sensitive_descriptors = quiesce_fork_sensitive_descriptors().map_err(|error| {
        spawn_failure(
            start,
            format!("Failed to spawn awaiting attachment: {error}"),
        )
    })?;
    let inherited_pending_control_fds = fork_sensitive_descriptors.pending_fork_control_fds();
    let (status_reader, status_writer) = attachment_pipe("readiness", start)?;
    let (release_reader, release_writer) = attachment_pipe("release", start)?;
    let child_status_reader_fd = status_reader.as_raw_fd();
    let child_release_writer_fd = release_writer.as_raw_fd();
    let gate = AttachmentWorkerGate {
        status_writer,
        release_reader,
        cwd_directory,
        child_status_reader_fd,
        child_release_writer_fd,
        inherited_pending_control_fds,
    };

    // A child held before exec retains every CLOEXEC descriptor inherited at
    // fork. Quiesce scopes which own descriptor-backed authority until the
    // worker reports the final hold boundary; otherwise a concurrent child can
    // inherit an advisory lock and deadlock the owner's durable attach path.
    let worker = thread::Builder::new()
        .name("lillux-attachment-spawn".to_string())
        .spawn(move || lib_spawn_with_stdio(request, false, Some(gate)))
        .map_err(|error| {
            spawn_failure(
                start,
                format!("Failed to spawn awaiting attachment worker: {error}"),
            )
        })?;

    let setup_deadline = supervised_setup_deadline(start, timeout);
    let identity =
        match read_attachment_ready(&status_reader, setup_deadline, ATTACHMENT_IDENTITY_PHASE) {
            Ok(identity) => identity,
            Err(error) => {
                drop(release_writer);
                let worker_detail = match worker.join() {
                    Ok(Err(result)) if !result.stderr.is_empty() => format!("; {}", result.stderr),
                    Ok(Ok(running)) => {
                        running.abort();
                        String::new()
                    }
                    Ok(Err(_)) => String::new(),
                    Err(_) => "; attachment spawn worker panicked".to_string(),
                };
                return Err(spawn_failure(
                    start,
                    format!("Failed to spawn awaiting attachment: {error}{worker_detail}"),
                ));
            }
        };
    if let Err(error) = validate_direct_attachment_identity(identity.pid, identity.pgid) {
        drop(release_writer);
        let cleanup = settle_direct_attachment_worker(identity.pid, worker).err();
        return Err(spawn_failure(
            start,
            format!(
                "Failed to spawn awaiting attachment: {error}{}",
                cleanup.map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
            ),
        ));
    }
    let observed_birth = match read_linux_process_birth(identity.pid) {
        Ok(birth) => birth,
        Err(error) => {
            drop(release_writer);
            let cleanup = settle_direct_attachment_worker(identity.pid, worker).err();
            return Err(spawn_failure(
                start,
                format!(
                    "Failed to inspect process awaiting attachment: {error}{}",
                    cleanup.map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
                ),
            ));
        }
    };
    let pidfd = match open_pidfd(identity.pid) {
        Ok(pidfd) => pidfd,
        Err(error) => {
            drop(release_writer);
            let cleanup = settle_direct_attachment_worker(identity.pid, worker).err();
            return Err(spawn_failure(
                start,
                format!(
                    "Failed to pin process awaiting attachment: {error}{}",
                    cleanup.map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
                ),
            ));
        }
    };
    if let Err(error) = validate_pinned_process_birth(
        identity.pid,
        identity.pgid,
        Some(process::id()),
        &observed_birth,
        pidfd.as_raw_fd(),
    ) {
        drop(release_writer);
        let cleanup = settle_direct_attachment_worker(identity.pid, worker).err();
        return Err(spawn_failure(
            start,
            format!(
                "Process identity changed while awaiting attachment: {error}{}",
                cleanup.map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
            ),
        ));
    }
    let ready = match read_attachment_ready(&status_reader, setup_deadline, ATTACHMENT_READY_PHASE)
    {
        Ok(ready) if ready.pid == identity.pid && ready.pgid == identity.pgid => ready,
        Ok(ready) => {
            drop(release_writer);
            let cleanup = settle_direct_attachment_worker(identity.pid, worker).err();
            return Err(spawn_failure(
                start,
                format!(
                    "Failed to spawn awaiting attachment: readiness identity changed from {}/{} to {}/{}{}",
                    identity.pid,
                    identity.pgid,
                    ready.pid,
                    ready.pgid,
                    cleanup.map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
                ),
            ));
        }
        Err(error) => {
            let _ = pidfd_send_signal(pidfd.as_raw_fd(), ATTACHMENT_ABORT_SIGNAL);
            drop(release_writer);
            let cleanup = settle_direct_attachment_worker(identity.pid, worker).err();
            return Err(spawn_failure(
                start,
                format!(
                    "Failed to reach final attachment boundary: {error}{}",
                    cleanup.map_or_else(String::new, |error| format!("; cleanup failed: {error}"))
                ),
            ));
        }
    };
    // Register the one-shot parent release authority before reopening the fork
    // window. Later direct children close this exact known descriptor in their
    // own pre-exec hook, without touching Rust's private exec-error channel or
    // any caller-declared inherited descriptor.
    let release_registration =
        fork_sensitive_descriptors.register_pending_fork_control(release_writer);
    drop(fork_sensitive_descriptors);

    Ok(ProcessAwaitingAttachment {
        pid: ready.pid,
        pgid: ready.pgid,
        owner: Some(AttachmentPendingOwner::Direct {
            worker,
            release_registration,
        }),
        pidfd,
        request_deadline: request_timeout_duration(timeout)
            .and_then(|duration| start.checked_add(duration)),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn lib_spawn_awaiting_attachment(
    _request: SubprocessRequest,
) -> Result<ProcessAwaitingAttachment, SubprocessResult> {
    Err(spawn_failure(
        Instant::now(),
        "Failed to spawn awaiting attachment: supported only on Linux",
    ))
}

fn lib_spawn_with_stdio(
    request: SubprocessRequest,
    inherit_stdio: bool,
    #[cfg(target_os = "linux")] attachment_gate: Option<AttachmentWorkerGate>,
    #[cfg(not(target_os = "linux"))] _attachment_gate: Option<()>,
) -> Result<RunningProcess, SubprocessResult> {
    let start = Instant::now();
    let SubprocessRequest {
        cmd,
        argv0,
        args,
        cwd,
        envs,
        stdin_data,
        timeout,
        limits,
        inherited_fds,
        supervised_status,
    } = request;
    if inherit_stdio
        && limits.as_ref().is_some_and(|limits| {
            limits.max_stdout_bytes.is_some() || limits.max_stderr_bytes.is_some()
        })
    {
        return Err(spawn_failure(
            start,
            "Failed to spawn: inherited stdio cannot enforce captured-output byte limits",
        ));
    }

    #[cfg(unix)]
    let raw_inherited_fds = {
        use std::os::fd::AsRawFd as _;

        let mut raw = Vec::with_capacity(inherited_fds.len());
        for file in &inherited_fds {
            let fd = file.as_raw_fd();
            if fd <= libc::STDERR_FILENO {
                return Err(spawn_failure(
                    start,
                    format!("Failed to spawn: inherited descriptor {fd} overlaps stdio"),
                ));
            }
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 {
                return Err(spawn_failure(
                    start,
                    format!(
                        "Failed to spawn: inherited descriptor {fd} cannot be inspected: {}",
                        std::io::Error::last_os_error()
                    ),
                ));
            }
            if flags & libc::FD_CLOEXEC == 0 {
                return Err(spawn_failure(
                    start,
                    format!(
                        "Failed to spawn: inherited descriptor {fd} is not protected by FD_CLOEXEC"
                    ),
                ));
            }
            raw.push(fd);
        }
        raw
    };
    #[cfg(not(unix))]
    if !inherited_fds.is_empty() {
        return Err(spawn_failure(
            start,
            "Failed to spawn: inherited descriptors are unsupported on this platform",
        ));
    }
    #[cfg(not(target_os = "linux"))]
    if supervised_status.is_some() {
        return Err(spawn_failure(
            start,
            "Failed to spawn: supervised launcher status is supported only on Linux",
        ));
    }

    let envs_str: Vec<String> = envs.iter().map(|(k, v)| format!("{k}={v}")).collect();

    let mut command = process::Command::new(&cmd);
    #[cfg(unix)]
    if let Some(argv0) = argv0 {
        use std::os::unix::process::CommandExt as _;
        command.arg0(argv0);
    }
    #[cfg(not(unix))]
    if argv0.is_some() {
        return Err(spawn_failure(
            start,
            "Failed to spawn: custom argv[0] is unsupported on this platform",
        ));
    }
    command.args(&args);
    command.env_clear();
    set_envs(&mut command, &envs_str);
    if let Some(ref dir) = cwd {
        command.current_dir(dir);
    }
    if inherit_stdio && stdin_data.is_some() {
        return Err(spawn_failure(
            start,
            "Failed to spawn: inherited stdio cannot also carry buffered stdin data",
        ));
    }
    command.stdin(if inherit_stdio {
        Stdio::inherit()
    } else if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(if inherit_stdio {
        Stdio::inherit()
    } else {
        Stdio::piped()
    });
    command.stderr(if inherit_stdio {
        Stdio::inherit()
    } else {
        Stdio::piped()
    });
    // `inherited_fds` remains owned in this scope through `Command::spawn`.
    // Descriptors stay CLOEXEC in the multithreaded parent and are made
    // inheritable only in the forked child, preventing unrelated concurrent
    // spawns from receiving them.

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // Resolve the initiating parent identity before fork. The post-fork
        // hook must remain allocation-free and may only compare this plain
        // pid_t with getppid(). PID 1 is a valid initiating parent inside a
        // PID namespace; only a change away from this exact identity proves
        // that the child was reparented.
        let expected_parent_pid = if attachment_gate.is_some() {
            let pid = libc::pid_t::try_from(process::id()).map_err(|_| {
                spawn_failure(
                    start,
                    "Failed to spawn awaiting attachment: parent PID exceeds pid_t",
                )
            })?;
            if pid <= 0 {
                return Err(spawn_failure(
                    start,
                    "Failed to spawn awaiting attachment: parent PID is not positive",
                ));
            }
            Some(pid)
        } else {
            None
        };
        let attachment_setup =
            attachment_gate
                .as_ref()
                .zip(expected_parent_pid)
                .map(|(gate, expected_parent_pid)| {
                    (
                        expected_parent_pid,
                        gate.status_writer.as_raw_fd(),
                        gate.child_status_reader_fd,
                        gate.child_release_writer_fd,
                        gate.cwd_directory
                            .as_ref()
                            .map(|directory| directory.as_raw_fd()),
                        gate.inherited_pending_control_fds.clone(),
                    )
                });
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for fd in &raw_inherited_fds {
                    let flags = libc::fcntl(*fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if let Some((
                    expected_parent_pid,
                    status_writer,
                    status_reader,
                    release_writer,
                    cwd_directory,
                    pending_control_fds,
                )) = &attachment_setup
                {
                    direct_attachment_identity_pre_exec(
                        *expected_parent_pid,
                        *status_writer,
                        *status_reader,
                        *release_writer,
                        *cwd_directory,
                        pending_control_fds,
                    )?;
                }
                Ok(())
            });
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for fd in &raw_inherited_fds {
                    let flags = libc::fcntl(*fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }

    if let Err(reason) = configure_subprocess_limits(&mut command, limits.as_ref()) {
        return Err(spawn_failure(
            start,
            format!("Failed to spawn: invalid resource limits: {reason}"),
        ));
    }

    // This hook is deliberately registered last. The child has already
    // completed session creation, inherited-descriptor setup, cwd/env/stdio
    // setup performed by Command, and every configured resource-limit hook.
    // It performs only bounded libc syscalls before returning to Rust's
    // existing exec implementation.
    #[cfg(target_os = "linux")]
    if let Some(gate) = attachment_gate.as_ref() {
        use std::os::unix::process::CommandExt as _;

        let status_writer_fd = gate.status_writer.as_raw_fd();
        let release_reader_fd = gate.release_reader.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                direct_attachment_hold_pre_exec(status_writer_fd, release_reader_fd)
            });
        }
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Err(spawn_failure(start, format!("Failed to spawn: {e}"))),
    };
    #[cfg(target_os = "linux")]
    drop(attachment_gate);
    // The forked child now owns its inherited descriptor copies. Close the
    // request-owned parent copies promptly: in particular, keeping the status
    // writer open here would hide a launcher's pre-target EOF and force every
    // failed setup to wait for the full supervision timeout.
    drop(inherited_fds);
    let wrapper_pid = child.id();

    // On Unix with setsid, pid == pgid since the child is its own process group leader.
    #[cfg(unix)]
    let wrapper_pgid = wrapper_pid as i64;
    #[cfg(not(unix))]
    let wrapper_pgid = -1i64;

    let stdout_capture = Arc::new(Mutex::new(BoundedCapture::default()));
    let stderr_capture = Arc::new(Mutex::new(BoundedCapture::default()));
    let drain_stop = Arc::new(AtomicBool::new(false));
    let (output_overflow_tx, output_overflow_rx) = std::sync::mpsc::channel();
    let (stdout_thread, stderr_thread) = if inherit_stdio {
        (thread::spawn(|| {}), thread::spawn(|| {}))
    } else {
        let mut stdout_handle = child.stdout.take().expect("stdout configured as piped");
        let mut stderr_handle = child.stderr.take().expect("stderr configured as piped");
        if let Err(error) = configure_nonblocking_fd(&mut stdout_handle)
            .and_then(|_| configure_nonblocking_fd(&mut stderr_handle))
        {
            kill_process_group_if_safe(wrapper_pgid);
            let _ = child.kill();
            let _ = child.wait();
            return Err(spawn_failure(
                start,
                format!("Failed to spawn: configure bounded output capture: {error}"),
            ));
        }
        (
            spawn_bounded_drain(
                stdout_handle,
                Some(
                    limits
                        .as_ref()
                        .and_then(|limits| limits.max_stdout_bytes)
                        .unwrap_or(DEFAULT_MAX_CAPTURE_BYTES),
                ),
                CapturedStream::Stdout,
                Arc::clone(&stdout_capture),
                Arc::clone(&drain_stop),
                output_overflow_tx.clone(),
            ),
            spawn_bounded_drain(
                stderr_handle,
                Some(
                    limits
                        .as_ref()
                        .and_then(|limits| limits.max_stderr_bytes)
                        .unwrap_or(DEFAULT_MAX_CAPTURE_BYTES),
                ),
                CapturedStream::Stderr,
                Arc::clone(&stderr_capture),
                Arc::clone(&drain_stop),
                output_overflow_tx,
            ),
        )
    };

    // Never write request input on the spawning thread. A child can stop
    // reading before the pipe buffer is empty; the dedicated writer may then
    // wait on WouldBlock, but it observes the same cleanup flag as the bounded
    // drainers. The request deadline can therefore terminate and join every
    // pipe worker even when the child never consumes the remaining input.
    let mut stdin_thread =
        match spawn_stdin_writer(child.stdin.take(), stdin_data, Arc::clone(&drain_stop)) {
            Ok(thread) => thread,
            Err(error) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.kill();
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(spawn_failure(
                    start,
                    format!("Failed to spawn: configure nonblocking stdin: {error}"),
                ));
            }
        };

    let (identity, status_thread, attachment_release) = if let Some(status) = supervised_status {
        let (reader, attachment_release) = match status {
            SupervisedProcessStatus::Run { reader } => (reader, None),
            SupervisedProcessStatus::AwaitingAttachment {
                reader,
                attachment_release,
            } => (reader, Some(attachment_release)),
        };
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let status_thread = match spawn_supervised_launcher_status_reader(
            reader,
            status_tx,
            Arc::clone(&drain_stop),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.kill();
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread.take() {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(spawn_failure(
                    start,
                    format!(
                        "Failed to spawn: initialize supervised-launcher status reader: {error}"
                    ),
                ));
            }
        };
        let setup_deadline = supervised_setup_deadline(start, timeout);
        let setup_wait = setup_deadline.saturating_duration_since(Instant::now());
        let reported_pid = match status_rx.recv_timeout(setup_wait) {
            Ok(Ok(InitialLauncherStatus::Target(pid))) => pid,
            Ok(Ok(InitialLauncherStatus::Refused(diagnostic))) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.kill();
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread.take() {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let _ = status_thread.join();
                return Err(spawn_failure_with_launcher_refusal(start, diagnostic));
            }
            Ok(Err(error)) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.kill();
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread.take() {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let _ = status_thread.join();
                return Err(spawn_failure(
                    start,
                    format!("Failed to spawn: supervised launcher refused: {error}"),
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.kill();
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread.take() {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let _ = status_thread.join();
                return Err(spawn_failure(
                    start,
                    format!(
                        "Failed to spawn: supervised launcher did not report its target PID before the bounded setup/request deadline ({:.3} seconds remaining after launch setup)",
                        setup_wait.as_secs_f64()
                    ),
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.kill();
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread.take() {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let _ = status_thread.join();
                return Err(spawn_failure(
                    start,
                    "Failed to spawn: supervised-launcher status channel closed before reporting its target PID",
                ));
            }
        };
        let identity = match resolve_supervised_identity(reported_pid, wrapper_pid, wrapper_pgid) {
            Ok(identity) => identity,
            Err(error) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.kill();
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread.take() {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let _ = status_thread.join();
                return Err(spawn_failure(
                    start,
                    format!("Failed to spawn: invalid supervised target identity: {error}"),
                ));
            }
        };
        (identity, Some(status_thread), attachment_release)
    } else {
        (
            ProcessIdentity {
                pid: wrapper_pid,
                pgid: wrapper_pgid,
            },
            None,
            None,
        )
    };

    Ok(RunningProcess {
        pid: identity.pid,
        pgid: identity.pgid,
        wrapper_pid,
        wrapper_pgid,
        child,
        stdin_thread,
        stdout_thread: Some(stdout_thread),
        stderr_thread: Some(stderr_thread),
        status_thread,
        stdout_capture,
        stderr_capture,
        drain_stop,
        output_overflow_rx,
        start,
        timeout,
        attachment_release,
        groups_terminated: false,
        wrapper_reaped: false,
    })
}

fn spawn_stdin_writer(
    stdin: Option<process::ChildStdin>,
    data: Option<String>,
    stop: Arc<AtomicBool>,
) -> Result<Option<thread::JoinHandle<()>>, String> {
    let (Some(mut stdin), Some(data)) = (stdin, data) else {
        return Ok(None);
    };
    configure_nonblocking_fd(&mut stdin)?;
    Ok(Some(thread::spawn(move || {
        let bytes = data.as_bytes();
        let mut written = 0;
        while written < bytes.len() && !stop.load(Ordering::Acquire) {
            match stdin.write(&bytes[written..]) {
                Ok(0) => break,
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(_) => break,
            }
        }
    })))
}

fn spawn_bounded_drain<R>(
    mut reader: R,
    limit: Option<u64>,
    stream: CapturedStream,
    capture: SharedCapture,
    stop: Arc<AtomicBool>,
    overflow_tx: std::sync::mpsc::Sender<CapturedStream>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut read_buffer = [0u8; 8192];
        let mut overflow_reported = false;
        let mut post_stop_reads = None;
        loop {
            if stop.load(Ordering::Acquire) {
                let remaining = post_stop_reads.get_or_insert(POST_STOP_DRAIN_READS);
                if *remaining == 0 {
                    let mut probe = [0u8; 1];
                    match reader.read(&mut probe) {
                        Ok(0) => {}
                        Ok(_) => {
                            let mut state =
                                capture.lock().unwrap_or_else(|error| error.into_inner());
                            state.truncated = true;
                            if !overflow_reported {
                                let _ = overflow_tx.send(stream);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => {}
                    }
                    break;
                }
            }
            match reader.read(&mut read_buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if let Some(remaining) = post_stop_reads.as_mut() {
                        *remaining -= 1;
                    }
                    let mut state = capture.lock().unwrap_or_else(|error| error.into_inner());
                    let retain = match limit {
                        Some(limit) => limit
                            .saturating_sub(state.bytes.len() as u64)
                            .min(read as u64) as usize,
                        None => read,
                    };
                    state.bytes.extend_from_slice(&read_buffer[..retain]);
                    if retain < read {
                        state.truncated = true;
                        if !overflow_reported {
                            overflow_reported = true;
                            let _ = overflow_tx.send(stream);
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(CAPTURE_POLL_INTERVAL);
                }
                Err(_) => break,
            }
        }
    })
}

#[cfg(unix)]
fn configure_nonblocking_fd<T>(reader: &mut T) -> Result<(), String>
where
    T: std::os::fd::AsRawFd,
{
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(format!(
            "set O_NONBLOCK on descriptor {fd}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn configure_nonblocking_fd<T>(_reader: &mut T) -> Result<(), String> {
    Ok(())
}

fn spawn_supervised_launcher_status_reader(
    mut reader: std::fs::File,
    initial_tx: std::sync::mpsc::Sender<Result<InitialLauncherStatus, String>>,
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, String> {
    configure_nonblocking_fd(&mut reader)
        .map_err(|error| format!("configure nonblocking status channel: {error}"))?;
    Ok(thread::spawn(move || {
        let mut initial_tx = Some(initial_tx);
        let mut pending = Vec::new();
        let mut buffer = [0u8; 4096];

        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if !pending.is_empty() && initial_tx.is_some() {
                        report_supervised_launcher_status_line(&pending, &mut initial_tx);
                    }
                    if let Some(tx) = initial_tx.take() {
                        let _ = tx.send(Err(
                            "status channel reached EOF before a child-pid document".to_string(),
                        ));
                    }
                    break;
                }
                Ok(read) => {
                    pending.extend_from_slice(&buffer[..read]);
                    if pending.len() > SUPERVISED_STATUS_MAX_LINE_BYTES {
                        if let Some(tx) = initial_tx.take() {
                            let _ = tx.send(Err(format!(
                                "status document exceeds {SUPERVISED_STATUS_MAX_LINE_BYTES} bytes"
                            )));
                        }
                        pending.clear();
                    }
                    while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                        let mut remainder = pending.split_off(newline + 1);
                        std::mem::swap(&mut pending, &mut remainder);
                        remainder.truncate(newline);
                        if initial_tx.is_some() && !remainder.is_empty() {
                            report_supervised_launcher_status_line(&remainder, &mut initial_tx);
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(error) => {
                    if let Some(tx) = initial_tx.take() {
                        let _ = tx.send(Err(format!("read status channel: {error}")));
                    }
                    break;
                }
            }
        }
    }))
}

fn report_supervised_launcher_status_line(
    line: &[u8],
    initial_tx: &mut Option<std::sync::mpsc::Sender<Result<InitialLauncherStatus, String>>>,
) {
    let result = match reject_duplicate_status_keys(line) {
        Err(error) => Err(format!("invalid JSON status document: {error}")),
        Ok(()) => match serde_json::from_slice::<LauncherTargetDocument>(line) {
            Ok(document) => Ok(InitialLauncherStatus::Target(document.child_pid)),
            Err(target_error) => match serde_json::from_slice::<LauncherRefusalDocument>(line) {
                Ok(document) => Ok(InitialLauncherStatus::Refused(
                    document.refused.get().to_string(),
                )),
                Err(refusal_error) => Err(format!(
                    "invalid JSON status document (target: {target_error}; refusal: {refusal_error})"
                )),
            },
        },
    };
    if let Some(tx) = initial_tx.take() {
        let _ = tx.send(result);
    }
}

fn reject_duplicate_status_keys(line: &[u8]) -> Result<(), serde_json::Error> {
    const MAX_STATUS_JSON_DEPTH: usize = 32;

    struct StrictStatusJson {
        depth: usize,
    }

    impl<'de> serde::de::DeserializeSeed<'de> for StrictStatusJson {
        type Value = ();

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(StrictStatusJsonVisitor { depth: self.depth })
        }
    }

    struct StrictStatusJsonVisitor {
        depth: usize,
    }

    impl<'de> serde::de::Visitor<'de> for StrictStatusJsonVisitor {
        type Value = ();

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded launcher status document with unique object keys")
        }

        fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
            Ok(())
        }

        fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
            Ok(())
        }

        fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
            Ok(())
        }

        fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
            Ok(())
        }

        fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
            Ok(())
        }

        fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
            Ok(())
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(())
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            if self.depth >= MAX_STATUS_JSON_DEPTH {
                return Err(serde::de::Error::custom(
                    "launcher status JSON is too deeply nested",
                ));
            }
            while sequence
                .next_element_seed(StrictStatusJson {
                    depth: self.depth + 1,
                })?
                .is_some()
            {}
            Ok(())
        }

        fn visit_map<A>(self, mut mapping: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            if self.depth >= MAX_STATUS_JSON_DEPTH {
                return Err(serde::de::Error::custom(
                    "launcher status JSON is too deeply nested",
                ));
            }
            let mut keys = std::collections::BTreeSet::new();
            while let Some(key) = mapping.next_key::<String>()? {
                if !keys.insert(key.clone()) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate JSON object key `{key}`"
                    )));
                }
                mapping.next_value_seed(StrictStatusJson {
                    depth: self.depth + 1,
                })?;
            }
            Ok(())
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(line);
    serde::de::DeserializeSeed::deserialize(StrictStatusJson { depth: 0 }, &mut deserializer)?;
    deserializer.end()
}

#[cfg(unix)]
fn resolve_supervised_identity(
    pid: u32,
    wrapper_pid: u32,
    wrapper_pgid: i64,
) -> Result<ProcessIdentity, String> {
    let pid_i32 = i32::try_from(pid).map_err(|_| format!("child PID {pid} exceeds pid_t"))?;
    if pid_i32 <= 1 || pid == wrapper_pid || pid == process::id() {
        return Err(format!("unsafe child PID {pid}"));
    }

    if wrapper_pgid <= 1
        || wrapper_pgid > i32::MAX as i64
        || wrapper_pgid != wrapper_pid as i64
        || wrapper_pgid == unsafe { libc::getpgrp() } as i64
    {
        return Err(format!(
            "unsafe retained launcher process group {wrapper_pgid}"
        ));
    }

    // Group membership is inherited atomically at fork, so there is no
    // target-side session-establishment race to wait through. If the target is
    // still visible, require it to be in the retained launcher's group. If it
    // has already exited, the trusted status PID remains useful for accounting
    // and the retained launcher still owns the only PGID Lillux will signal.
    let observed_pgid = unsafe { libc::getpgid(pid_i32) };
    if observed_pgid < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("getpgid({pid}) failed: {error}"));
        }
    } else if observed_pgid as i64 != wrapper_pgid {
        return Err(format!(
            "child PID {pid} is outside retained launcher process group {wrapper_pgid} (observed {observed_pgid})"
        ));
    }

    Ok(ProcessIdentity {
        pid,
        pgid: wrapper_pgid,
    })
}

#[cfg(not(unix))]
fn resolve_supervised_identity(
    _pid: u32,
    _wrapper_pid: u32,
    _wrapper_pgid: i64,
) -> Result<ProcessIdentity, String> {
    Err("supervised process identity is unsupported on this platform".to_string())
}

fn supervised_setup_deadline(start: Instant, timeout: f64) -> Instant {
    let status_deadline = start
        .checked_add(SUPERVISED_STATUS_SETUP_TIMEOUT)
        .unwrap_or_else(Instant::now);
    request_timeout_duration(timeout)
        .and_then(|timeout| start.checked_add(timeout))
        .map_or(status_deadline, |request_deadline| {
            std::cmp::min(status_deadline, request_deadline)
        })
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct AttachmentReady {
    pid: u32,
    pgid: i64,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinuxProcessBirth {
    state: char,
    parent_pid: u32,
    process_group: i64,
    start_time_ticks: u64,
}

#[cfg(target_os = "linux")]
fn attachment_pipe(
    label: &str,
    start: Instant,
) -> Result<(std::fs::File, std::fs::File), SubprocessResult> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(spawn_failure(
            start,
            format!(
                "Failed to create attachment {label} pipe: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    // SAFETY: pipe2 returned two new uniquely-owned descriptors.
    Ok(unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    })
}

#[cfg(target_os = "linux")]
fn open_attachment_cwd(path: &str, start: Instant) -> Result<std::fs::File, SubprocessResult> {
    let path = std::ffi::CString::new(path).map_err(|_| {
        spawn_failure(
            start,
            "Failed to spawn awaiting attachment: cwd contains an interior NUL byte",
        )
    })?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(spawn_failure(
            start,
            format!(
                "Failed to spawn awaiting attachment: open cwd: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    // SAFETY: open returned a new uniquely-owned descriptor.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Final post-fork child hook for a direct attachment-prepared launch.
///
/// Keep this function allocation-free and syscall-only. It executes in the
/// forked child of a multithreaded daemon before Rust's normal exec path.
#[cfg(target_os = "linux")]
fn direct_attachment_identity_pre_exec(
    expected_parent_pid: libc::pid_t,
    status_writer_fd: i32,
    status_reader_fd: i32,
    release_writer_fd: i32,
    cwd_directory_fd: Option<i32>,
    inherited_pending_control_fds: &[i32],
) -> std::io::Result<()> {
    unsafe {
        libc::close(status_reader_fd);
        libc::close(release_writer_fd);
        for fd in inherited_pending_control_fds {
            libc::close(*fd);
        }

        validate_attachment_parent(libc::getppid(), expected_parent_pid)?;
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The initiating parent can die between getppid and prctl. Rechecking
        // its exact identity closes that window before readiness is published.
        validate_attachment_parent(libc::getppid(), expected_parent_pid)?;

        let pid = libc::getpid();
        let pgid = libc::getpgrp();
        if pid <= 1 || pgid != pid {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        write_attachment_record(
            status_writer_fd,
            ATTACHMENT_IDENTITY_PHASE,
            pid as u32,
            pgid,
        )?;
        if let Some(cwd_directory_fd) = cwd_directory_fd {
            if libc::fchdir(cwd_directory_fd) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(cwd_directory_fd);
        }
        Ok(())
    }
}

/// Require continuity with the exact process that initiated the fork.
///
/// PID 1 is intentionally valid: a daemon may legitimately be PID 1 inside a
/// container PID namespace. A mismatch, rather than the numeric value 1,
/// identifies an orphan/reparent race.
#[cfg(target_os = "linux")]
fn validate_attachment_parent(
    observed_parent_pid: libc::pid_t,
    expected_parent_pid: libc::pid_t,
) -> std::io::Result<()> {
    if expected_parent_pid <= 0 || observed_parent_pid != expected_parent_pid {
        return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod attachment_parent_tests {
    use super::validate_attachment_parent;

    #[test]
    fn pid_one_is_a_valid_exact_attachment_parent() {
        validate_attachment_parent(1, 1).expect("PID 1 may legitimately initiate the fork");
    }

    #[test]
    fn changed_attachment_parent_fails_with_echild() {
        let error = validate_attachment_parent(1, 42).expect_err("reparenting must fail closed");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    fn invalid_expected_attachment_parent_fails_with_echild() {
        let error = validate_attachment_parent(0, 0).expect_err("PID zero is not a parent");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
    }
}

#[cfg(target_os = "linux")]
fn direct_attachment_hold_pre_exec(
    status_writer_fd: i32,
    release_reader_fd: i32,
) -> std::io::Result<()> {
    unsafe {
        let pid = libc::getpid();
        let pgid = libc::getpgrp();
        if pid <= 1 || pgid != pid {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        write_attachment_record(status_writer_fd, ATTACHMENT_READY_PHASE, pid as u32, pgid)?;

        let mut token = 0u8;
        loop {
            let count = libc::read(release_reader_fd, (&mut token as *mut u8).cast(), 1);
            if count == 1 {
                break;
            }
            if count == 0 {
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if token != ATTACHMENT_RELEASE_TOKEN {
            return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
        }
        if libc::prctl(libc::PR_SET_PDEATHSIG, 0, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        libc::close(status_writer_fd);
        libc::close(release_reader_fd);
        Ok(())
    }
}

/// Write one fixed-width phase record without allocation or buffered I/O.
#[cfg(target_os = "linux")]
unsafe fn write_attachment_record(
    status_writer_fd: i32,
    phase: u32,
    pid: u32,
    pgid: i32,
) -> std::io::Result<()> {
    let record = [
        u32::from_ne_bytes(ATTACHMENT_READY_MAGIC).to_ne_bytes(),
        phase.to_ne_bytes(),
        pid.to_ne_bytes(),
        pgid.to_ne_bytes(),
    ];
    let record_ptr = record.as_ptr().cast::<u8>();
    let mut written = 0usize;
    while written < ATTACHMENT_READY_RECORD_BYTES {
        let count = unsafe {
            libc::write(
                status_writer_fd,
                record_ptr.add(written).cast(),
                ATTACHMENT_READY_RECORD_BYTES - written,
            )
        };
        if count > 0 {
            written += count as usize;
            continue;
        }
        let error = std::io::Error::last_os_error();
        if count < 0 && error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_attachment_ready(
    reader: &std::fs::File,
    deadline: Instant,
    expected_phase: u32,
) -> Result<AttachmentReady, String> {
    let fd = reader.as_raw_fd();
    let mut bytes = [0u8; ATTACHMENT_READY_RECORD_BYTES];
    let mut read = 0usize;
    while read < bytes.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("child setup did not reach the attachment boundary before the bounded setup/request deadline".to_string());
        }
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let poll_result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if poll_result == 0 {
            return Err("child setup did not reach the attachment boundary before the bounded setup/request deadline".to_string());
        }
        if poll_result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("read attachment readiness: {error}"));
        }
        let count =
            unsafe { libc::read(fd, bytes[read..].as_mut_ptr().cast(), bytes.len() - read) };
        if count > 0 {
            read += count as usize;
            continue;
        }
        if count == 0 {
            return Err("child setup failed before publishing attachment readiness".to_string());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!("read attachment readiness: {error}"));
        }
    }

    if bytes[0..4] != ATTACHMENT_READY_MAGIC {
        return Err("child published malformed attachment readiness magic".to_string());
    }
    let phase = u32::from_ne_bytes(bytes[4..8].try_into().expect("fixed slice"));
    if phase != expected_phase {
        return Err(format!(
            "child published attachment phase {phase}, expected {expected_phase}"
        ));
    }
    let pid = u32::from_ne_bytes(bytes[8..12].try_into().expect("fixed slice"));
    let pgid = i32::from_ne_bytes(bytes[12..16].try_into().expect("fixed slice")) as i64;
    Ok(AttachmentReady { pid, pgid })
}

#[cfg(target_os = "linux")]
fn validate_direct_attachment_identity(pid: u32, pgid: i64) -> Result<(), String> {
    let pid_i32 = i32::try_from(pid).map_err(|_| format!("child PID {pid} exceeds pid_t"))?;
    if pid_i32 <= 1 || pid == process::id() || pgid != pid as i64 {
        return Err(format!(
            "unsafe direct attachment identity PID {pid}, PGID {pgid}"
        ));
    }
    let observed_pgid = unsafe { libc::getpgid(pid_i32) };
    if observed_pgid < 0 {
        return Err(format!(
            "inspect direct attachment process group: {}",
            std::io::Error::last_os_error()
        ));
    }
    if observed_pgid as i64 != pgid {
        return Err(format!(
            "direct attachment child {pid} changed process groups (expected {pgid}, observed {observed_pgid})"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_linux_process_birth(pid: u32) -> Result<LinuxProcessBirth, String> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("read /proc/{pid}/stat: {error}"))?;
    let close = raw
        .rfind(')')
        .ok_or_else(|| format!("malformed /proc/{pid}/stat comm"))?;
    let fields: Vec<_> = raw[close + 1..].split_whitespace().collect();
    let state = fields
        .first()
        .and_then(|value| value.chars().next())
        .ok_or_else(|| format!("missing /proc/{pid}/stat state"))?;
    let parent_pid = fields
        .get(1)
        .ok_or_else(|| format!("missing /proc/{pid}/stat parent pid"))?
        .parse::<u32>()
        .map_err(|error| format!("invalid /proc/{pid}/stat parent pid: {error}"))?;
    let process_group = fields
        .get(2)
        .ok_or_else(|| format!("missing /proc/{pid}/stat process group"))?
        .parse::<i64>()
        .map_err(|error| format!("invalid /proc/{pid}/stat process group: {error}"))?;
    let start_time_ticks = fields
        .get(19)
        .ok_or_else(|| format!("missing /proc/{pid}/stat start time"))?
        .parse::<u64>()
        .map_err(|error| format!("invalid /proc/{pid}/stat start time: {error}"))?;
    if start_time_ticks == 0 {
        return Err(format!("invalid zero /proc/{pid}/stat start time"));
    }
    Ok(LinuxProcessBirth {
        state,
        parent_pid,
        process_group,
        start_time_ticks,
    })
}

#[cfg(target_os = "linux")]
fn validate_pinned_process_birth(
    pid: u32,
    pgid: i64,
    expected_parent: Option<u32>,
    observed_before_pin: &LinuxProcessBirth,
    pidfd: i32,
) -> Result<(), String> {
    pidfd_send_signal(pidfd, 0)?;
    let observed_after_pin = read_linux_process_birth(pid)?;
    if observed_after_pin.parent_pid != observed_before_pin.parent_pid
        || observed_after_pin.process_group != observed_before_pin.process_group
        || observed_after_pin.start_time_ticks != observed_before_pin.start_time_ticks
    {
        return Err(format!(
            "process {pid} birth identity changed while its pidfd was opened"
        ));
    }
    if observed_after_pin.process_group != pgid {
        return Err(format!(
            "process {pid} escaped retained process group {pgid} (observed {})",
            observed_after_pin.process_group
        ));
    }
    if let Some(expected_parent) = expected_parent {
        if observed_after_pin.parent_pid != expected_parent {
            return Err(format!(
                "process {pid} parent changed before identity pin (expected {expected_parent}, observed {})",
                observed_after_pin.parent_pid
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_supervised_attachment_target(pid: u32, pgid: i64, pidfd: i32) -> Result<(), String> {
    pidfd_send_signal(pidfd, 0)?;
    let pid_i32 = i32::try_from(pid).map_err(|_| format!("target PID {pid} exceeds pid_t"))?;
    let observed_pgid = unsafe { libc::getpgid(pid_i32) };
    if observed_pgid < 0 {
        return Err(format!(
            "inspect supervised attachment target process group: {}",
            std::io::Error::last_os_error()
        ));
    }
    if observed_pgid as i64 != pgid {
        return Err(format!(
            "supervised attachment target {pid} escaped retained process group {pgid} (observed {observed_pgid})"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> Result<OwnedFd, String> {
    let pid = i32::try_from(pid).map_err(|_| "PID exceeds pid_t".to_string())?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
    if fd < 0 {
        return Err(format!(
            "pidfd_open({pid}): {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: pidfd_open returned a new uniquely-owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn pidfd_send_signal(pidfd: i32, signal: i32) -> Result<(), String> {
    pidfd_send_signal_io(pidfd, signal)
        .map_err(|error| format!("pidfd_send_signal({signal}): {error}"))
}

#[cfg(target_os = "linux")]
fn pidfd_send_signal_io(pidfd: i32, signal: i32) -> std::io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn wait_pidfd_exit(pidfd: i32, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "pidfd exit deadline overflow".to_string())?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("exact process did not exit before cleanup deadline".to_string());
        }
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd: pidfd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if result > 0 {
            if pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                return Ok(());
            }
            return Err(format!(
                "pidfd reported unexpected cleanup events {:#x}",
                pollfd.revents
            ));
        }
        if result == 0 {
            return Err("exact process did not exit before cleanup deadline".to_string());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!("poll exact process pidfd for exit: {error}"));
        }
    }
}

#[cfg(target_os = "linux")]
fn prove_attachment_cleanup(pidfd: i32, cleanup: Result<(), String>) -> Result<(), String> {
    let exit = wait_pidfd_exit(pidfd, ATTACHMENT_ABORT_SETTLE_TIMEOUT);
    match (cleanup, exit) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(cleanup), Ok(())) => Err(cleanup),
        (Ok(()), Err(exit)) => Err(exit),
        (Err(cleanup), Err(exit)) => Err(format!("{cleanup}; exact-exit proof failed: {exit}")),
    }
}

#[cfg(target_os = "linux")]
fn wait_owned_process_group_quiescent(
    pgid: i64,
    retained_leader_pid: u32,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "process-group quiescence deadline overflow".to_string())?;
    loop {
        let mut live_member = None;
        let entries = std::fs::read_dir("/proc")
            .map_err(|error| format!("enumerate /proc for process-group cleanup: {error}"))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("enumerate /proc process entry: {error}"))?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == retained_leader_pid {
                continue;
            }
            match read_linux_process_birth(pid) {
                Ok(stat) if stat.process_group == pgid && !matches!(stat.state, 'Z' | 'X') => {
                    live_member = Some(pid);
                    break;
                }
                Ok(_) => {}
                Err(error) if error.contains("No such file or directory") => {}
                Err(error) => {
                    return Err(format!(
                        "inspect process-group member {pid} during cleanup: {error}"
                    ));
                }
            }
        }
        if live_member.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "process group {pgid} retained live member {} after termination",
                live_member.expect("checked Some")
            ));
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

#[cfg(target_os = "linux")]
fn reap_exact_child_pid(pid: u32) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|_| "PID exceeds pid_t".to_string())?;
    let mut status = 0i32;
    loop {
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            return Ok(());
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // ECHILD means Command::spawn or RunningProcess already reaped the
            // exact child, which is also a successful settlement proof.
            if error.raw_os_error() == Some(libc::ECHILD) {
                return Ok(());
            }
            return Err(format!("waitpid({pid}): {error}"));
        }
    }
}

fn settle_direct_attachment_worker(
    pid: u32,
    worker: thread::JoinHandle<Result<RunningProcess, SubprocessResult>>,
) -> Result<(), String> {
    match worker.join() {
        Ok(Ok(running)) => running.abort_and_reap_checked(),
        Ok(Err(_)) => reap_exact_child_pid(pid),
        Err(_) => reap_exact_child_pid(pid)
            .map_err(|error| format!("attachment worker panicked; {error}")),
    }
}

#[cfg(target_os = "linux")]
fn cleanup_direct_after_release_worker_panic(
    pid: u32,
    pgid: i64,
    pidfd: i32,
) -> Result<(), String> {
    kill_owned_process_group(pid, pgid, true);
    let signal = match pidfd_send_signal_io(pidfd, ATTACHMENT_ABORT_SIGNAL) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        Err(error) => Err(format!(
            "pidfd_send_signal({ATTACHMENT_ABORT_SIGNAL}): {error}"
        )),
    };
    let exit = wait_pidfd_exit(pidfd, ATTACHMENT_ABORT_SETTLE_TIMEOUT);
    let group = wait_owned_process_group_quiescent(pgid, pid, ATTACHMENT_ABORT_SETTLE_TIMEOUT);
    let mut failures = Vec::new();
    if exit.is_ok() && group.is_ok() {
        if let Err(error) = reap_exact_child_pid(pid) {
            failures.push(error);
        }
    } else {
        if let Err(error) = signal {
            failures.push(error);
        }
        if let Err(error) = exit {
            failures.push(error);
        }
        if let Err(error) = group {
            failures.push(error);
        }
    }
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    Ok(())
}

/// Last-resort cleanup proof used after the structured owner has completed or
/// reported an error. The release authority has already been closed, so
/// repeated termination cannot make the target runnable. The retained target
/// pidfd and unreaped group leader keep both numeric identities fenced while
/// the exact process, every same-group member, and the wrapper are settled.
#[cfg(target_os = "linux")]
fn force_attachment_cleanup(pgid: i64, pidfd: i32, timeout: Duration) -> Result<(), String> {
    let group_leader = u32::try_from(pgid)
        .map_err(|_| "attachment process-group leader exceeds pid_t".to_string())?;
    let signal = match pidfd_send_signal_io(pidfd, ATTACHMENT_ABORT_SIGNAL) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        Err(error) => Err(format!(
            "pidfd_send_signal({ATTACHMENT_ABORT_SIGNAL}) during final attachment cleanup: {error}"
        )),
    };
    kill_owned_process_group(group_leader, pgid, true);
    let exit = wait_pidfd_exit(pidfd, timeout);
    let group = wait_owned_process_group_quiescent(pgid, group_leader, timeout);
    let leader_exit = wait_exact_child_exit_unreaped(group_leader, timeout);
    let mut failures = Vec::new();
    match (&exit, &group, &leader_exit) {
        (Ok(()), Ok(()), Ok(())) => {
            // Only now may the leader be reaped. Until this point its
            // unreaped identity is what makes negative-PGID signalling safe
            // across cleanup retries.
            if let Err(error) = reap_exact_child_pid(group_leader) {
                failures.push(error);
            }
        }
        _ => {
            if let Err(error) = signal {
                failures.push(error);
            }
            if let Err(error) = exit {
                failures.push(error);
            }
            if let Err(error) = group {
                failures.push(error);
            }
            if let Err(error) = leader_exit {
                failures.push(error);
            }
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "final attachment cleanup proof failed: {}",
            failures.join("; ")
        ));
    }
    Ok(())
}

/// Observe an owned child exit without reaping it. Keeping the zombie owned
/// reserves its PID and process-group identity until all same-group members
/// have been proved quiescent.
#[cfg(target_os = "linux")]
fn wait_exact_child_exit_unreaped(pid: u32, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "child-exit observation deadline overflow".to_string())?;
    let pid = i32::try_from(pid).map_err(|_| "PID exceeds pid_t".to_string())?;
    loop {
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            if unsafe { status.si_pid() } != 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "process-group leader {pid} did not exit before cleanup deadline"
                ));
            }
            thread::sleep(PROCESS_POLL_INTERVAL);
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        // All structured cleanup paths reap only after proving group
        // quiescence. ECHILD therefore means an earlier attempt already
        // completed the stronger proof and reaped the leader.
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(());
        }
        return Err(format!(
            "observe process-group leader {pid} before reap: {error}"
        ));
    }
}

#[cfg(target_os = "linux")]
fn complete_attachment_cleanup(pidfd: i32, pgid: i64) {
    loop {
        if force_attachment_cleanup(pgid, pidfd, ATTACHMENT_ABORT_SETTLE_TIMEOUT).is_ok() {
            return;
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

#[cfg(not(target_os = "linux"))]
fn reap_exact_child_pid(_pid: u32) -> Result<(), String> {
    Err("exact child reaping is supported only on Linux".to_string())
}

fn request_timeout_duration(timeout: f64) -> Option<Duration> {
    if !timeout.is_finite() || timeout <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(timeout).ok()
}

#[cfg(unix)]
fn kill_owned_process_group(pid: u32, pgid: i64, leader_owned: bool) {
    if !leader_owned || pgid <= 1 || pgid > i32::MAX as i64 {
        return;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let current_pgid = unsafe { libc::getpgrp() } as i64;
    if pgid == current_pgid {
        return;
    }

    // Revalidate the retained group leader immediately before signalling. The
    // caller still owns that leader as a live child or unreaped zombie, and a
    // session leader cannot move to another process group, so the numeric PGID
    // cannot be recycled between this check and the group signal. Platforms
    // without WNOWAIT reach this helper after reaping and safely skip instead.
    if unsafe { libc::getpgid(pid) } as i64 != pgid {
        return;
    }
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_owned_process_group(_pid: u32, _pgid: i64, _leader_owned: bool) {}

#[cfg(unix)]
fn kill_process_group_if_safe(pgid: i64) {
    let current_pgid = unsafe { libc::getpgrp() } as i64;
    if pgid <= 1 || pgid == current_pgid || pgid > i32::MAX as i64 {
        return;
    }
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group_if_safe(_pgid: i64) {}

fn take_capture(capture: &SharedCapture) -> BoundedCapture {
    let mut capture = capture.lock().unwrap_or_else(|error| error.into_inner());
    std::mem::take(&mut *capture)
}

fn output_limit_exceeded(
    stdout: &BoundedCapture,
    stderr: &BoundedCapture,
) -> Option<OutputLimitExceeded> {
    match (stdout.truncated, stderr.truncated) {
        (true, true) => Some(OutputLimitExceeded::Both),
        (true, false) => Some(OutputLimitExceeded::Stdout),
        (false, true) => Some(OutputLimitExceeded::Stderr),
        (false, false) => None,
    }
}

fn append_diagnostic(existing: &str, diagnostic: &str) -> String {
    if existing.is_empty() {
        diagnostic.to_string()
    } else if existing.ends_with('\n') {
        format!("{existing}{diagnostic}")
    } else {
        format!("{existing}\n{diagnostic}")
    }
}

fn spawn_failure(start: Instant, reason: impl Into<String>) -> SubprocessResult {
    SubprocessResult {
        success: false,
        stdout: String::new(),
        stderr: reason.into(),
        exit_code: -1,
        duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        pid: 0,
        timed_out: false,
        launcher_refusal: None,
        output_limit_exceeded: None,
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

fn spawn_failure_with_launcher_refusal(start: Instant, diagnostic: String) -> SubprocessResult {
    SubprocessResult {
        success: false,
        stdout: String::new(),
        stderr: "Failed to spawn: supervised launcher refused target execution".to_string(),
        exit_code: -1,
        duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        pid: 0,
        timed_out: false,
        launcher_refusal: Some(diagnostic),
        output_limit_exceeded: None,
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

/// Validate subprocess resource limits without changing process state.
///
/// This checks platform support, finite representation, and the current
/// process's hard limit. It does not install any limit.
pub fn validate_subprocess_limits(limits: Option<&SubprocessLimits>) -> Result<(), String> {
    validate_output_retention_limits(limits)?;
    #[cfg(unix)]
    {
        validated_max_open_files(limits).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        if let Some(max_open_files) = limits.and_then(|limits| limits.max_open_files) {
            return Err(format!(
                "max_open_files {max_open_files} is unsupported on this platform"
            ));
        }
        Ok(())
    }
}

/// Validate and attach subprocess resource limits to `command`.
///
/// On Unix the limits are installed in a `pre_exec` hook. A failure to install
/// them aborts the spawn, so callers cannot accidentally run without the
/// configured cap. Unsupported or invalid limits are rejected immediately.
pub fn configure_subprocess_limits(
    command: &mut process::Command,
    limits: Option<&SubprocessLimits>,
) -> Result<(), String> {
    validate_output_retention_limits(limits)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let max_open_files = validated_max_open_files(limits)?;
        if let Some(max_open_files) = max_open_files {
            unsafe {
                command.pre_exec(move || {
                    let limit = libc::rlimit {
                        rlim_cur: max_open_files,
                        rlim_max: max_open_files,
                    };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        validate_subprocess_limits(limits)?;
    }
    Ok(())
}

fn validate_output_retention_limits(limits: Option<&SubprocessLimits>) -> Result<(), String> {
    let Some(limits) = limits else {
        return Ok(());
    };
    for (name, value) in [
        ("max_stdout_bytes", limits.max_stdout_bytes),
        ("max_stderr_bytes", limits.max_stderr_bytes),
    ] {
        if let Some(value) = value {
            usize::try_from(value)
                .map_err(|_| format!("{name} {value} cannot be represented on this platform"))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validated_max_open_files(
    limits: Option<&SubprocessLimits>,
) -> Result<Option<libc::rlim_t>, String> {
    let Some(max_open_files) = limits.and_then(|limits| limits.max_open_files) else {
        return Ok(None);
    };
    let platform_limit = max_open_files as libc::rlim_t;
    if platform_limit as u128 != max_open_files as u128 {
        return Err(format!(
            "max_open_files {max_open_files} cannot be represented on this platform"
        ));
    }
    if platform_limit == libc::RLIM_INFINITY {
        return Err("max_open_files must be finite".to_string());
    }

    let mut parent_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut parent_limit) } != 0 {
        return Err(format!(
            "failed to inspect parent RLIMIT_NOFILE: {}",
            std::io::Error::last_os_error()
        ));
    }
    if parent_limit.rlim_max != libc::RLIM_INFINITY && platform_limit > parent_limit.rlim_max {
        return Err(format!(
            "max_open_files {max_open_files} exceeds parent hard limit {}",
            parent_limit.rlim_max
        ));
    }

    Ok(Some(platform_limit))
}

#[cfg(all(test, not(unix)))]
mod resource_limit_tests {
    use super::*;

    #[test]
    fn configured_open_file_limit_is_refused() {
        let limits = SubprocessLimits {
            max_open_files: Some(64),
            ..SubprocessLimits::default()
        };

        let error = validate_subprocess_limits(Some(&limits)).unwrap_err();

        assert!(error.contains("unsupported"), "{error}");

        let mut command = process::Command::new("unused");
        let error = configure_subprocess_limits(&mut command, Some(&limits)).unwrap_err();
        assert!(error.contains("unsupported"), "{error}");
    }
}

/// Run a subprocess synchronously and return structured results.
pub fn lib_run(request: SubprocessRequest) -> SubprocessResult {
    match lib_spawn(request) {
        Ok(running) => running.wait(),
        Err(result) => result,
    }
}

pub fn lib_run_inherited_stdio(request: SubprocessRequest) -> SubprocessResult {
    match lib_spawn_inherited_stdio(request) {
        Ok(running) => running.wait(),
        Err(result) => result,
    }
}

/// Spawn a detached subprocess.
pub fn lib_spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[(String, String)],
) -> Result<SpawnResult, String> {
    let envs_str: Vec<String> = envs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    spawn_detached(cmd, args, log, &envs_str, None).map(|pid| SpawnResult { pid })
}

/// Kill a process by PID. Returns the method used: "terminated", "killed", or "already_dead".
pub fn lib_kill(pid: u32, grace: f64) -> Result<String, String> {
    kill_process(pid, grace).map(|s| s.to_string())
}

/// Check if a process is alive.
pub fn lib_is_alive(pid: u32) -> bool {
    is_alive(pid)
}

// ---------------------------------------------------------------------------
// CLI types and entry point
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum ExecAction {
    /// Run a command, wait for completion, capture output
    Run {
        #[arg(long)]
        cmd: String,
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        stdin: Option<String>,
        #[arg(long)]
        stdin_pipe: bool,
        #[arg(long = "env")]
        envs: Vec<String>,
        #[arg(long, default_value_t = 300.0)]
        timeout: f64,
    },
    /// Spawn a detached/daemonized child process
    Spawn {
        #[arg(long)]
        cmd: String,
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long)]
        log: Option<String>,
        #[arg(long = "env")]
        envs: Vec<String>,
        #[arg(long)]
        stdin: Option<String>,
        #[arg(long)]
        stdin_pipe: bool,
    },
    /// Kill a process by PID
    Kill {
        #[arg(long)]
        pid: u32,
        #[arg(long, default_value_t = 3.0)]
        grace: f64,
    },
    /// Stream a command's output with raw passthrough (no JSON wrapping)
    Stream {
        #[arg(long)]
        cmd: String,
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        stdin: Option<String>,
        #[arg(long)]
        stdin_pipe: bool,
        #[arg(long = "env")]
        envs: Vec<String>,
        #[arg(long, default_value_t = 300.0)]
        timeout: f64,
    },
    /// Check if a process is alive
    Status {
        #[arg(long)]
        pid: u32,
    },
}

fn resolve_stdin(stdin_arg: Option<String>, stdin_pipe: bool) -> Option<String> {
    if let Some(data) = stdin_arg {
        return Some(data);
    }
    if stdin_pipe {
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        if !buf.is_empty() {
            return Some(buf);
        }
    }
    None
}

/// Apply env key=value pairs to a Command. Callers should call
/// `command.env_clear()` before this to ensure `envs` is authoritative.
fn set_envs(command: &mut process::Command, envs: &[String]) {
    for env in envs {
        if let Some((k, v)) = env.split_once('=') {
            command.env(k, v);
        }
    }
}

fn write_stdin(child: &mut process::Child, data: Option<&str>) {
    if let Some(data) = data {
        if let Some(mut s) = child.stdin.take() {
            let _ = s.write_all(data.as_bytes());
        }
    }
}

fn setup_log(command: &mut process::Command, log: Option<&str>) -> Result<(), String> {
    if let Some(path) = log {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|e| format!("Failed to open log file: {e}"))?;
        let file2 = file
            .try_clone()
            .map_err(|e| format!("Failed to clone log fd: {e}"))?;
        command.stdout(file).stderr(file2);
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    Ok(())
}

pub fn run(action: ExecAction) -> serde_json::Value {
    match action {
        ExecAction::Run {
            cmd,
            args,
            cwd,
            stdin,
            stdin_pipe,
            envs,
            timeout,
        } => do_exec(
            &cmd,
            &args,
            cwd.as_deref(),
            resolve_stdin(stdin, stdin_pipe).as_deref(),
            &envs,
            timeout,
        ),
        ExecAction::Spawn {
            cmd,
            args,
            log,
            envs,
            stdin,
            stdin_pipe,
        } => {
            match spawn_detached(
                &cmd,
                &args,
                log.as_deref(),
                &envs,
                resolve_stdin(stdin, stdin_pipe).as_deref(),
            ) {
                Ok(pid) => serde_json::json!({ "success": true, "pid": pid }),
                Err(e) => serde_json::json!({ "success": false, "error": e }),
            }
        }
        ExecAction::Stream {
            cmd,
            args,
            cwd,
            stdin,
            stdin_pipe,
            envs,
            timeout,
        } => {
            let code = do_stream(
                &cmd,
                &args,
                cwd.as_deref(),
                resolve_stdin(stdin, stdin_pipe).as_deref(),
                &envs,
                timeout,
            );
            process::exit(code);
        }
        ExecAction::Kill { pid, grace } => match kill_process(pid, grace) {
            Ok(method) => serde_json::json!({ "success": true, "pid": pid, "method": method }),
            Err(e) => serde_json::json!({ "success": false, "pid": pid, "error": e }),
        },
        ExecAction::Status { pid } => serde_json::json!({ "pid": pid, "alive": is_alive(pid) }),
    }
}

fn do_exec(
    cmd: &str,
    args: &[String],
    cwd: Option<&str>,
    stdin_data: Option<&str>,
    envs: &[String],
    timeout: f64,
) -> serde_json::Value {
    let r = lib_run(SubprocessRequest {
        cmd: cmd.to_string(),
        argv0: None,
        args: args.to_vec(),
        cwd: cwd.map(|s| s.to_string()),
        envs: envs
            .iter()
            .filter_map(|e| {
                e.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect(),
        stdin_data: stdin_data.map(|s| s.to_string()),
        timeout,
        limits: None,
        inherited_fds: Vec::new(),
        supervised_status: None,
    });
    serde_json::json!({
        "success": r.success, "stdout": r.stdout, "stderr": r.stderr,
        "return_code": r.exit_code, "duration_ms": r.duration_ms,
        "timed_out": r.timed_out,
        "output_limit_exceeded": r.output_limit_exceeded.map(OutputLimitExceeded::as_str),
        "stdout_truncated": r.stdout_truncated,
        "stderr_truncated": r.stderr_truncated,
    })
}

/// Stream mode: raw passthrough of child stdout/stderr, no JSON wrapping.
/// Returns: child exit code, 124 on timeout, 125 on spawn failure.
fn do_stream(
    cmd: &str,
    args: &[String],
    cwd: Option<&str>,
    stdin_data: Option<&str>,
    envs: &[String],
    timeout: f64,
) -> i32 {
    let mut command = process::Command::new(cmd);
    command.args(args);
    command.env_clear();
    set_envs(&mut command, envs);
    // Set PYTHONUNBUFFERED for Python children to ensure streaming latency
    command.env("PYTHONUNBUFFERED", "1");
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to spawn: {e}");
            return 125;
        }
    };

    let stderr_handle = child.stderr.take();
    let stderr_thread = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        if let Some(mut err) = stderr_handle {
            let mut stderr_out = std::io::stderr();
            loop {
                match err.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = stderr_out.write_all(&buf[..n]);
                        let _ = stderr_out.flush();
                    }
                    Err(_) => break,
                }
            }
        }
    });

    // Forward stdout: raw chunks with flush
    let stdout_handle = child.stdout.take();
    let stdout_thread = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        if let Some(mut out) = stdout_handle {
            let mut stdout_out = std::io::stdout();
            loop {
                match out.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = stdout_out.write_all(&buf[..n]);
                        let _ = stdout_out.flush();
                    }
                    Err(_) => break,
                }
            }
        }
    });

    // Wait with timeout. A non-positive timeout is the no-timeout sentinel.
    let timeout_rx = if let Some(timeout_dur) = request_timeout_duration(timeout) {
        let (tx, rx) = std::sync::mpsc::channel();
        let _timer = thread::spawn(move || {
            thread::sleep(timeout_dur);
            let _ = tx.send(());
        });
        Some(rx)
    } else {
        None
    };
    let stream_stop = Arc::new(AtomicBool::new(false));
    let mut stdin_thread = match spawn_stdin_writer(
        child.stdin.take(),
        stdin_data.map(str::to_owned),
        Arc::clone(&stream_stop),
    ) {
        Ok(thread) => thread,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            stream_stop.store(true, Ordering::Release);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            eprintln!("Failed to configure nonblocking stdin: {error}");
            return 125;
        }
    };

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                stream_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread.take() {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return status.code().unwrap_or(1);
            }
            Ok(None) => {
                if timeout_rx.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    stream_stop.store(true, Ordering::Release);
                    if let Some(handle) = stdin_thread.take() {
                        let _ = handle.join();
                    }
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    eprintln!("Command timed out after {timeout} seconds");
                    return 124;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                stream_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread.take() {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                eprintln!("Wait failed: {e}");
                return 125;
            }
        }
    }
}

#[cfg(unix)]
fn spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[String],
    stdin_data: Option<&str>,
) -> Result<u32, String> {
    use std::os::unix::process::CommandExt;
    let mut command = process::Command::new(cmd);
    command.args(args);
    command.env_clear();
    set_envs(&mut command, envs);
    command.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    setup_log(&mut command, log)?;
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn: {e}"))?;
    write_stdin(&mut child, stdin_data);
    Ok(child.id())
}

#[cfg(windows)]
fn spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[String],
    stdin_data: Option<&str>,
) -> Result<u32, String> {
    use std::os::windows::process::CommandExt;
    let mut command = process::Command::new(cmd);
    command.args(args);
    command.env_clear();
    set_envs(&mut command, envs);
    command.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    setup_log(&mut command, log)?;
    command.creation_flags(0x00000200 | 0x00000008); // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn: {e}"))?;
    write_stdin(&mut child, stdin_data);
    Ok(child.id())
}

#[cfg(unix)]
fn kill_process(pid: u32, grace: f64) -> Result<&'static str, String> {
    let pid = pid as i32;
    if unsafe { libc::kill(pid, 0) } != 0 {
        return Ok("already_dead");
    }
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(format!(
            "SIGTERM failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    for _ in 0..(grace / 0.1).ceil() as u32 {
        thread::sleep(Duration::from_millis(100));
        if unsafe { libc::kill(pid, 0) } != 0 {
            return Ok("terminated");
        }
    }
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return Ok("terminated");
        }
        return Err(format!(
            "SIGKILL failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok("killed")
}

#[cfg(windows)]
fn kill_process(pid: u32, grace: f64) -> Result<&'static str, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::*;
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle == 0 {
        return Ok("already_dead");
    }
    if unsafe { WaitForSingleObject(handle, (grace * 1000.0) as u32) } == WAIT_OBJECT_0 {
        unsafe { CloseHandle(handle) };
        return Ok("terminated");
    }
    let ok = unsafe { TerminateProcess(handle, 1) };
    unsafe { CloseHandle(handle) };
    if ok != 0 {
        Ok("killed")
    } else {
        Err(format!(
            "TerminateProcess failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::*;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle == 0 {
        return false;
    }
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    result != 0
}
