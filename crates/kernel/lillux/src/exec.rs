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
    /// child. The sandbox uses these only for Bubblewrap's fd-based mounts.
    pub inherited_fds: Vec<std::sync::Arc<std::fs::File>>,
    /// Optional trusted launcher status channel. When present, Lillux waits for
    /// Bubblewrap to report the host PID of its command and supervises that
    /// command's process group in addition to the outer Bubblewrap process.
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

/// Parent end of Bubblewrap's `--json-status-fd` channel.
///
/// Construct this only through [`bubblewrap_status_pipe`]. That factory and
/// the parser form one protocol: the paired writer must be inherited by
/// Bubblewrap and named by `--json-status-fd`; the launch must also use
/// `--new-session`, making the reported command PID its initial host PGID.
pub struct SupervisedProcessStatus {
    reader: std::fs::File,
}

/// Both ends needed to connect Lillux supervision to Bubblewrap.
pub struct BubblewrapStatusPipe {
    pub reader: SupervisedProcessStatus,
    pub writer: Arc<std::fs::File>,
}

impl BubblewrapStatusPipe {
    /// Raw descriptor to pass as Bubblewrap's `--json-status-fd` value.
    #[cfg(unix)]
    pub fn writer_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.writer.as_raw_fd()
    }

    #[cfg(not(unix))]
    pub fn writer_fd(&self) -> i32 {
        // Construction fails on non-Linux platforms, so this value is never
        // handed to a child. Keeping the method in the cross-platform API lets
        // shared sandbox plumbing compile without platform-specific branches.
        -1
    }
}

/// Create an atomically-CLOEXEC status pipe for Bubblewrap supervision.
///
/// The writer remains CLOEXEC in the multithreaded parent. Lillux clears that
/// bit only in the forked child through `inherited_fds`, avoiding descriptor
/// leaks into unrelated concurrent spawns.
#[cfg(target_os = "linux")]
pub fn bubblewrap_status_pipe() -> Result<BubblewrapStatusPipe, String> {
    use std::os::fd::FromRawFd as _;

    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "create Bubblewrap status pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: pipe2 initialized both owned descriptors on success. Each is
    // transferred into exactly one File below.
    let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok(BubblewrapStatusPipe {
        reader: SupervisedProcessStatus { reader },
        writer: Arc::new(writer),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn bubblewrap_status_pipe() -> Result<BubblewrapStatusPipe, String> {
    Err("Bubblewrap status supervision is supported only on Linux".to_string())
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
    /// Set when a node-owned stdout/stderr retention limit was crossed. This
    /// outcome always makes `success` false, independently of the exit status.
    pub output_limit_exceeded: Option<OutputLimitExceeded>,
    /// Whether bytes beyond the retained stdout prefix were drained/discarded.
    pub stdout_truncated: bool,
    /// Whether bytes beyond the retained stderr prefix were drained/discarded.
    pub stderr_truncated: bool,
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
    /// Whether the promised process group was observed while its leader was
    /// still alive. A status document can race a very short-lived target; in
    /// that case the public identity remains useful for accounting, but Lillux
    /// must not later signal an unverified, potentially recycled group ID.
    group_observed: bool,
}

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SUPERVISED_STATUS_SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const SUPERVISED_STATUS_MAX_LINE_BYTES: usize = 64 * 1024;

/// A running subprocess that can be waited on later.
pub struct RunningProcess {
    /// Identity of the supervised command. For a direct launch this is the
    /// spawned child; for Bubblewrap it is the command reported over
    /// `--json-status-fd`.
    pub pid: u32,
    pub pgid: i64,
    /// The outer process is retained separately so timeout/overflow cleanup
    /// always reaps Bubblewrap as well as the command group.
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
    target_group_observed: bool,
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
        let timeout = if self.timeout.is_finite() && self.timeout > 0.0 {
            Some(Duration::from_secs_f64(self.timeout))
        } else {
            None
        };

        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.wrapper_reaped = true;
                    // A command may exit after leaving ordinary background
                    // descendants in its process group. Synchronous execution
                    // owns the whole group, so do not let those descendants
                    // outlive the reported completion.
                    self.kill_supervised_processes();
                    let (out, err) = self.finish_drains();
                    let code = status.code().unwrap_or(-1);
                    if let Some(exceeded) = output_limit_exceeded(&out, &err) {
                        return self.output_limit_result(out, err, exceeded);
                    }
                    return SubprocessResult {
                        success: code == 0,
                        stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
                        stderr: String::from_utf8_lossy(&err.bytes).into_owned(),
                        exit_code: code,
                        duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
                        pid: self.pid,
                        timed_out: false,
                        output_limit_exceeded: None,
                        stdout_truncated: false,
                        stderr_truncated: false,
                    };
                }
                Ok(None) => {
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
                Err(e) => {
                    // A failed wait must not silently orphan the supervised
                    // command or its launcher.
                    self.kill_supervised_processes();
                    self.reap_wrapper();
                    let (out, err) = self.finish_drains();
                    return SubprocessResult {
                        success: false,
                        stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
                        stderr: append_diagnostic(
                            &String::from_utf8_lossy(&err.bytes),
                            &format!("Wait failed: {e}"),
                        ),
                        exit_code: -1,
                        duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
                        pid: self.pid,
                        timed_out: false,
                        output_limit_exceeded: output_limit_exceeded(&out, &err),
                        stdout_truncated: out.truncated,
                        stderr_truncated: err.truncated,
                    };
                }
            }
        }
    }

    fn kill_supervised_processes(&mut self) {
        if self.groups_terminated {
            return;
        }
        #[cfg(unix)]
        {
            debug_assert_eq!(self.wrapper_pgid, self.wrapper_pid as i64);
            // Kill the target group first. Bubblewrap's `--new-session` puts
            // it outside the wrapper's group, so killing only the latter is
            // insufficient. Then kill the wrapper group to force teardown and
            // reaping. A descendant that deliberately creates another session
            // is outside this local guarantee; hosted workers use cgroup.kill.
            kill_observed_process_group(
                self.pid,
                self.pgid,
                self.target_group_observed,
            );
            if self.wrapper_pgid != self.pgid {
                kill_observed_process_group(self.wrapper_pid, self.wrapper_pgid, true);
            }
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
        // are already buffered and stop at the next WouldBlock. This prevents
        // an escaped setsid descendant holding a pipe open from hanging the
        // daemon, while preserving normal output already written.
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

// ---------------------------------------------------------------------------
// Library functions — public API for in-process callers
// ---------------------------------------------------------------------------

/// Spawn a subprocess and return a handle that can be waited on later.
pub fn lib_spawn(request: SubprocessRequest) -> Result<RunningProcess, SubprocessResult> {
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
    command.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
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
    // writer open here would hide Bubblewrap's pre-command EOF and force every
    // failed setup to wait for the full supervision timeout.
    drop(inherited_fds);
    let wrapper_pid = child.id();

    // On Unix with setsid, pid == pgid since the child is its own process group leader.
    #[cfg(unix)]
    let wrapper_pgid = wrapper_pid as i64;
    #[cfg(not(unix))]
    let wrapper_pgid = -1i64;

    let mut stdout_handle = child.stdout.take().expect("stdout configured as piped");
    let mut stderr_handle = child.stderr.take().expect("stderr configured as piped");
    if let Err(error) = configure_nonblocking_capture(&mut stdout_handle)
        .and_then(|_| configure_nonblocking_capture(&mut stderr_handle))
    {
        kill_process_group_if_safe(wrapper_pgid);
        let _ = child.wait();
        return Err(spawn_failure(
            start,
            format!("Failed to spawn: configure bounded output capture: {error}"),
        ));
    }

    let stdout_capture = Arc::new(Mutex::new(BoundedCapture::default()));
    let stderr_capture = Arc::new(Mutex::new(BoundedCapture::default()));
    let drain_stop = Arc::new(AtomicBool::new(false));
    let (output_overflow_tx, output_overflow_rx) = std::sync::mpsc::channel();
    let stdout_thread = spawn_bounded_drain(
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
    );
    let stderr_thread = spawn_bounded_drain(
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
    );

    // Never write request input on the spawning thread. A child can stop
    // reading before the pipe buffer is empty; the dedicated writer may then
    // block, but bounded stdout/stderr draining and the request deadline are
    // already established and remain able to terminate the workload.
    let stdin_thread = spawn_stdin_writer(child.stdin.take(), stdin_data);

    let (identity, status_thread) = if let Some(status) = supervised_status {
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let status_thread = match spawn_bubblewrap_status_reader(status, status_tx) {
            Ok(handle) => handle,
            Err(error) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(spawn_failure(
                    start,
                    format!("Failed to spawn: initialize Bubblewrap status reader: {error}"),
                ));
            }
        };
        let setup_deadline = supervised_setup_deadline(start, timeout);
        let setup_wait = setup_deadline.saturating_duration_since(Instant::now());
        let reported_pid = match status_rx.recv_timeout(setup_wait) {
            Ok(Ok(pid)) => pid,
            Ok(Err(error)) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let _ = status_thread.join();
                return Err(spawn_failure(
                    start,
                    format!("Failed to spawn: Bubblewrap status refused: {error}"),
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let _ = status_thread.join();
                return Err(spawn_failure(
                    start,
                    format!(
                        "Failed to spawn: Bubblewrap did not report its command PID before the bounded setup/request deadline ({:.3} seconds remaining after launch setup)",
                        setup_wait.as_secs_f64()
                    ),
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let _ = status_thread.join();
                return Err(spawn_failure(
                    start,
                    "Failed to spawn: Bubblewrap status channel closed before reporting its command PID",
                ));
            }
        };
        let identity = match resolve_supervised_identity(
            reported_pid,
            wrapper_pid,
            wrapper_pgid,
            setup_deadline,
        ) {
            Ok(identity) => identity,
            Err(error) => {
                kill_process_group_if_safe(wrapper_pgid);
                let _ = child.wait();
                drain_stop.store(true, Ordering::Release);
                if let Some(handle) = stdin_thread {
                    let _ = handle.join();
                }
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let _ = status_thread.join();
                return Err(spawn_failure(
                    start,
                    format!("Failed to spawn: invalid Bubblewrap command identity: {error}"),
                ));
            }
        };
        (identity, Some(status_thread))
    } else {
        (
            ProcessIdentity {
                pid: wrapper_pid,
                pgid: wrapper_pgid,
                group_observed: true,
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
        target_group_observed: identity.group_observed,
        groups_terminated: false,
        wrapper_reaped: false,
    })
}

fn spawn_stdin_writer(
    stdin: Option<process::ChildStdin>,
    data: Option<String>,
) -> Option<thread::JoinHandle<()>> {
    let (Some(mut stdin), Some(data)) = (stdin, data) else {
        return None;
    };
    Some(thread::spawn(move || {
        let _ = stdin.write_all(data.as_bytes());
    }))
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
        loop {
            match reader.read(&mut read_buffer) {
                Ok(0) => break,
                Ok(read) => {
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
fn configure_nonblocking_capture<T>(reader: &mut T) -> Result<(), String>
where
    T: std::os::fd::AsRawFd,
{
    use std::os::fd::AsRawFd as _;

    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(format!(
            "set O_NONBLOCK on capture descriptor {fd}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn configure_nonblocking_capture<T>(_reader: &mut T) -> Result<(), String> {
    Ok(())
}

fn spawn_bubblewrap_status_reader(
    mut status: SupervisedProcessStatus,
    initial_tx: std::sync::mpsc::Sender<Result<u32, String>>,
) -> Result<thread::JoinHandle<()>, String> {
    Ok(thread::spawn(move || {
        let mut initial_tx = Some(initial_tx);
        let mut pending = Vec::new();
        let mut buffer = [0u8; 4096];

        loop {
            match status.reader.read(&mut buffer) {
                Ok(0) => {
                    if !pending.is_empty() && initial_tx.is_some() {
                        report_bubblewrap_status_line(&pending, &mut initial_tx);
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
                            report_bubblewrap_status_line(&remainder, &mut initial_tx);
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
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

fn report_bubblewrap_status_line(
    line: &[u8],
    initial_tx: &mut Option<std::sync::mpsc::Sender<Result<u32, String>>>,
) {
    let document: serde_json::Value = match serde_json::from_slice(line) {
        Ok(document) => document,
        Err(error) => {
            if let Some(tx) = initial_tx.take() {
                let _ = tx.send(Err(format!("invalid JSON status document: {error}")));
            }
            return;
        }
    };
    let Some(value) = document.get("child-pid") else {
        return;
    };
    let result = value
        .as_u64()
        .ok_or_else(|| "child-pid must be an unsigned integer".to_string())
        .and_then(|pid| {
            u32::try_from(pid).map_err(|_| format!("child-pid {pid} exceeds the host PID range"))
        });
    if let Some(tx) = initial_tx.take() {
        let _ = tx.send(result);
    }
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

    // Bubblewrap writes child-pid after starting the command, but the status
    // write and the command's `--new-session` setsid can be observed in either
    // order. Wait briefly for the promised target PGID instead of accidentally
    // publishing the wrapper group as the workload identity.
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let pgid = unsafe { libc::getpgid(pid_i32) };
        if pgid == pid_i32 {
            let current_pgid = unsafe { libc::getpgrp() };
            if pgid <= 1 || pgid == current_pgid || pgid as i64 == wrapper_pgid {
                return Err(format!("unsafe child process group {pgid}"));
            }
            return Ok(ProcessIdentity {
                pid,
                pgid: pgid as i64,
            });
        }
        if pgid < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                // A very short-lived command can exit between the status write
                // and getpgid. Under the factory's `--new-session` contract its
                // initial PGID was its PID; the wrapper wait will now settle it.
                return Ok(ProcessIdentity {
                    pid,
                    pgid: pid as i64,
                });
            }
            return Err(format!("getpgid({pid}) failed: {error}"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "child PID {pid} did not establish its promised process group (observed {pgid})"
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(not(unix))]
fn resolve_supervised_identity(
    _pid: u32,
    _wrapper_pid: u32,
    _wrapper_pgid: i64,
) -> Result<ProcessIdentity, String> {
    Err("supervised process identity is unsupported on this platform".to_string())
}

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
    let max_open_files: libc::rlim_t = max_open_files.try_into().map_err(|_| {
        format!("max_open_files {max_open_files} cannot be represented on this platform")
    })?;
    if max_open_files == libc::RLIM_INFINITY {
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
    if parent_limit.rlim_max != libc::RLIM_INFINITY && max_open_files > parent_limit.rlim_max {
        return Err(format!(
            "max_open_files {max_open_files} exceeds parent hard limit {}",
            parent_limit.rlim_max
        ));
    }

    Ok(Some(max_open_files))
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
    write_stdin(&mut child, stdin_data);

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
    let timeout_rx = if timeout > 0.0 {
        let timeout_dur = Duration::from_secs_f64(timeout);
        let (tx, rx) = std::sync::mpsc::channel();
        let _timer = thread::spawn(move || {
            thread::sleep(timeout_dur);
            let _ = tx.send(());
        });
        Some(rx)
    } else {
        None
    };

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return status.code().unwrap_or(1);
            }
            Ok(None) => {
                if timeout_rx.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    eprintln!("Command timed out after {timeout} seconds");
                    return 124;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
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
