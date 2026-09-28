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
    child: Child,
    stdin: Option<SubordinateProcessInput>,
    stdout: Option<SubordinateProcessOutput>,
    stderr: Option<SubordinateProcessError>,
    reaped: bool,
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

impl SubordinateProcess {
    pub fn spawn(request: SubordinateProcessRequest) -> Result<Self, String> {
        if request.cmd.is_empty() || request.cwd.is_empty() {
            return Err("subordinate process command and cwd must be nonempty".to_owned());
        }
        let mut command = Command::new(&request.cmd);
        command.current_dir(&request.cwd);
        Self::spawn_configured(
            command,
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
        request
            .cwd
            .configure_command_cwd(&mut command)
            .map_err(|error| error.to_string())?;
        request.inherited_fds.push(executable);
        Self::spawn_configured(
            command,
            request.argv0,
            request.args,
            request.envs,
            request.limits,
            request.inherited_fds,
        )
    }

    fn spawn_configured(
        mut command: Command,
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

        let mut child = command
            .spawn()
            .map_err(|error| format!("spawn subordinate process: {error}"))?;
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
            child,
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
        CooperativeChildTermination::for_child(&self.child)
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
            .kill()
            .map_err(|error| format!("kill exact subordinate child: {error}"))?;
        self.wait_exact_child_until(deadline)
    }

    pub fn wait_exact_child(&mut self) -> Result<SubordinateProcessExit, String> {
        let status = self
            .child
            .wait()
            .map_err(|error| format!("wait for subordinate process: {error}"))?;
        self.reaped = true;
        Ok(SubordinateProcessExit {
            success: status.success(),
            code: status.code(),
        })
    }

    fn terminate_and_reap(&mut self) {
        if self.reaped {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
    }
}

impl Drop for SubordinateProcess {
    fn drop(&mut self) {
        self.terminate_and_reap();
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
