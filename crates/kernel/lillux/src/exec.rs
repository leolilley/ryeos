use std::io::{Read, Write};
use std::process::{self, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
pub struct SupervisedProcessStatus {
    reader: std::fs::File,
}

/// Both ends needed to connect Lillux supervision to a trusted launcher.
pub struct SupervisedLauncherStatusPipe {
    pub reader: SupervisedProcessStatus,
    pub writer: Arc<std::fs::File>,
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
        reader: SupervisedProcessStatus { reader },
        writer: Arc::new(writer),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn supervised_launcher_status_pipe() -> Result<SupervisedLauncherStatusPipe, String> {
    Err("supervised-launcher status is supported only on Linux".to_string())
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
const SUPERVISED_STATUS_MAX_LINE_BYTES: usize = 64 * 1024;

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
    groups_terminated: bool,
    wrapper_reaped: bool,
}

impl RunningProcess {
    /// Terminate every supervised process group and reap the outer child.
    ///
    /// This consumes the handle so callers cannot accidentally wait on or
    /// publish an execution after aborting it. Dropping a handle without
    /// calling either `wait` or `abort` performs the same fail-safe cleanup.
    pub fn abort(mut self) {
        self.abort_and_reap();
    }

    /// Wait for the process to finish (or time out) and return the result.
    pub fn wait(mut self) -> SubprocessResult {
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

    fn abort_and_reap(&mut self) {
        self.kill_supervised_processes();
        self.reap_wrapper();
        let _ = self.finish_drains();
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
        self.abort_and_reap();
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
    lib_spawn_with_stdio(request, false)
}

/// Spawn with inherited terminal stdio while retaining the same session,
/// supervised-launcher status, timeout, process-group cleanup, and wait
/// contract as captured execution.
pub fn lib_spawn_inherited_stdio(
    request: SubprocessRequest,
) -> Result<RunningProcess, SubprocessResult> {
    lib_spawn_with_stdio(request, true)
}

fn lib_spawn_with_stdio(
    request: SubprocessRequest,
    inherit_stdio: bool,
) -> Result<RunningProcess, SubprocessResult> {
    let start = Instant::now();
    let SubprocessRequest {
        cmd,
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

    #[cfg(unix)]
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

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Err(spawn_failure(start, format!("Failed to spawn: {e}"))),
    };
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

    let (identity, status_thread) = if let Some(status) = supervised_status {
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let status_thread = match spawn_supervised_launcher_status_reader(
            status,
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
        (identity, Some(status_thread))
    } else {
        (
            ProcessIdentity {
                pid: wrapper_pid,
                pgid: wrapper_pgid,
            },
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
    mut status: SupervisedProcessStatus,
    initial_tx: std::sync::mpsc::Sender<Result<InitialLauncherStatus, String>>,
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, String> {
    configure_nonblocking_fd(&mut status.reader)
        .map_err(|error| format!("configure nonblocking status channel: {error}"))?;
    Ok(thread::spawn(move || {
        let mut initial_tx = Some(initial_tx);
        let mut pending = Vec::new();
        let mut buffer = [0u8; 4096];

        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            match status.reader.read(&mut buffer) {
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
