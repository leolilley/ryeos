//! Exact child execution inside an already-owned enclosing process lifetime.
//!
//! Unlike the buffered subprocess runner, this capability does not create or
//! control a process group. It owns one direct child, its standard streams,
//! exact-child cooperative cancellation and reap. The enclosing RyeOS process
//! authority remains responsible for descendant containment and cleanup.

use std::io::{Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::exec::{
    CooperativeChildTermination, InheritedDescriptorAuthority, SubprocessLimits,
    configure_command_argv0, configure_command_piped_stdio,
    configure_inherited_descriptor_authorities, configure_owner_private_creation_mask,
    configure_subprocess_limits,
};
use crate::secure_fs::{PinnedDirectory, PinnedRegularFile};
use crate::time::MonotonicDeadline;

pub struct SubordinateProcessRequest {
    pub cmd: String,
    pub argv0: Option<String>,
    pub args: Vec<String>,
    pub cwd: String,
    pub envs: Vec<(String, String)>,
    pub limits: Option<SubprocessLimits>,
    pub inherited_fds: Vec<InheritedDescriptorAuthority>,
}

/// Launch a direct child from exact descriptor-owned executable and cwd
/// identities. Ordinary path strings remain appropriate only for cases where
/// the caller has separately established an immutable namespace.
pub struct PinnedSubordinateProcessRequest {
    pub executable: PinnedRegularFile,
    pub cwd: PinnedDirectory,
    pub argv0: Option<String>,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub limits: Option<SubprocessLimits>,
    pub inherited_fds: Vec<InheritedDescriptorAuthority>,
}

/// One exact subordinate child. This never attests descendant settlement.
pub struct SubordinateProcess {
    child: Option<Child>,
    stdin: Option<SubordinateProcessInput>,
    stdout: Option<SubordinateProcessOutput>,
    stderr: Option<SubordinateProcessError>,
    reaped: bool,
}

/// Unresolved exact-child ownership retained until enclosing root termination.
///
/// This opaque, non-Clone owner grants no success or settlement evidence and
/// exposes no child PID or signaling capability. Its destructor requests
/// best-effort exact-child termination but NEVER waits or reaps. The enclosing
/// process-scope authority must terminate and settle the root and descendants.
/// Keep this owner in the invocation's typed terminal failure state until root
/// exit. Dropping it early abandons observation; it is never successful cleanup.
///
/// Streams still held by SubordinateProcess transfer here. Streams already taken
/// and diagnostic tasks remain separately owned by the invocation and must also
/// be retained through unresolved failure. This does not capture those owners.
#[must_use = "retain unresolved child ownership through terminal root failure"]
pub struct RootTerminalSubordinateFailure {
    child: Option<Child>,
    _stdin: Option<SubordinateProcessInput>,
    _stdout: Option<SubordinateProcessOutput>,
    _stderr: Option<SubordinateProcessError>,
    reaped: bool,
}

impl Drop for RootTerminalSubordinateFailure {
    fn drop(&mut self) {
        if !self.reaped {
            if let Some(child) = self.child.as_mut() {
                // Child::kill requests exact-child termination. No wait,
                // try_wait, task join or descendant settlement occurs here.
                let _ = child.kill();
            }
        }
        // std::process::Child destruction does not wait. Pipe destruction
        // closes retained descriptors; it does not produce EOF/reap evidence.
    }
}

pub struct SubordinateProcessInput {
    input: ChildStdin,
}

pub struct SubordinateProcessOutput {
    output: ChildStdout,
}

pub struct SubordinateProcessError {
    output: ChildStderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubordinateProcessExit {
    pub success: bool,
    pub code: Option<i32>,
}

/// Bounded retained diagnostics. EOF only describes this pipe, not the process
/// tree. A deadline returns partial evidence with `eof == false`.
#[derive(Debug, PartialEq, Eq)]
pub struct SubordinateProcessDiagnostics {
    pub bytes: Vec<u8>,
    pub truncated: bool,
    pub eof: bool,
}

/// Observation of the diagnostic reader only, never child or descendant death.
/// No provider bytes or panic payloads are included in this public observation.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubordinateDiagnosticDrainEnd {
    Eof,
    Cancelled,
    ReadFailed(std::io::ErrorKind),
    Panicked,
}

/// Owned zero-retention diagnostic reader. Explicit completion joins its task;
/// timeout retains this owner for another observation or enclosing cleanup.
/// Drop requests cancellation as a last-resort mitigation, but loses join
/// authority and is NOT successful settlement. Callers must use explicit
/// completion before claiming reader shutdown. No method signals a process.
#[must_use = "retain and explicitly settle the diagnostic reader"]
pub struct SubordinateDiagnosticDrain {
    stop: Arc<AtomicBool>,
    task: Option<crate::task::HostTask<SubordinateDiagnosticDrainEnd>>,
    end: Option<SubordinateDiagnosticDrainEnd>,
}

impl SubordinateDiagnosticDrain {
    /// Observe completion under one absolute deadline without cancelling the
    /// reader. None retains ownership; EOF is not inferred from child exit.
    pub fn finish_until(
        &mut self,
        deadline: MonotonicDeadline,
    ) -> Option<SubordinateDiagnosticDrainEnd> {
        loop {
            if let Some(end) = self.end {
                return Some(end);
            }
            if self
                .task
                .as_ref()
                .expect("unsettled reader owns task")
                .is_finished()
            {
                let end = self
                    .task
                    .take()
                    .expect("observed completed task")
                    .join()
                    .unwrap_or(SubordinateDiagnosticDrainEnd::Panicked);
                self.end = Some(end);
                return Some(end);
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return None;
            }
            crate::time::sleep(remaining.min(crate::time::Duration::from_millis(5)));
        }
    }

    /// Stop reading and observe task completion. Cancellation is not EOF, and
    /// closing the pipe may cause a still-writing producer to receive EPIPE.
    /// The caller must select this consequence under its cleanup policy.
    pub fn cancel_until(
        &mut self,
        deadline: MonotonicDeadline,
    ) -> Option<SubordinateDiagnosticDrainEnd> {
        self.stop.store(true, Ordering::Release);
        self.finish_until(deadline)
    }
}

impl Drop for SubordinateDiagnosticDrain {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // HostTask destruction detaches. This is abandonment of observation,
        // not a cleanup receipt; explicit finish retains the handle on timeout.
    }
}

/// How long a subordinate launch may wait for the process-creation window.
#[cfg(unix)]
const SUBORDINATE_FORK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(unix)]
type SubordinateForkCustody = crate::exec::QuiescedForkSensitiveDescriptors;
#[cfg(not(unix))]
type SubordinateForkCustody = ();

/// Hold the process-creation window for this launch and install the
/// registered-descriptor scrub as the command's first pre-exec hook, so the
/// child closes every registered parent descriptor except its own inherited
/// authorities before anything else can run or block in it. Call this before
/// any other pre-exec hook is configured.
fn prepare_subordinate_fork_custody(
    command: &mut Command,
    inherited_fds: &[InheritedDescriptorAuthority],
) -> Result<SubordinateForkCustody, String> {
    #[cfg(unix)]
    {
        let mut preserved = std::collections::BTreeSet::from([0, 1, 2]);
        preserved.extend(crate::exec::normalize_inherited_descriptors(inherited_fds)?);
        let deadline = std::time::Instant::now()
            .checked_add(SUBORDINATE_FORK_WAIT)
            .ok_or("subordinate process fork wait bound overflow")?;
        crate::exec::prepare_command_fork_custody(command, &preserved, deadline)
            .map_err(|error| format!("subordinate process fork custody: {error}"))
    }
    #[cfg(not(unix))]
    {
        let _ = (command, inherited_fds);
        Ok(())
    }
}

impl SubordinateProcess {
    pub fn spawn(request: SubordinateProcessRequest) -> Result<Self, String> {
        if request.cmd.is_empty() || request.cwd.is_empty() {
            return Err("subordinate process command and cwd must be nonempty".to_owned());
        }
        let mut command = Command::new(&request.cmd);
        command.current_dir(&request.cwd);
        let custody = prepare_subordinate_fork_custody(&mut command, &request.inherited_fds)?;
        Self::spawn_configured(
            command,
            custody,
            request.argv0,
            request.args,
            request.envs,
            request.limits,
            request.inherited_fds,
        )
    }

    /// The executable path is an inherited descriptor spelling, not a
    /// reopened staged pathname. `fchdir` selects the exact pinned cwd inside
    /// the child before exec. The enclosing RyeOS authority must still own
    /// descendants and settlement; this method owns only the direct child.
    pub fn spawn_pinned(mut request: PinnedSubordinateProcessRequest) -> Result<Self, String> {
        request
            .executable
            .require_executable()
            .map_err(|error| error.to_string())?;
        let executable = request
            .executable
            .into_inherited_descriptor_path()
            .map_err(|error| error.to_string())?;
        let mut command = Command::new(executable.path());
        request.inherited_fds.push(executable);
        // The scrub hook precedes the descriptor-selected cwd hook.
        let custody = prepare_subordinate_fork_custody(&mut command, &request.inherited_fds)?;
        request
            .cwd
            .configure_command_cwd(&mut command)
            .map_err(|error| error.to_string())?;
        Self::spawn_configured(
            command,
            custody,
            request.argv0,
            request.args,
            request.envs,
            request.limits,
            request.inherited_fds,
        )
    }

    fn spawn_configured(
        mut command: Command,
        custody: SubordinateForkCustody,
        argv0: Option<String>,
        args: Vec<String>,
        envs: Vec<(String, String)>,
        limits: Option<SubprocessLimits>,
        inherited_fds: Vec<InheritedDescriptorAuthority>,
    ) -> Result<Self, String> {
        if let Some(argv0) = argv0.as_deref() {
            configure_command_argv0(&mut command, argv0)?;
        }
        configure_command_piped_stdio(&mut command);
        command.args(&args).env_clear().envs(
            envs.iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
        configure_owner_private_creation_mask(&mut command);
        configure_inherited_descriptor_authorities(&mut command, &inherited_fds)?;
        configure_subprocess_limits(&mut command, limits.as_ref())?;

        // The window reopens once the child has exec'd, or once the creation
        // witness proves no created process still holds its pre-exec table.
        #[cfg(unix)]
        let spawned = crate::exec::spawn_command_with_fork_custody(&mut command, Some(custody));
        #[cfg(not(unix))]
        let spawned = {
            let () = custody;
            command.spawn()
        };
        let mut child = spawned.map_err(|error| format!("spawn subordinate process: {error}"))?;
        // After a successful spawn, return the owner without another fallible
        // host operation. Stream acquisition/configuration errors must leave
        // this exact child with its caller, not kill/wait inside an error path.
        let stdin = child
            .stdin
            .take()
            .map(|input| SubordinateProcessInput { input });
        let stdout = child
            .stdout
            .take()
            .map(|output| SubordinateProcessOutput { output });
        let stderr = child
            .stderr
            .take()
            .map(|output| SubordinateProcessError { output });
        Ok(Self {
            child: Some(child),
            stdin,
            stdout,
            stderr,
            reaped: false,
        })
    }

    pub fn take_input(&mut self) -> Result<SubordinateProcessInput, String> {
        let input = self
            .stdin
            .as_mut()
            .ok_or_else(|| "subordinate process input is absent or already taken".to_owned())?;
        configure_input_nonblocking(&mut input.input).map_err(|error| {
            format!("configure subordinate process input backpressure: {error}")
        })?;
        Ok(self.stdin.take().expect("configured owned input"))
    }

    pub fn take_output(&mut self) -> Result<SubordinateProcessOutput, String> {
        self.stdout
            .take()
            .ok_or_else(|| "subordinate process output was already taken".to_owned())
    }

    pub fn take_error(&mut self) -> Result<SubordinateProcessError, String> {
        self.stderr
            .take()
            .ok_or_else(|| "subordinate process error stream was already taken".to_owned())
    }

    pub fn try_exit(&mut self) -> Result<Option<SubordinateProcessExit>, String> {
        self.child
            .as_mut()
            .expect("ordinary subordinate owns exact child")
            .try_wait()
            .map(|status| {
                status.map(|status| {
                    self.reaped = true;
                    SubordinateProcessExit {
                        success: status.success(),
                        code: status.code(),
                    }
                })
            })
            .map_err(|error| format!("observe subordinate process exit: {error}"))
    }

    pub fn cooperative_termination(&self) -> Result<CooperativeChildTermination, String> {
        CooperativeChildTermination::for_child(
            self.child.as_ref().expect("ordinary subordinate owns exact child"),
        )
    }

    /// Observe/reap only this child under one absolute deadline. Timeout does
    /// not signal it, discard ownership, or attest descendant cleanup. The
    /// caller must retain this value and settle its enclosing cleanup obligation.
    pub fn wait_exact_child_until(
        &mut self,
        deadline: MonotonicDeadline,
    ) -> Result<Option<SubordinateProcessExit>, String> {
        loop {
            if let Some(exit) = self.try_exit()? {
                return Ok(Some(exit));
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Ok(None);
            }
            crate::time::sleep(remaining.min(crate::time::Duration::from_millis(5)));
        }
    }

    /// Explicit caller-authorized forceful termination of this exact child,
    /// then deadline-bound reap. This never signals the enclosing process group.
    /// On timeout or error the child owner is retained, not detached.
    pub fn kill_exact_child_until(
        &mut self,
        deadline: MonotonicDeadline,
    ) -> Result<Option<SubordinateProcessExit>, String> {
        if let Some(exit) = self.try_exit()? {
            return Ok(Some(exit));
        }
        self.child
            .as_mut()
            .expect("ordinary subordinate owns exact child")
            .kill()
            .map_err(|error| format!("kill exact subordinate child: {error}"))?;
        self.wait_exact_child_until(deadline)
    }

    pub fn wait_exact_child(&mut self) -> Result<SubordinateProcessExit, String> {
        let status = self
            .child
            .as_mut()
            .expect("ordinary subordinate owns exact child")
            .wait()
            .map_err(|error| format!("wait for subordinate process: {error}"))?;
        self.reaped = true;
        Ok(SubordinateProcessExit {
            success: status.success(),
            code: status.code(),
        })
    }

    /// Consume this exact child into unresolved terminal root-failure custody.
    ///
    /// This infallible transfer neither signals nor waits. Ordinary Drop loses
    /// its child before running, so it cannot enter unbounded kill/wait. Use only
    /// when bounded local cleanup could not complete and the enclosing retained
    /// root owner will refuse and exit under its process-scope supervisor.
    /// Already-taken IO/drain/task owners must remain in that same invocation.
    /// This creates no cleanup budget and no evidence of child/descendant death.
    pub fn into_root_terminal_failure(mut self) -> RootTerminalSubordinateFailure {
        RootTerminalSubordinateFailure {
            child: self.child.take(),
            _stdin: self.stdin.take(),
            _stdout: self.stdout.take(),
            _stderr: self.stderr.take(),
            reaped: self.reaped,
        }
    }

    fn terminate_and_reap(&mut self) {
        if self.reaped {
            return;
        }
        let Some(child) = self.child.as_mut() else {
            // Only consuming terminal handoff removes the exact child.
            return;
        };
        let _ = child.kill();
        let _ = child.wait();
        self.reaped = true;
    }
}

impl Drop for SubordinateProcess {
    fn drop(&mut self) {
        self.terminate_and_reap();
    }
}

#[cfg(all(test, unix))]
mod terminal_failure_tests {
    use super::*;
    use std::os::fd::AsRawFd;

    // Authored ownership regression cases. They do not attest enclosing scope
    // settlement or prove a deadline bound; execution is separately authorized.
    fn child_with_pipes() -> SubordinateProcess {
        SubordinateProcess::spawn(SubordinateProcessRequest {
            cmd: "/bin/sh".to_owned(),
            argv0: None,
            args: vec!["-c".to_owned(), "exit 0".to_owned()],
            cwd: "/".to_owned(),
            envs: Vec::new(),
            limits: None,
            inherited_fds: Vec::new(),
        })
        .expect("spawn test child")
    }

    #[test]
    fn spawn_is_created_only_under_the_fork_window() {
        // An unguarded spawn ignores descriptor leases; this one refuses a
        // caller that holds one, and waits for one held elsewhere.
        let lease = crate::exec::try_retain_fork_sensitive_descriptors().unwrap();
        match SubordinateProcess::spawn(SubordinateProcessRequest {
            cmd: "/bin/true".to_owned(),
            argv0: None,
            args: Vec::new(),
            cwd: "/".to_owned(),
            envs: Vec::new(),
            limits: None,
            inherited_fds: Vec::new(),
        }) {
            Ok(_) => panic!("spawned while this thread held a descriptor lease"),
            Err(error) => {
                assert!(error.contains("retains fork-sensitive descriptor authority"), "{error}");
            }
        }
        drop(lease);
        let hold = std::time::Duration::from_millis(300);
        let (held, observed) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let lease = crate::exec::try_retain_fork_sensitive_descriptors().unwrap();
            held.send(()).unwrap();
            std::thread::sleep(hold);
            drop(lease);
        });
        observed.recv().unwrap();
        let started = std::time::Instant::now();
        let mut process = child_with_pipes();
        let waited = started.elapsed();
        holder.join().unwrap();
        assert!(waited >= hold - std::time::Duration::from_millis(50), "{waited:?}");
        let exit = process
            .wait_exact_child_until(MonotonicDeadline::after(std::time::Duration::from_secs(5)))
            .unwrap()
            .expect("test child exits");
        assert!(exit.success);
        // The window reopened once the child exec'd.
        drop(crate::exec::try_retain_fork_sensitive_descriptors().unwrap());
    }

    #[test]
    fn terminal_handoff_preserves_exact_child_and_retained_pipe_descriptors() {
        let process = child_with_pipes();
        let pid = process.child.as_ref().unwrap().id();
        let input = process.stdin.as_ref().unwrap().input.as_raw_fd();
        let output = process.stdout.as_ref().unwrap().output.as_raw_fd();
        let error = process.stderr.as_ref().unwrap().output.as_raw_fd();
        let mut terminal = process.into_root_terminal_failure();
        // Settle test-only child explicitly before assertions. Production
        // terminal owner intentionally exposes no observe/wait/recovery API.
        let child = terminal.child.as_mut().unwrap();
        let transferred_pid = child.id();
        child.wait().expect("reap test child");
        terminal.reaped = true;
        assert_eq!(transferred_pid, pid);
        assert_eq!(terminal._stdin.as_ref().unwrap().input.as_raw_fd(), input);
        assert_eq!(terminal._stdout.as_ref().unwrap().output.as_raw_fd(), output);
        assert_eq!(terminal._stderr.as_ref().unwrap().output.as_raw_fd(), error);
    }

    #[test]
    fn terminal_handoff_does_not_reacquire_previously_taken_output() {
        let mut process = child_with_pipes();
        let output = process.take_output().expect("take exact output owner");
        let descriptor = output.output.as_raw_fd();
        let mut terminal = process.into_root_terminal_failure();
        terminal.child.as_mut().unwrap().wait().expect("reap test child");
        terminal.reaped = true;
        assert!(terminal._stdout.is_none());
        assert_eq!(output.output.as_raw_fd(), descriptor);
        // The invocation, not terminal child ownership, retains taken IO.
        drop(output);
        drop(terminal);
    }
}

impl SubordinateProcessInput {
    /// Write and flush one complete frame under one absolute deadline. Partial
    /// writes never restart the clock.
    pub fn write_all_until(
        &mut self,
        bytes: &[u8],
        deadline: MonotonicDeadline,
    ) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            let descriptor = self.input.as_raw_fd();
            let mut written = 0;
            while written < bytes.len() {
                crate::exec::duplex_deadline::wait_ready(descriptor, libc::POLLOUT, deadline)?;
                match self.input.write(&bytes[written..]) {
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "subordinate process input accepted no bytes",
                        ));
                    }
                    Ok(count) => written += count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (bytes, deadline);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "deadline-bound subordinate process input is unavailable",
            ))
        }
    }
}

#[cfg(unix)]
fn configure_input_nonblocking(input: &mut ChildStdin) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let descriptor = input.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn configure_input_nonblocking(_input: &mut ChildStdin) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "nonblocking subordinate process input is unavailable",
    ))
}

impl Read for SubordinateProcessOutput {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.output.read(bytes)
    }
}

impl SubordinateProcessOutput {
    /// Read one delimiter-terminated frame under one absolute deadline. The
    /// delimiter counts toward `maximum_bytes`. EOF returns the bytes already
    /// observed; an empty result is clean EOF. An over-limit frame consumes one
    /// extra byte to distinguish exact-length EOF, then returns InvalidData.
    /// Errors do not establish a new framing boundary; the protocol owner must
    /// not resume parsing as though a complete frame had been returned.
    pub fn read_frame_until(
        &mut self,
        delimiter: u8,
        maximum_bytes: usize,
        deadline: MonotonicDeadline,
    ) -> std::io::Result<Vec<u8>> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            let descriptor = self.output.as_raw_fd();
            let mut frame = Vec::new();
            loop {
                crate::exec::duplex_deadline::wait_ready(descriptor, libc::POLLIN, deadline)?;
                let mut byte = [0_u8; 1];
                match self.output.read(&mut byte) {
                    Ok(0) => return Ok(frame),
                    Ok(_) => {
                        if frame.len() == maximum_bytes {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "subordinate process frame exceeds its byte bound",
                            ));
                        }
                        frame.push(byte[0]);
                        if byte[0] == delimiter {
                            return Ok(frame);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (delimiter, maximum_bytes, deadline);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "deadline-bound subordinate process output is unavailable",
            ))
        }
    }
}

impl Read for SubordinateProcessError {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.output.read(bytes)
    }
}

impl SubordinateProcessError {
    /// Start draining immediately with no retained diagnostic bytes. The sole
    /// reader checks cancellation at least between 50ms absolute-deadline
    /// slices, including when the producer writes continuously. Task startup
    /// failure closes this pipe and returns an error; it never implies child
    /// cleanup. Retain the returned owner during partial process startup too.
    pub fn start_discarding(mut self) -> std::io::Result<SubordinateDiagnosticDrain> {
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let task = crate::task::spawn_host_task("lillux-subordinate-diagnostics", move || {
            loop {
                if reader_stop.load(Ordering::Acquire) {
                    return SubordinateDiagnosticDrainEnd::Cancelled;
                }
                let deadline = MonotonicDeadline::after(crate::time::Duration::from_millis(50));
                match self.drain_until(0, deadline) {
                    Ok(capture) if capture.eof => return SubordinateDiagnosticDrainEnd::Eof,
                    Ok(_) => {}
                    Err(error) => return SubordinateDiagnosticDrainEnd::ReadFailed(error.kind()),
                }
            }
        })?;
        Ok(SubordinateDiagnosticDrain {
            stop,
            task: Some(task),
            end: None,
        })
    }

    /// Drain until EOF or the supplied deadline, retaining at most the given
    /// byte count. Excess output is discarded while draining continues, so the
    /// retention limit does not close the producer's pipe or induce backpressure.
    /// The deadline is never renewed by partial output. Timeout retains the pipe
    /// owner and returns partial diagnostics rather than pretending to see EOF.
    pub fn drain_until(
        &mut self,
        maximum_bytes: usize,
        deadline: MonotonicDeadline,
    ) -> std::io::Result<SubordinateProcessDiagnostics> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            let mut capture = SubordinateProcessDiagnostics {
                bytes: Vec::new(),
                truncated: false,
                eof: false,
            };
            let mut buffer = [0_u8; 4096];
            loop {
                match crate::exec::duplex_deadline::wait_ready(
                    self.output.as_raw_fd(),
                    libc::POLLIN,
                    deadline,
                ) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                        return Ok(capture);
                    }
                    Err(error) => return Err(error),
                }
                match self.output.read(&mut buffer) {
                    Ok(0) => {
                        capture.eof = true;
                        return Ok(capture);
                    }
                    Ok(count) => {
                        let retain = count.min(maximum_bytes.saturating_sub(capture.bytes.len()));
                        capture.bytes.extend_from_slice(&buffer[..retain]);
                        capture.truncated |= retain < count;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (maximum_bytes, deadline);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "deadline-bound subordinate diagnostic drain is unavailable",
            ))
        }
    }
}
