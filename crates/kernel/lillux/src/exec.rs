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

mod duplex_deadline;
pub use duplex_deadline::DeadlineDuplexStream;

#[cfg(target_os = "linux")]
mod descriptor_transfer;
#[cfg(target_os = "linux")]
pub use descriptor_transfer::{
    DescriptorTransferBounds, InheritedDescriptorTransferChildAuthority,
    InheritedDescriptorTransferReceiver, InheritedDescriptorTransferSender,
    ReceivedDescriptorAuthority, ReceivedDescriptorTransfer, inherited_descriptor_transfer_pair,
    take_inherited_descriptor_transfer_sender,
};

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
    pub inherited_fds: Vec<InheritedDescriptorAuthority>,
    /// Exact child-descriptor mappings. Sources remain CLOEXEC in the parent;
    /// Lillux reserves free destinations before fork and installs every
    /// mapping only in the trusted pre-exec boundary.
    pub inherited_fd_mappings: Vec<InheritedDescriptorMapping>,
    /// Optional trusted launcher status channel. When present, Lillux waits for
    /// the launcher to report the host PID of its target and supervises that
    /// target's process group in addition to the outer launcher process.
    pub supervised_status: Option<SupervisedProcessStatus>,
}

/// One exact already-open descriptor mapped to a distinct child coordinate.
/// Construction remains inside typed Lillux channel authority.
#[derive(Clone)]
pub struct InheritedDescriptorMapping {
    source: InheritedDescriptorAuthority,
    target_fd: u32,
}

impl InheritedDescriptorMapping {
    fn source_descriptor(&self) -> Result<u32, String> {
        #[cfg(unix)]
        {
            self.source.inherited_descriptor()
        }
        #[cfg(not(unix))]
        {
            Err("mapped inherited descriptors are unavailable on this platform".to_owned())
        }
    }
}

/// Resource limits applied to a spawned subprocess.
///
/// Limits are fail-closed: a configured limit that is unsupported, invalid,
/// or cannot be installed prevents the subprocess from spawning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubprocessLimits {
    /// Maximum number of file descriptors the subprocess may open.
    pub max_open_files: Option<u64>,
    /// Maximum virtual address space inherited by the subprocess tree.
    pub max_address_space_bytes: Option<u64>,
    /// Maximum CPU seconds for each process in the subprocess tree.
    pub max_cpu_seconds: Option<u64>,
    /// Maximum processes/threads available to the subprocess OS account.
    pub max_processes: Option<u64>,
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
    state: SupervisedProcessStatusState,
}

enum SupervisedProcessStatusState {
    Run {
        reader: InheritedDescriptorAuthority,
    },
    AwaitingAttachment {
        reader: InheritedDescriptorAuthority,
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
    writer: Option<PendingForkControlDescriptor>,
}

/// Both ends needed to connect Lillux supervision to a trusted launcher.
pub struct SupervisedLauncherStatusPipe {
    pub reader: SupervisedProcessStatus,
    pub writer: InheritedDescriptorAuthority,
}

/// Exact status and release authorities for a supervised target that must
/// remain blocked until durable process attachment.
pub struct SupervisedLauncherAttachmentStatusPipe {
    pub reader: SupervisedProcessStatus,
    pub writer: InheritedDescriptorAuthority,
    /// Read end inherited by the trusted launcher and bound to its final
    /// target-exec boundary.
    pub attachment_release_reader: InheritedDescriptorAuthority,
    /// Child-side duplicate of the release writer. The trusted launcher keeps
    /// it open while blocked so parent death cannot turn pipe EOF into a
    /// release.
    pub attachment_release_keepalive_writer: InheritedDescriptorAuthority,
}

impl SupervisedLauncherStatusPipe {
    /// Validated numeric coordinate committed to the trusted launch protocol.
    /// This is not a raw handle or ownership transfer.
    pub fn writer_descriptor(&self) -> Result<u32, String> {
        self.writer.inherited_descriptor()
    }
}

impl SupervisedLauncherAttachmentStatusPipe {
    /// Validated numeric coordinate committed to the trusted launch protocol.
    /// This is not a raw handle or ownership transfer.
    pub fn writer_descriptor(&self) -> Result<u32, String> {
        self.writer.inherited_descriptor()
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

    let lease = retain_fork_sensitive_descriptors();
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
        reader: SupervisedProcessStatus {
            state: SupervisedProcessStatusState::Run {
                reader: InheritedDescriptorAuthority::from_owned_file(reader, &lease)?,
            },
        },
        writer: InheritedDescriptorAuthority::from_owned_file(writer, &lease)?,
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
pub fn supervised_launcher_attachment_status_pipe()
-> Result<SupervisedLauncherAttachmentStatusPipe, String> {
    use std::os::fd::FromRawFd as _;

    let lease = retain_fork_sensitive_descriptors();
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
    let gate_keepalive_writer = InheritedDescriptorAuthority::from_owned_file(
        gate_writer.try_clone().map_err(|error| {
            format!("duplicate supervised-launcher attachment keepalive: {error}")
        })?,
        &lease,
    )?;
    Ok(SupervisedLauncherAttachmentStatusPipe {
        reader: SupervisedProcessStatus {
            state: SupervisedProcessStatusState::AwaitingAttachment {
                reader: InheritedDescriptorAuthority::from_owned_file(status_reader, &lease)?,
                attachment_release: ProcessAttachmentRelease {
                    writer: Some(register_pending_fork_control_file(gate_writer)),
                },
            },
        },
        writer: InheritedDescriptorAuthority::from_owned_file(status_writer, &lease)?,
        attachment_release_reader: InheritedDescriptorAuthority::from_owned_file(
            gate_reader,
            &lease,
        )?,
        attachment_release_keepalive_writer: gate_keepalive_writer,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn supervised_launcher_attachment_status_pipe()
-> Result<SupervisedLauncherAttachmentStatusPipe, String> {
    Err("supervised-launcher attachment boundaries are supported only on Linux".to_string())
}

/// Create an immutable, rewound anonymous file for descriptor-backed protocol
/// data.
///
/// The returned descriptor is always above stdio, retains `FD_CLOEXEC`, and
/// carries all four write-prevention seals. Callers explicitly inherit it only
/// for the child exec that consumes the data.
#[cfg(target_os = "linux")]
pub fn sealed_memfd(
    name: &std::ffi::CStr,
    bytes: &[u8],
) -> Result<InheritedDescriptorAuthority, String> {
    sealed_memfd_with_flags(
        name,
        bytes,
        libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        None,
    )
}

/// Create a sealed anonymous file that the supported Linux kernel may execute.
/// This is separate from protocol-data memfds so hardened `memfd_noexec`
/// policies cannot silently turn an exact executable capture into a noexec fd.
#[cfg(target_os = "linux")]
pub fn sealed_executable_memfd(
    name: &std::ffi::CStr,
    bytes: &[u8],
) -> Result<InheritedDescriptorAuthority, String> {
    // MFD_EXEC was added in Linux 6.3; RyeOS requires Linux 6.9 or newer.
    const MFD_EXEC: libc::c_uint = 0x0010;
    sealed_memfd_with_flags(
        name,
        bytes,
        libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | MFD_EXEC,
        Some(0o500),
    )
}

#[cfg(target_os = "linux")]
fn sealed_memfd_with_flags(
    name: &std::ffi::CStr,
    bytes: &[u8],
    flags: libc::c_uint,
    mode: Option<libc::mode_t>,
) -> Result<InheritedDescriptorAuthority, String> {
    use std::io::Seek as _;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let lease = retain_fork_sensitive_descriptors();
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
    if let Some(mode) = mode
        && unsafe { libc::fchmod(file.as_raw_fd(), mode) } < 0
    {
        return Err(format!(
            "restrict sealed executable memfd permissions: {}",
            std::io::Error::last_os_error()
        ));
    }
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

    InheritedDescriptorAuthority::from_owned_file(file, &lease)
}

#[cfg(not(target_os = "linux"))]
pub fn sealed_memfd(
    _name: &std::ffi::CStr,
    _bytes: &[u8],
) -> Result<InheritedDescriptorAuthority, String> {
    Err("sealed memfd is supported only on Linux".to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn sealed_executable_memfd(
    _name: &std::ffi::CStr,
    _bytes: &[u8],
) -> Result<InheritedDescriptorAuthority, String> {
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
    /// Exact held-launch cleanup proved by the existing process owner before
    /// returning a spawn failure. Absence grants no cleanup authority: neither
    /// a refusal diagnostic nor a missing target PID is a death certificate.
    /// This is in-memory testimony, never inferred during history replay.
    pub aborted_before_attachment: Option<AbortedProcess>,
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
    #[serde(rename = "cgroup-namespace")]
    _cgroup_namespace: Option<u64>,
    #[serde(rename = "ipc-namespace")]
    _ipc_namespace: Option<u64>,
    #[serde(rename = "mnt-namespace")]
    _mount_namespace: Option<u64>,
    #[serde(rename = "net-namespace")]
    _network_namespace: Option<u64>,
    #[serde(rename = "pid-namespace")]
    _pid_namespace: Option<u64>,
    #[serde(rename = "uts-namespace")]
    _uts_namespace: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherRefusalDocument {
    refused: Box<serde_json::value::RawValue>,
}

/// Validate retained descriptors and make them inheritable only inside this
/// command's forked child. The command's pre-exec closure owns cloned handles,
/// so the exact descriptors remain live until the command is spawned or
/// discarded.
pub fn configure_inherited_fds(
    command: &mut process::Command,
    inherited_fds: &[InheritedDescriptorAuthority],
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

        let mut retained = Vec::with_capacity(inherited_fds.len());
        for file in inherited_fds {
            let fd = file.file().as_raw_fd();
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
            retained.push(file.clone());
        }
        unsafe {
            command.pre_exec(move || {
                for file in &retained {
                    let fd = file.file().as_raw_fd();
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(())
    }
}

/// Convert one already-open CLOEXEC descriptor into the stable pathname that
/// a Linux child can use after `configure_inherited_fds` makes that exact
/// descriptor inheritable in the forked child. No ambient pathname is
/// reopened. The returned handle must be retained through `Command::spawn`.
#[derive(Debug, Clone)]
pub struct InheritedDescriptorAuthority {
    path: std::path::PathBuf,
    #[cfg(unix)]
    handle: Arc<ForkChildCloseFile>,
}

/// One immutable document retained across an exact controller credential
/// transition. The privileged opener snapshots a protected administrator file
/// into a sealed anonymous descriptor; the unprivileged child receives only
/// that descriptor and cannot substitute or mutate its contents.
#[derive(Debug)]
pub struct InheritedReadonlyDocument {
    authority: InheritedDescriptorAuthority,
}

impl InheritedReadonlyDocument {
    /// Snapshot an already pinned administrator document into immutable
    /// inherited launch authority. The containing namespace must have been
    /// protected by the caller's descriptor-rooted traversal before this
    /// conversion. The sealed snapshot also prevents an administrator update
    /// racing the parent/child interpretations of one launch.
    pub fn from_administrator_file(
        file: &crate::PinnedRegularFile,
        maximum_bytes: u64,
    ) -> anyhow::Result<Self> {
        file.require_owner(0)?;
        let observation = file.observation()?;
        let bytes = file.read_stable_bounded(&observation, maximum_bytes)?;
        Ok(Self {
            authority: sealed_memfd(c"lillux-protected-document", &bytes)
                .map_err(anyhow::Error::msg)?,
        })
    }

    /// Read the exact retained document while proving it stayed unchanged.
    pub fn read_stable_bounded(&self, maximum_bytes: u64) -> anyhow::Result<Vec<u8>> {
        let (bytes, _) = self
            .authority
            .read_regular_file_stable_bounded(maximum_bytes)?;
        Ok(bytes)
    }

    /// Retain this exact document through one child exec and publish only its
    /// numeric coordinate in the named environment slot. The environment is
    /// transport, not authority: the descriptor and its root-owned metadata
    /// are validated again by the child.
    pub fn bind_to_command(
        self,
        command: &mut process::Command,
        descriptor_env_name: &str,
    ) -> Result<(), String> {
        if descriptor_env_name.is_empty() || descriptor_env_name.contains(['=', '\0']) {
            return Err("inherited document environment name is invalid".to_owned());
        }
        let descriptor = self.authority.inherited_descriptor()?;
        configure_inherited_fds(command, std::slice::from_ref(&self.authority))?;
        command.env(descriptor_env_name, descriptor.to_string());
        Ok(())
    }

    /// Adopt the unique descriptor installed by the trusted parent launch.
    /// Absence is distinct from a malformed coordinate. The variable is
    /// consumed before returning so unrelated descendants cannot mistake it
    /// for newly granted authority.
    pub fn take_from_environment(descriptor_env_name: &str) -> Result<Option<Self>, String> {
        if descriptor_env_name.is_empty() || descriptor_env_name.contains(['=', '\0']) {
            return Err("inherited document environment name is invalid".to_owned());
        }
        let Some(raw) = std::env::var_os(descriptor_env_name) else {
            return Ok(None);
        };
        // SAFETY: daemon bootstrap is single-threaded before any runtime or
        // application thread exists. This consumes launch transport state.
        unsafe { std::env::remove_var(descriptor_env_name) };
        let raw = raw
            .to_str()
            .ok_or("inherited document descriptor coordinate is not UTF-8")?;
        let descriptor: i32 = raw
            .parse()
            .map_err(|_| "inherited document descriptor coordinate is invalid")?;
        if descriptor <= libc::STDERR_FILENO {
            return Err("inherited document descriptor overlaps standard I/O".to_owned());
        }
        #[cfg(unix)]
        {
            use std::os::fd::FromRawFd as _;
            let lease = retain_fork_sensitive_descriptors();
            // Duplicate before constructing an owned File. An inherited raw
            // coordinate can be repeated in hostile process environment; the
            // successful duplicate gives this call unique ownership and
            // closing the transport coordinate makes any repeated adoption
            // fail without creating aliased File owners.
            let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
            if duplicate < 0 {
                return Err(format!(
                    "adopt inherited document descriptor: {}",
                    std::io::Error::last_os_error()
                ));
            }
            unsafe {
                libc::close(descriptor);
            }
            // SAFETY: F_DUPFD_CLOEXEC returned this uniquely owned descriptor.
            let file = unsafe { std::fs::File::from_raw_fd(duplicate) };
            let authority = InheritedDescriptorAuthority::from_owned_file(file, &lease)?;
            let document = Self { authority };
            document
                .authority
                .regular_file_observation()
                .map_err(|error| error.to_string())?;
            let identity = document
                .authority
                .file_identity()
                .map_err(|error| error.to_string())?;
            if identity.owner() != 0 {
                return Err("inherited document was not created by the administrator".to_owned());
            }
            let required_seals =
                libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
            let observed_seals =
                unsafe { libc::fcntl(document.authority.file().as_raw_fd(), libc::F_GET_SEALS) };
            if observed_seals < 0 || observed_seals & required_seals != required_seals {
                return Err("inherited document is not sealed against mutation".to_owned());
            }
            Ok(Some(document))
        }
        #[cfg(not(unix))]
        {
            let _ = descriptor;
            Err("inherited documents are unavailable on this platform".to_owned())
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod inherited_readonly_document_tests {
    use super::*;
    use std::io::Seek as _;
    use std::os::fd::IntoRawFd as _;

    fn document_memfd(bytes: &[u8], seal: bool) -> i32 {
        let fd = unsafe {
            libc::memfd_create(
                c"lillux-inherited-document-test".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(fd >= 0);
        // SAFETY: memfd_create returned this uniquely owned descriptor.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(bytes).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        if seal {
            let seals =
                libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) }, 0);
        }
        file.into_raw_fd()
    }

    #[test]
    fn inherited_document_refuses_stdio_and_malformed_coordinates() {
        for value in ["not-a-descriptor", "0", "1", "2"] {
            let name = format!("LILLUX_TEST_INHERITED_DOCUMENT_{}", std::process::id());
            // SAFETY: this test uses a process-unique name and consumes it in
            // the same thread before returning.
            unsafe { std::env::set_var(&name, value) };
            assert!(InheritedReadonlyDocument::take_from_environment(&name).is_err());
            assert!(std::env::var_os(&name).is_none());
        }
    }

    #[test]
    fn inherited_document_refuses_non_regular_authority() {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: pipe2 returned two uniquely owned descriptors.
        let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
        let _writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
        let coordinate = reader.into_raw_fd();
        let name = format!("LILLUX_TEST_INHERITED_DOCUMENT_PIPE_{}", std::process::id());
        // SAFETY: this test uses a process-unique name and transfers the exact
        // raw descriptor to the adoption method.
        unsafe { std::env::set_var(&name, coordinate.to_string()) };
        assert!(InheritedReadonlyDocument::take_from_environment(&name).is_err());
        assert!(std::env::var_os(&name).is_none());
    }

    #[test]
    fn inherited_document_requires_seals_and_uniquely_consumes_coordinate() {
        let name = format!(
            "LILLUX_TEST_INHERITED_DOCUMENT_SEALS_{}",
            std::process::id()
        );
        let unsealed = document_memfd(b"mutable", false);
        // SAFETY: this test uses a process-unique name and transfers the exact
        // raw descriptor to the adoption method.
        unsafe { std::env::set_var(&name, unsealed.to_string()) };
        assert!(InheritedReadonlyDocument::take_from_environment(&name).is_err());

        let sealed = document_memfd(b"immutable", true);
        // SAFETY: same process-local transport contract as above.
        unsafe { std::env::set_var(&name, sealed.to_string()) };
        let adopted = InheritedReadonlyDocument::take_from_environment(&name);
        if unsafe { libc::geteuid() } == 0 {
            let document = adopted.unwrap().unwrap();
            assert_eq!(document.read_stable_bounded(32).unwrap(), b"immutable");
        } else {
            assert!(adopted.is_err());
        }

        // Repeating the consumed coordinate cannot manufacture a second File
        // owner or revive the inherited authority.
        unsafe { std::env::set_var(&name, sealed.to_string()) };
        assert!(InheritedReadonlyDocument::take_from_environment(&name).is_err());
        assert!(std::env::var_os(&name).is_none());
    }
}

/// Return the validated numeric coordinate for an exact, CLOEXEC-protected
/// descriptor that will be retained by a typed Lillux launch request.
pub fn inherited_descriptor_coordinate(file: &std::fs::File) -> Result<u32, String> {
    #[cfg(not(unix))]
    {
        let _ = file;
        Err("inherited descriptor coordinates are unavailable on this platform".to_owned())
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;

        protect_descriptor_from_exec(file)?;
        let descriptor = file.as_raw_fd();
        if descriptor <= libc::STDERR_FILENO {
            return Err(format!("inherited descriptor {descriptor} overlaps stdio"));
        }
        u32::try_from(descriptor)
            .map_err(|_| "inherited descriptor exceeds the protocol coordinate range".to_owned())
    }
}

/// Return the Linux descriptor-rooted pathname for one exact inherited file.
/// The caller must retain the same file through the child launch.
pub fn inherited_descriptor_path_for(file: &std::fs::File) -> Result<std::path::PathBuf, String> {
    let descriptor = inherited_descriptor_coordinate(file)?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = descriptor;
        Err("descriptor-rooted inherited paths are unavailable on this platform".to_owned())
    }
    #[cfg(target_os = "linux")]
    {
        Ok(std::path::PathBuf::from(format!(
            "/proc/self/fd/{descriptor}"
        )))
    }
}

impl InheritedDescriptorAuthority {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn inherited_descriptor(&self) -> Result<u32, String> {
        #[cfg(unix)]
        {
            inherited_descriptor_coordinate(self.file())
        }
        #[cfg(not(unix))]
        {
            Err("inherited descriptors are unavailable on this platform".to_owned())
        }
    }

    pub fn retain_for_child(&self, inherited_fds: &mut Vec<Self>) {
        inherited_fds.push(self.clone());
    }

    /// Physically close this registered descriptor only when it has no other
    /// strong owner and the fork barrier can be leased before `deadline`.
    /// Failure returns the unchanged owner. This proves this descriptor's
    /// close, NOT the death of mapped/SCM_RIGHTS copies or opened descendants;
    /// callers must separately prove those process and borrower lifetimes.
    pub fn try_close_last_owner(
        self,
        deadline: crate::time::MonotonicDeadline,
    ) -> Result<(), (Self, std::io::Error)> {
        #[cfg(not(unix))]
        {
            return Err((
                self,
                std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "inherited descriptors are unavailable",
                ),
            ));
        }
        #[cfg(unix)]
        {
            let lease = match retain_fork_sensitive_descriptors_until(deadline) {
                Ok(lease) => lease,
                Err(error) => return Err((self, error)),
            };
            let Self { path, handle } = self;
            match Arc::try_unwrap(handle) {
                Ok(handle) => {
                    // The shared lease rules out deferred close. Do not replace
                    // this with a bare Arc uniqueness check followed by Drop.
                    drop(handle);
                    drop(lease);
                    Ok(())
                }
                Err(handle) => Err((
                    Self { path, handle },
                    std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "inherited descriptor still has another owner",
                    ),
                )),
            }
        }
    }

    #[cfg(unix)]
    pub fn file_identity(&self) -> anyhow::Result<crate::secure_fs::OpenFileIdentity> {
        crate::secure_fs::observe_open_file_identity(self.file())
    }

    #[cfg(unix)]
    pub fn regular_file_observation(
        &self,
    ) -> anyhow::Result<crate::secure_fs::OpenRegularFileObservation> {
        crate::secure_fs::observe_open_regular_file(self.file())
    }

    #[cfg(unix)]
    pub fn digest_regular_file_stable_exact(
        &self,
        observation: &crate::secure_fs::OpenRegularFileObservation,
    ) -> anyhow::Result<String> {
        crate::secure_fs::ensure_open_regular_file_unchanged(self.file(), observation)?;
        let (digest, _) = crate::secure_fs::digest_open_regular_file_stable_exact(
            self.file(),
            observation.size(),
        )?;
        crate::secure_fs::ensure_open_regular_file_unchanged(self.file(), observation)?;
        Ok(digest)
    }

    #[cfg(unix)]
    pub fn mount_entry_kind(&self) -> anyhow::Result<crate::secure_fs::OpenMountEntryKind> {
        crate::secure_fs::open_mount_entry_kind(self.file())
    }

    #[cfg(unix)]
    pub fn same_file_identity(&self, other: &Self) -> anyhow::Result<bool> {
        crate::secure_fs::same_open_file_identity(self.file(), other.file())
    }

    #[cfg(unix)]
    pub fn directory_identity(&self) -> anyhow::Result<crate::secure_fs::PinnedDirectoryIdentity> {
        let lease = retain_fork_sensitive_descriptors();
        let root = crate::secure_fs::PinnedDirectory::from_open_directory(
            self.path.clone(),
            self.file().try_clone()?,
        )?;
        let identity = root.identity();
        drop(root);
        drop(lease);
        identity
    }

    /// Descriptor-relative traversal stays entirely inside Lillux's short
    /// fork lease; no temporary directory or member File escapes unregistered.
    #[cfg(unix)]
    pub fn open_regular_descendant(
        &self,
        relative: &std::path::Path,
    ) -> anyhow::Result<Option<Self>> {
        let lease = retain_fork_sensitive_descriptors();
        let root = crate::secure_fs::PinnedDirectory::from_open_directory(
            self.path.clone(),
            self.file().try_clone()?,
        )?;
        let member = root.open_pinned_regular_descendant(relative, false)?;
        let result = member
            .map(|member| member.into_inherited_descriptor_path())
            .transpose();
        drop(root);
        drop(lease);
        result
    }

    /// Open or create one bounded, canonical directory descendant from this
    /// exact held root and make the leaf owner-private. No ambient root path
    /// is reopened. All temporary descriptors stay under the short fork
    /// lease, and the returned directory is registered before it is released.
    #[cfg(unix)]
    pub fn open_or_create_private_directory_descendant(
        &self,
        relative: &std::path::Path,
    ) -> anyhow::Result<Self> {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::MetadataExt as _;
        use std::path::Component;

        let bytes = relative.as_os_str().as_bytes();
        if bytes.is_empty()
            || bytes.len() >= libc::PATH_MAX as usize
            || bytes.contains(&0)
            || relative.is_absolute()
        {
            anyhow::bail!(
                "private directory descendant must be bounded canonical relative components"
            );
        }
        let normalized = relative.components().collect::<std::path::PathBuf>();
        if normalized.as_os_str().as_bytes() != bytes
            || relative.components().any(|component| {
                !matches!(component, Component::Normal(name) if name.as_bytes().len() <= 255)
            })
        {
            anyhow::bail!("private directory descendant must be bounded canonical relative components");
        }
        let lease = retain_fork_sensitive_descriptors();
        // A workspace view may be held with O_PATH. Reopen only its exact
        // inode, not its diagnostic pathname, for mkdirat/fsync traversal.
        let fd = unsafe {
            libc::openat(
                self.file().as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let root = unsafe { std::fs::File::from_raw_fd(fd) };
        if !crate::secure_fs::same_open_file_identity(self.file(), &root)? {
            anyhow::bail!("private directory traversal changed its exact held root");
        }
        let mut directory =
            crate::secure_fs::PinnedDirectory::from_open_directory(self.path.clone(), root)?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                unreachable!("relative components validated before mutation");
            };
            directory = directory.open_or_create_child(name, 0o700)?;
        }
        // Do not use the pathname-binding variant: this directory's
        // diagnostic path starts at an intentionally opaque descriptor path.
        directory.set_mode(0o700)?;
        let result = directory.into_inherited_descriptor_path()?;
        let metadata = result.file().metadata()?;
        if !metadata.is_dir() || metadata.mode() & 0o7777 != 0o700 {
            anyhow::bail!("private directory descendant is not exactly owner-private");
        }
        drop(lease);
        Ok(result)
    }

    #[cfg(unix)]
    pub fn set_regular_file_mode(&self, mode: u32) -> anyhow::Result<()> {
        crate::secure_fs::set_open_regular_file_mode(self.file(), mode)
    }

    #[cfg(unix)]
    pub fn require_owned_executable(&self) -> anyhow::Result<crate::secure_fs::OpenFileIdentity> {
        crate::secure_fs::require_effective_user_owned_executable(self.file())
    }

    #[cfg(unix)]
    pub fn require_owned_regular(&self) -> anyhow::Result<crate::secure_fs::OpenFileIdentity> {
        crate::secure_fs::require_effective_user_owned_regular(self.file())
    }

    #[cfg(unix)]
    pub fn read_regular_file_stable_bounded(
        &self,
        max_bytes: u64,
    ) -> anyhow::Result<(Vec<u8>, crate::secure_fs::OpenRegularFileObservation)> {
        let observation = self.regular_file_observation()?;
        let bytes = crate::secure_fs::read_open_regular_file_stable_bounded(
            self.file(),
            &observation,
            max_bytes,
        )?;
        Ok((bytes, observation))
    }

    /// Register a uniquely owned descriptor while the caller retains the lease
    /// acquired BEFORE its creation. Registration is not retrospective: never
    /// open or duplicate first and then acquire a lease to wrap the result.
    #[cfg(unix)]
    pub(crate) fn from_owned_file(
        file: std::fs::File,
        lease: &ForkSensitiveDescriptorLease,
    ) -> Result<Self, String> {
        assert!(lease.retained && lease.owner == thread::current().id());
        // Registered child-close coordinates must never alias Command's final
        // stdio setup. Adoption may legitimately consume a channel at fd 0;
        // relocate that owned endpoint before registering it, not the child's
        // later, unrelated configured stdin.
        let file = move_owned_descriptor_above_stdio(file).map_err(|error| error.to_string())?;
        inherited_descriptor_coordinate(&file)?;
        Self::from_registered_file(Arc::new(register_fork_child_close_file(file)))
    }

    #[cfg(unix)]
    pub(crate) fn from_registered_file(file: Arc<ForkChildCloseFile>) -> Result<Self, String> {
        let path = inherited_descriptor_path_for(&file)?;
        Ok(Self { path, handle: file })
    }

    /// Lillux-internal inspection only. Never let an unregistered `try_clone`
    /// escape this owner; child retention shares its registered strong owner.
    #[cfg(unix)]
    pub(crate) fn file(&self) -> &std::fs::File {
        &self.handle
    }
}

#[cfg(all(test, unix))]
mod inherited_directory_traversal_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    #[cfg(target_os = "linux")]
    #[test]
    fn private_descendant_from_path_descriptor_survives_root_rename() {
        use std::os::unix::fs::OpenOptionsExt as _;
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("source");
        std::fs::create_dir(&original).unwrap();
        let root = {
            let lease = retain_fork_sensitive_descriptors();
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&original)
                .unwrap();
            InheritedDescriptorAuthority::from_owned_file(file, &lease).unwrap()
        };
        std::fs::rename(&original, parent.path().join("retained")).unwrap();
        std::fs::create_dir(&original).unwrap();
        let directory = root
            .open_or_create_private_directory_descendant(Path::new("cache/tool"))
            .unwrap();
        assert!(parent.path().join("retained/cache/tool").is_dir());
        assert!(!original.join("cache").exists());
        assert_eq!(
            directory.file().metadata().unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_ne!(
            unsafe { libc::fcntl(directory.file().as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let repeated = root
            .open_or_create_private_directory_descendant(Path::new("cache/tool"))
            .unwrap();
        assert!(directory.same_file_identity(&repeated).unwrap());
    }

    #[test]
    fn private_descendant_refuses_links_and_tightens_only_exact_leaf() {
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(parent.path().join("existing")).unwrap();
        std::fs::set_permissions(
            parent.path().join("existing"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), parent.path().join("link")).unwrap();
        std::fs::write(parent.path().join("regular"), b"not a directory").unwrap();
        let root = crate::secure_fs::PinnedDirectory::open(parent.path())
            .unwrap()
            .unwrap()
            .into_inherited_descriptor_path()
            .unwrap();
        for path in ["link", "link/escape", "regular", "regular/escape"] {
            assert!(
                root.open_or_create_private_directory_descendant(Path::new(path))
                    .is_err()
            );
        }
        assert!(!outside.path().join("escape").exists());
        let leaf = root
            .open_or_create_private_directory_descendant(Path::new("existing"))
            .unwrap();
        assert_eq!(
            leaf.file().metadata().unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn private_descendant_validates_entire_relative_path_before_creation() {
        let parent = tempfile::tempdir().unwrap();
        let root = crate::secure_fs::PinnedDirectory::open(parent.path())
            .unwrap()
            .unwrap()
            .into_inherited_descriptor_path()
            .unwrap();
        for path in [
            "",
            ".",
            "/absolute",
            "new/../escape",
            "new/./leaf",
            "new//leaf",
            "new/",
            "new/\0leaf",
        ] {
            assert!(
                root.open_or_create_private_directory_descendant(Path::new(path))
                    .is_err(),
                "{path:?}"
            );
        }
        let too_long = format!("new/{}", "x".repeat(libc::PATH_MAX as usize));
        assert!(
            root.open_or_create_private_directory_descendant(Path::new(&too_long))
                .is_err()
        );
        assert!(!parent.path().join("new").exists());
    }
}

// Keep the opaque inspection interface callable at the existing platform
// refusal boundary; unsupported hosts never construct synthetic descriptors,
// metadata, mount identities, or fallback authority.
#[cfg(not(unix))]
impl InheritedDescriptorAuthority {
    pub fn file_identity(&self) -> anyhow::Result<crate::secure_fs::OpenFileIdentity> {
        anyhow::bail!("inherited descriptor inspection is unavailable on this platform")
    }

    pub fn regular_file_observation(
        &self,
    ) -> anyhow::Result<crate::secure_fs::OpenRegularFileObservation> {
        anyhow::bail!("inherited descriptor inspection is unavailable on this platform")
    }

    pub fn digest_regular_file_stable_exact(
        &self,
        _observation: &crate::secure_fs::OpenRegularFileObservation,
    ) -> anyhow::Result<String> {
        anyhow::bail!("inherited descriptor inspection is unavailable on this platform")
    }

    pub fn mount_entry_kind(&self) -> anyhow::Result<crate::secure_fs::OpenMountEntryKind> {
        anyhow::bail!("inherited descriptor inspection is unavailable on this platform")
    }

    pub fn same_file_identity(&self, _other: &Self) -> anyhow::Result<bool> {
        anyhow::bail!("inherited descriptor inspection is unavailable on this platform")
    }

    pub fn directory_identity(&self) -> anyhow::Result<crate::secure_fs::PinnedDirectoryIdentity> {
        anyhow::bail!("inherited descriptor inspection is unavailable on this platform")
    }

    pub fn open_regular_descendant(
        &self,
        _relative: &std::path::Path,
    ) -> anyhow::Result<Option<Self>> {
        anyhow::bail!("inherited descriptor traversal is unavailable on this platform")
    }

    pub fn open_or_create_private_directory_descendant(
        &self,
        _relative: &std::path::Path,
    ) -> anyhow::Result<Self> {
        anyhow::bail!("inherited descriptor traversal is unavailable on this platform")
    }

    pub fn set_regular_file_mode(&self, _mode: u32) -> anyhow::Result<()> {
        anyhow::bail!("inherited descriptor mode control is unavailable on this platform")
    }

    pub fn require_owned_executable(&self) -> anyhow::Result<crate::secure_fs::OpenFileIdentity> {
        anyhow::bail!("inherited descriptor inspection is unavailable on this platform")
    }

    pub fn require_owned_regular(&self) -> anyhow::Result<crate::secure_fs::OpenFileIdentity> {
        anyhow::bail!("inherited descriptor inspection is unavailable on this platform")
    }

    pub fn read_regular_file_stable_bounded(
        &self,
        _max_bytes: u64,
    ) -> anyhow::Result<(Vec<u8>, crate::secure_fs::OpenRegularFileObservation)> {
        anyhow::bail!("inherited descriptor reads are unavailable on this platform")
    }
}

pub(crate) fn inherited_descriptor_path(
    file: std::fs::File,
) -> Result<InheritedDescriptorAuthority, String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        Err("descriptor-rooted inherited paths are unavailable on this platform".to_owned())
    }
    #[cfg(target_os = "linux")]
    {
        let lease = retain_fork_sensitive_descriptors();
        // The consumed pinned source predates this child-inheritance owner.
        // Create its new registered descriptor under the lease and retire the
        // old source before reopening the fork window. Received mount owners
        // use from_registered_file instead: they must never take this path.
        let inherited = file.try_clone().map_err(|error| error.to_string())?;
        drop(file);
        InheritedDescriptorAuthority::from_owned_file(inherited, &lease)
    }
}

/// Configure a command to inherit a set of typed descriptor-path
/// authorities. The raw file handles remain private to Lillux.
pub fn configure_inherited_descriptor_authorities(
    command: &mut process::Command,
    authorities: &[InheritedDescriptorAuthority],
) -> Result<(), String> {
    configure_inherited_fds(command, authorities)
}

/// Ensure a live descriptor is protected from accidental inheritance. The
/// platform flag manipulation remains inside Lillux.
#[cfg(unix)]
pub fn protect_descriptor_from_exec<T: std::os::fd::AsRawFd>(descriptor: &T) -> Result<(), String> {
    let fd = descriptor.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(format!(
            "descriptor {fd} cannot be inspected: {}",
            std::io::Error::last_os_error()
        ));
    }
    if flags & libc::FD_CLOEXEC != 0 {
        return Ok(());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(format!(
            "descriptor {fd} cannot be protected from exec: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Typed connected inherited byte-stream channel. Platform socket and
/// descriptor mechanics remain private to Lillux.
pub struct InheritedDuplexChannel {
    #[cfg(unix)]
    stream: InheritedDescriptorAuthority,
}

#[cfg(unix)]
fn move_owned_descriptor_above_stdio(file: std::fs::File) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    if file.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(file);
    }
    let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: F_DUPFD_CLOEXEC created one newly owned descriptor; closing the
    // original prevents any parent control coordinate aliasing later stdio.
    let moved = unsafe { std::fs::File::from_raw_fd(duplicate) };
    drop(file);
    Ok(moved)
}

/// Command has already installed these explicitly configured child streams.
/// When parent stdio was closed, a CLOEXEC pipe may already occupy its final
/// coordinate, so no dup2/dup3 clears that flag. Preserve only those configured
/// streams across exec; never reopen ambient stdio or apply this to inherited
/// stdio. This runs only in the allocation-free child setup hook.
#[cfg(unix)]
fn preserve_configured_stdio_across_exec() -> std::io::Result<()> {
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Configure three fresh pipes for a directly driven child command.
///
/// Use this for full-duplex callers that own `ChildStdin`/`ChildStdout` rather
/// than Lillux's buffered subprocess runner. Consuming an inherited channel
/// can leave fd 0 closed: Rust may then allocate its CLOEXEC stdin pipe at fd
/// 0 and skip dup2 in the fork/pre-exec path. Reuse the runner's exact child
/// stdio preservation, not ambient `/dev/null` reopening or parent flag edits.
/// Callers must not replace these streams with inherited stdio afterwards.
pub fn configure_command_piped_stdio(command: &mut process::Command) {
    command
        .stdin(process::Stdio::piped())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: Command installs our three fresh streams before this hook;
        // the shared helper uses only allocation-free descriptor syscalls.
        unsafe {
            command.pre_exec(preserve_configured_stdio_across_exec);
        }
    }
}

fn bind_inherited_channel_to_subprocess_request(
    channel: &InheritedDescriptorAuthority,
    request: &mut SubprocessRequest,
    descriptor_env_name: &str,
    target_fd: u32,
) -> Result<(), String> {
    let descriptor = channel.inherited_descriptor()?;
    if target_fd == 1 || target_fd == 2 {
        return Err("inherited duplex target descriptor overlaps stdout or stderr".to_owned());
    }
    if request
        .inherited_fd_mappings
        .iter()
        .any(|mapping| mapping.target_fd == target_fd)
    {
        return Err(format!(
            "subprocess already contains target descriptor mapping {target_fd}"
        ));
    }
    if request
        .inherited_fd_mappings
        .iter()
        .map(InheritedDescriptorMapping::source_descriptor)
        .collect::<Result<Vec<_>, _>>()?
        .contains(&descriptor)
    {
        return Err(format!(
            "subprocess already contains inherited duplex source descriptor {descriptor}"
        ));
    }
    if request
        .envs
        .iter()
        .any(|(name, _)| name == descriptor_env_name)
    {
        return Err(format!(
            "subprocess environment already contains protected descriptor binding {descriptor_env_name}"
        ));
    }
    request
        .envs
        .push((descriptor_env_name.to_owned(), target_fd.to_string()));
    request
        .inherited_fd_mappings
        .push(InheritedDescriptorMapping {
            source: channel.clone(),
            target_fd,
        });
    Ok(())
}

/// Child-side authority for one connected inherited duplex channel.
///
/// The descriptor stays close-on-exec in the parent. Binding this authority to
/// a launch request carries both its hidden source identity and exact target
/// coordinate; each request rejects aliased sources and destinations. Raw
/// descriptor mechanics never leave Lillux.
#[derive(Debug, Clone)]
pub struct InheritedDuplexChannelChildAuthority {
    channel: InheritedDescriptorAuthority,
}

impl InheritedDuplexChannelChildAuthority {
    /// Numeric descriptor committed into an external typed launch protocol.
    /// Lillux retains ownership and validates liveness/CLOEXEC before exposing
    /// the coordinate; callers receive no raw handle or conversion authority.
    pub fn inherited_descriptor(&self) -> Result<u32, String> {
        #[cfg(unix)]
        {
            self.channel.inherited_descriptor()
        }
        #[cfg(not(unix))]
        {
            Err("inherited duplex channels are unavailable on this platform".to_owned())
        }
    }

    /// Retain this exact channel through a Lillux subprocess launch. This is
    /// deliberately narrower than exposing or cloning the underlying file.
    pub fn retain_for_child(&self, inherited_fds: &mut Vec<InheritedDescriptorAuthority>) {
        inherited_fds.push(self.channel.clone());
    }

    /// Bind this exact channel into an existing Lillux request for the direct
    /// (non-adapter) launch path.
    pub fn bind_to_subprocess_request(
        &self,
        request: &mut SubprocessRequest,
        descriptor_env_name: &str,
        target_fd: u32,
    ) -> Result<(), String> {
        bind_inherited_channel_to_subprocess_request(
            &self.channel,
            request,
            descriptor_env_name,
            target_fd,
        )
    }

    /// Consume this authority into one child command. The exact descriptor is
    /// both retained by the command and installed under `descriptor_env_name`;
    /// callers cannot split or replay those two operations.
    pub fn bind_to_command(
        self,
        command: &mut process::Command,
        descriptor_env_name: &str,
    ) -> Result<(), String> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            let descriptor = self.channel.file().as_raw_fd();
            if descriptor <= libc::STDERR_FILENO {
                return Err("inherited duplex channel overlaps standard I/O".to_owned());
            }
            configure_inherited_fds(command, std::slice::from_ref(&self.channel))?;
            command.env(descriptor_env_name, descriptor.to_string());
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = command;
            let _ = descriptor_env_name;
            Err("inherited duplex channels are unavailable on this platform".to_owned())
        }
    }
}

/// Create one connected, atomically close-on-exec duplex channel pair for a
/// parent and one explicitly configured child.
#[cfg(unix)]
pub fn inherited_duplex_channel_pair()
-> Result<(InheritedDuplexChannel, InheritedDuplexChannelChildAuthority), String> {
    use std::os::fd::OwnedFd;

    let lease = retain_fork_sensitive_descriptors();
    let (parent, child) = std::os::unix::net::UnixStream::pair()
        .map_err(|error| format!("create inherited duplex channel: {error}"))?;
    protect_descriptor_from_exec(&parent)?;
    protect_descriptor_from_exec(&child)?;
    Ok((
        InheritedDuplexChannel {
            stream: InheritedDescriptorAuthority::from_owned_file(
                std::fs::File::from(OwnedFd::from(parent)),
                &lease,
            )?,
        },
        InheritedDuplexChannelChildAuthority {
            channel: InheritedDescriptorAuthority::from_owned_file(
                std::fs::File::from(OwnedFd::from(child)),
                &lease,
            )?,
        },
    ))
}

#[cfg(not(unix))]
pub fn inherited_duplex_channel_pair()
-> Result<(InheritedDuplexChannel, InheritedDuplexChannelChildAuthority), String> {
    Err("inherited duplex channels are unavailable on this platform".to_owned())
}

impl InheritedDuplexChannel {
    pub fn with_deadline(
        &mut self,
        deadline: crate::time::MonotonicDeadline,
    ) -> DeadlineDuplexStream<'_> {
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            DeadlineDuplexStream::new(self.stream.file().as_fd(), deadline)
        }
        #[cfg(not(unix))]
        {
            DeadlineDuplexStream::unsupported(deadline)
        }
    }

    /// Wake all aliases blocked in channel I/O without closing a borrowed FD
    /// or waiting for its writer lock. Used by exact channel lifecycle owners.
    pub fn shutdown(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: the registered inherited authority retains this socket.
            if unsafe { libc::shutdown(self.stream.file().as_raw_fd(), libc::SHUT_RDWR) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "duplex shutdown is unavailable",
            ))
        }
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                stream: self.stream.clone(),
            })
        }
        #[cfg(not(unix))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "inherited duplex channels are unavailable on this platform",
            ))
        }
    }

    /// Configure nonblocking byte-stream operation without exposing the
    /// platform socket or descriptor to the protocol owner.
    pub fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let fd = self.stream.file().as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = if nonblocking {
                flags | libc::O_NONBLOCK
            } else {
                flags & !libc::O_NONBLOCK
            };
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = nonblocking;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "inherited duplex channels are unavailable on this platform",
            ))
        }
    }
}

impl Read for InheritedDuplexChannel {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        {
            let count = unsafe {
                libc::recv(
                    self.stream.file().as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if count < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        }
        #[cfg(not(unix))]
        {
            let _ = buffer;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "inherited duplex channels are unavailable on this platform",
            ))
        }
    }
}

impl Write for InheritedDuplexChannel {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        {
            let count = unsafe {
                libc::send(
                    self.stream.file().as_raw_fd(),
                    buffer.as_ptr().cast(),
                    buffer.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if count < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        }
        #[cfg(not(unix))]
        {
            let _ = buffer;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "inherited duplex channels are unavailable on this platform",
            ))
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "inherited duplex channels are unavailable on this platform",
            ))
        }
    }
}

/// Consume one connected duplex descriptor named by the inherited process
/// environment and immediately protect it from further inheritance.
///
/// Parsing, descriptor ownership conversion, and `FD_CLOEXEC` manipulation
/// stay within Lillux. The returned stream is a typed IPC byte channel rather
/// than ambient descriptor authority.
///
/// # Safety
///
/// The typed launch/isolation authority must attest that the descriptor is one
/// end of a connected Unix stream and grant this process unique ownership of
/// it. No other owning Rust handle may exist for the same descriptor. Socket
/// validation happens while minting that authority because the target sandbox
/// deliberately does not admit socket-inspection syscalls.
#[cfg(unix)]
pub unsafe fn take_inherited_duplex_channel_from_env(
    name: &str,
) -> Result<InheritedDuplexChannel, String> {
    let encoded = std::env::var(name)
        .map_err(|error| format!("missing inherited descriptor {name}: {error}"))?;
    // SAFETY: the caller's ownership guarantee applies to the descriptor
    // encoded by this exact inherited environment binding.
    unsafe { take_inherited_duplex_channel(name, &encoded) }
}

#[cfg(not(unix))]
pub unsafe fn take_inherited_duplex_channel_from_env(
    _name: &str,
) -> Result<InheritedDuplexChannel, String> {
    Err("inherited duplex channels are unavailable on this platform".to_owned())
}

#[cfg(unix)]
unsafe fn take_inherited_duplex_channel(
    name: &str,
    encoded: &str,
) -> Result<InheritedDuplexChannel, String> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    if encoded.is_empty() || encoded.len() > 4096 || encoded.chars().any(char::is_control) {
        return Err(format!(
            "inherited descriptor {name} is not canonical and bounded"
        ));
    }
    let descriptor = encoded
        .parse::<std::os::fd::RawFd>()
        .map_err(|error| format!("parse inherited descriptor {name}: {error}"))?;
    if descriptor == libc::STDOUT_FILENO || descriptor == libc::STDERR_FILENO {
        return Err(format!(
            "inherited descriptor {name} overlaps standard output or error"
        ));
    }
    // SAFETY: the caller guarantees unique ownership of this live descriptor.
    // Adopt it before any fallible inspection so every error path closes it.
    let lease = retain_fork_sensitive_descriptors();
    let owned = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let descriptor = owned.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(format!(
            "inherited descriptor {name} cannot be inspected: {}",
            std::io::Error::last_os_error()
        ));
    }
    if flags & libc::FD_CLOEXEC == 0
        && unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(format!(
            "inherited descriptor {name} cannot be protected from exec: {}",
            std::io::Error::last_os_error()
        ));
    }

    Ok(InheritedDuplexChannel {
        stream: InheritedDescriptorAuthority::from_owned_file(std::fs::File::from(owned), &lease)?,
    })
}

#[cfg(all(test, unix))]
mod inherited_unix_stream_tests {
    use super::*;
    use std::os::fd::IntoRawFd as _;
    use std::os::unix::net::UnixStream;

    #[test]
    fn typed_duplex_pair_is_connected_and_close_on_exec() {
        let (mut parent, child) = inherited_duplex_channel_pair().unwrap();
        let descriptor = child.channel.file().as_raw_fd();
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);

        let mut writer = child.channel.file();
        writer.write_all(b"phase\n").unwrap();
        let mut observed = [0u8; 6];
        parent.read_exact(&mut observed).unwrap();
        assert_eq!(&observed, b"phase\n");
    }

    #[test]
    fn inherited_stream_is_immediately_close_on_exec() {
        let (source, _peer) = UnixStream::pair().unwrap();
        let encoded = source.into_raw_fd().to_string();
        // SAFETY: `into_raw_fd` transferred the sole source ownership into
        // this call, and no other owning handle exists for it.
        let inherited = unsafe { take_inherited_duplex_channel("TEST_SESSION_FD", &encoded) }
            .expect("consume connected inherited stream");

        let flags = unsafe { libc::fcntl(inherited.stream.file().as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
    }

    #[test]
    fn inherited_stream_rejects_noncanonical_and_stdio_descriptors() {
        let noncanonical = unsafe { take_inherited_duplex_channel("TEST_SESSION_FD", "3\n") }
            .err()
            .expect("control characters must be rejected");
        assert!(noncanonical.contains("not canonical"), "{noncanonical}");
        let stdio = unsafe { take_inherited_duplex_channel("TEST_SESSION_FD", "2") }
            .err()
            .expect("standard I/O descriptors must be rejected");
        assert!(stdio.contains("overlaps standard I/O"), "{stdio}");
    }
}

/// One-shot authority to request cooperative termination of an exact owned
/// child. Requesting termination sends one `SIGTERM`; it never waits, sends
/// `SIGKILL`, or installs an escalation policy.
pub struct CooperativeChildTermination {
    #[cfg(target_os = "linux")]
    pidfd: OwnedFd,
}

/// Exact child identity after its one cooperative termination request has
/// been sent. Callers may poll for natural exit without gaining signal or
/// escalation authority.
pub struct PendingCooperativeChildTermination {
    #[cfg(target_os = "linux")]
    pidfd: OwnedFd,
}

impl CooperativeChildTermination {
    /// Pin the identity of a child that remains owned and unreaped by the
    /// caller. Linux retains a pidfd so the later request cannot target a
    /// recycled numeric PID. Platforms without pidfds fail closed.
    pub fn for_child(child: &process::Child) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                pidfd: open_pidfd(child.id())?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = child;
            Err("exact cooperative child termination requires Linux pidfds".to_owned())
        }
    }

    /// Consume this one-shot authority and send one cooperative termination
    /// request. An already-exited child is treated as success.
    pub fn request(self) -> Result<PendingCooperativeChildTermination, String> {
        #[cfg(target_os = "linux")]
        {
            match pidfd_send_signal_io(self.pidfd.as_raw_fd(), libc::SIGTERM) {
                Ok(()) => Ok(PendingCooperativeChildTermination { pidfd: self.pidfd }),
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
                    Ok(PendingCooperativeChildTermination { pidfd: self.pidfd })
                }
                Err(error) => Err(format!("request cooperative child termination: {error}")),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err("exact cooperative child termination requires Linux pidfds".to_owned())
        }
    }
}

impl PendingCooperativeChildTermination {
    /// Report whether the exact child has exited, without reaping it or
    /// changing its lifecycle. The original `Child` owner remains responsible
    /// for reaping.
    pub fn has_exited(&self) -> Result<bool, String> {
        #[cfg(target_os = "linux")]
        {
            loop {
                let mut pollfd = libc::pollfd {
                    fd: self.pidfd.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut pollfd, 1, 0) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(format!("poll cooperatively terminated child: {error}"));
                }
                if ready == 0 {
                    return Ok(false);
                }
                if pollfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                    return Err(format!(
                        "poll cooperatively terminated child returned unexpected events {:#x}",
                        pollfd.revents
                    ));
                }
                return Ok(pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err("exact cooperative child termination requires Linux pidfds".to_owned())
        }
    }
}

/// Disable core dumps for this process and all subsequently spawned
/// descendants.
#[cfg(unix)]
pub fn disable_process_core_dumps() -> Result<(), String> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        return Err(format!(
            "disable process core dumps: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Install an owner-private creation mask only in this command's forked
/// child. The parent process-wide mask is never changed.
#[cfg(unix)]
pub fn configure_owner_private_creation_mask(command: &mut process::Command) {
    use std::os::unix::process::CommandExt as _;

    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
}

/// Set an explicit child `argv[0]` without exposing platform command
/// extensions to the caller.
pub fn configure_command_argv0(command: &mut process::Command, argv0: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.arg0(argv0);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (command, argv0);
        Err("explicit child argv[0] is unavailable on this platform".to_owned())
    }
}

/// Result of a detached spawn.
pub struct SpawnResult {
    pub pid: u32,
}

/// Replace this process using the caller's already configured command.
/// Success never returns. Unlike spawn, pre-exec hooks run in this process;
/// an exec failure may already have changed stdio or other process state.
/// The caller must be a dedicated replacement boundary, not a daemon worker.
pub fn replace_current_process(command: &mut process::Command) -> std::io::Error {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.exec()
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "process replacement is unsupported on this platform",
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum CapturedStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Default)]
struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
    closed: bool,
    read_error: Option<std::io::ErrorKind>,
}

#[derive(Default)]
struct OutputCapture {
    state: Mutex<BoundedCapture>,
    changed: Condvar,
}

type SharedCapture = Arc<OutputCapture>;

/// One byte-preserving observer of a subprocess's existing bounded stdout
/// capture. This never takes over its pipe, drainer, or process lifecycle.
///
/// Reads wait for captured bytes or capture closure without polling or an extra
/// output queue. Cleanup may close capture before a descendant closes its pipe.
/// EOF is not process completion: callers must still settle the
/// exact [`RunningProcess`] and check its exit/timeout/output-limit result.
pub struct ProcessStdoutReader {
    capture: SharedCapture,
    offset: usize,
}

/// Observation failure is separate from exact subprocess settlement. The
/// observer interprets bytes; it never receives process termination authority.
#[derive(Debug)]
pub enum ProcessObservationError<E> {
    AlreadyConsumed,
    Start(std::io::Error),
    Panicked,
    Observation(E),
}

struct InterruptFailedObservation<'a>(Option<&'a AtomicBool>);

impl Drop for InterruptFailedObservation<'_> {
    fn drop(&mut self) {
        if let Some(failed) = self.0 {
            failed.store(true, Ordering::Release);
        }
    }
}

impl Read for ProcessStdoutReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let mut state = self
            .capture
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        loop {
            if self.offset < state.bytes.len() {
                let count = output.len().min(state.bytes.len() - self.offset);
                output[..count].copy_from_slice(&state.bytes[self.offset..self.offset + count]);
                self.offset += count;
                return Ok(count);
            }
            if state.truncated {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "subprocess stdout exceeded its capture bound",
                ));
            }
            if let Some(kind) = state.read_error {
                return Err(std::io::Error::new(
                    kind,
                    "subprocess stdout capture failed",
                ));
            }
            if state.closed {
                return Ok(0);
            }
            state = self
                .capture
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

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

#[cfg(target_os = "linux")]
struct OccupancyWatchdog {
    pid: libc::pid_t,
    cancel: Option<std::fs::File>,
}

#[cfg(target_os = "linux")]
impl OccupancyWatchdog {
    fn cancel_and_reap(&mut self) -> Result<(), String> {
        if let Some(mut cancel) = self.cancel.take() {
            let _ = cancel.write_all(&[1]);
            drop(cancel);
        }
        let mut status = 0;
        loop {
            let result = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if result == self.pid {
                self.pid = -1;
                return Ok(());
            }
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.pid = -1;
                    return Ok(());
                }
                return Err(format!("reap occupancy watchdog: {error}"));
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for OccupancyWatchdog {
    fn drop(&mut self) {
        // Loss of the daemon-side owner is not cancellation authority. The
        // child retains its keepalive writer and continues to the absolute
        // scope-kill deadline. A detached waiter prevents a zombie while this
        // daemon remains alive, but has no descriptor capable of cancelling
        // enforcement. Normal proved cleanup calls cancel_and_reap.
        self.cancel.take();
        let pid = self.pid;
        if pid <= 0 {
            return;
        }
        let _ = thread::Builder::new()
            .name("lillux-occupancy-watchdog-reaper".to_owned())
            .spawn(move || {
                let mut status = 0;
                while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
                    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                        break;
                    }
                }
            });
    }
}

#[cfg(target_os = "linux")]
fn arm_occupancy_watchdog(
    target_pidfd: BorrowedFd<'_>,
    scope_kill: std::fs::File,
    limit: &crate::time::OccupancyLimit,
    cleanup_allowance: Duration,
) -> Result<OccupancyWatchdog, String> {
    limit.validate()?;
    if !matches!(
        limit.window(cleanup_allowance)?,
        crate::time::OccupancyWindowState::Service { .. }
    ) {
        return Err("occupancy service window elapsed before target release".to_owned());
    }
    let cleanup_ns = u64::try_from(cleanup_allowance.as_nanos())
        .map_err(|_| "occupancy cleanup allowance overflows nanoseconds".to_owned())?;
    let service_tick = limit
        .start
        .tick_ns
        .checked_add(limit.maximum_occupancy_ns - cleanup_ns)
        .ok_or_else(|| "occupancy service deadline overflow".to_owned())?;
    let expiry_tick = limit
        .start
        .tick_ns
        .checked_add(limit.maximum_occupancy_ns)
        .ok_or_else(|| "occupancy expiry deadline overflow".to_owned())?;
    let timer_fd = unsafe { libc::timerfd_create(libc::CLOCK_BOOTTIME, libc::TFD_CLOEXEC) };
    if timer_fd < 0 {
        return Err(format!(
            "create occupancy watchdog timer: {}",
            std::io::Error::last_os_error()
        ));
    }
    let timer = unsafe { OwnedFd::from_raw_fd(timer_fd) };
    arm_absolute_boottime_timer(timer.as_raw_fd(), service_tick)?;
    let mut cancel_fds = [-1_i32; 2];
    if unsafe { libc::pipe2(cancel_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "create occupancy watchdog cancellation boundary: {}",
            std::io::Error::last_os_error()
        ));
    }
    let cancel_reader = unsafe { OwnedFd::from_raw_fd(cancel_fds[0]) };
    let cancel_writer = unsafe { OwnedFd::from_raw_fd(cancel_fds[1]) };
    let mut ready_fds = [-1_i32; 2];
    if unsafe { libc::pipe2(ready_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "create occupancy watchdog readiness boundary: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut ready_reader = unsafe { std::fs::File::from_raw_fd(ready_fds[0]) };
    let ready_writer = unsafe { OwnedFd::from_raw_fd(ready_fds[1]) };
    let duplicate_for_child = |fd: i32| -> Result<OwnedFd, String> {
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 8) };
        if duplicate < 0 {
            return Err(format!(
                "duplicate occupancy watchdog authority: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
    };
    let child_cancel_reader = duplicate_for_child(cancel_reader.as_raw_fd())?;
    let child_timer = duplicate_for_child(timer.as_raw_fd())?;
    let child_scope_kill = duplicate_for_child(scope_kill.as_raw_fd())?;
    let child_target_pidfd = duplicate_for_child(target_pidfd.as_raw_fd())?;
    let child_cancel_keepalive = duplicate_for_child(cancel_writer.as_raw_fd())?;
    let child_ready_writer = duplicate_for_child(ready_writer.as_raw_fd())?;
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(format!(
            "fork occupancy watchdog: {}",
            std::io::Error::last_os_error()
        ));
    }
    if child == 0 {
        unsafe {
            occupancy_watchdog_child(
                child_cancel_reader.as_raw_fd(),
                child_cancel_keepalive.as_raw_fd(),
                child_timer.as_raw_fd(),
                child_scope_kill.as_raw_fd(),
                child_target_pidfd.as_raw_fd(),
                child_ready_writer.as_raw_fd(),
                expiry_tick,
            )
        }
    }
    drop(ready_writer);
    drop(child_cancel_reader);
    drop(child_timer);
    drop(child_scope_kill);
    drop(child_target_pidfd);
    drop(child_cancel_keepalive);
    drop(child_ready_writer);
    drop(cancel_reader);
    let mut readiness_poll = libc::pollfd {
        fd: ready_reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let polled = unsafe { libc::poll(&mut readiness_poll, 1, 5_000) };
    let mut ready = [0_u8; 1];
    let acknowledged = polled == 1
        && readiness_poll.revents & libc::POLLIN != 0
        && ready_reader.read_exact(&mut ready).is_ok()
        && ready[0] == 1;
    if !acknowledged {
        let mut cancel = std::fs::File::from(cancel_writer);
        let _ = cancel.write_all(&[1]);
        let mut status = 0;
        while unsafe { libc::waitpid(child, &mut status, 0) } < 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
        return Err(
            "occupancy watchdog did not acknowledge its isolated enforcement boundary".to_owned(),
        );
    }
    Ok(OccupancyWatchdog {
        pid: child,
        cancel: Some(std::fs::File::from(cancel_writer)),
    })
}

#[cfg(target_os = "linux")]
fn arm_absolute_boottime_timer(fd: i32, tick_ns: u64) -> Result<(), String> {
    let seconds = tick_ns / 1_000_000_000;
    let nanoseconds = tick_ns % 1_000_000_000;
    let specification = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: libc::time_t::try_from(seconds)
                .map_err(|_| "occupancy timer seconds exceed time_t".to_owned())?,
            tv_nsec: libc::c_long::try_from(nanoseconds)
                .map_err(|_| "occupancy timer nanoseconds exceed c_long".to_owned())?,
        },
    };
    if unsafe {
        libc::timerfd_settime(
            fd,
            libc::TFD_TIMER_ABSTIME,
            &specification,
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err(format!(
            "arm occupancy watchdog timer: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Child side of the crash-surviving lifetime boundary. Only async-signal-safe
/// syscalls occur after fork; inherited daemon authority is closed before the
/// child waits or signals anything.
#[cfg(target_os = "linux")]
unsafe fn occupancy_watchdog_child(
    cancel_reader: i32,
    cancel_keepalive: i32,
    timer: i32,
    scope_kill: i32,
    target_pidfd: i32,
    ready_writer: i32,
    expiry_tick: u64,
) -> ! {
    for (source, target) in [
        cancel_reader,
        timer,
        scope_kill,
        target_pidfd,
        cancel_keepalive,
        ready_writer,
    ]
    .into_iter()
    .zip([3, 4, 5, 6, 7, 8])
    {
        if unsafe { libc::dup2(source, target) } < 0 {
            unsafe { libc::_exit(125) }
        }
    }
    if unsafe { libc::syscall(libc::SYS_close_range, 9_u32, u32::MAX, 2_u32) } != 0 {
        unsafe { libc::_exit(125) }
    }
    unsafe {
        libc::close(0);
        libc::close(1);
        libc::close(2);
        libc::prctl(libc::PR_SET_PDEATHSIG, 0, 0, 0, 0);
    }
    if unsafe { libc::write(8, [1_u8].as_ptr().cast(), 1) } != 1 {
        unsafe { libc::_exit(125) }
    }
    unsafe { libc::close(8) };
    let mut poll_fds = [
        libc::pollfd {
            fd: 3,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: 4,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, -1) };
        if result < 0 {
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            unsafe { kill_scope_and_exit(5) }
        }
        if poll_fds[0].revents != 0 {
            unsafe { libc::_exit(0) }
        }
        if poll_fds[1].revents & libc::POLLIN != 0 {
            break;
        }
    }
    // The service-boundary request is fenced by the retained pidfd. A numeric
    // PID or PGID may be recycled after daemon loss and is never safe here.
    let signal_result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            6,
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0_u32,
        )
    };
    if signal_result != 0 {
        let error = unsafe { *libc::__errno_location() };
        if error != libc::ESRCH {
            unsafe { kill_scope_and_exit(5) }
        }
    }
    let seconds = expiry_tick / 1_000_000_000;
    let nanoseconds = expiry_tick % 1_000_000_000;
    let expiry = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: seconds as libc::time_t,
            tv_nsec: nanoseconds as libc::c_long,
        },
    };
    if unsafe { libc::timerfd_settime(4, libc::TFD_TIMER_ABSTIME, &expiry, std::ptr::null_mut()) }
        != 0
    {
        unsafe { kill_scope_and_exit(5) }
    }
    for poll_fd in &mut poll_fds {
        poll_fd.revents = 0;
    }
    loop {
        let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, -1) };
        if result < 0 {
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            unsafe { kill_scope_and_exit(5) }
        }
        if poll_fds[0].revents != 0 {
            unsafe { libc::_exit(0) }
        }
        if poll_fds[1].revents & libc::POLLIN != 0 {
            break;
        }
    }
    unsafe { kill_scope_and_exit(5) }
}

#[cfg(target_os = "linux")]
unsafe fn kill_scope_and_exit(scope_kill_fd: i32) -> ! {
    let byte = b'1';
    loop {
        let written = unsafe { libc::write(scope_kill_fd, (&byte as *const u8).cast(), 1) };
        if written == 1 {
            unsafe { libc::_exit(0) }
        }
        if written < 0 && unsafe { *libc::__errno_location() } == libc::EINTR {
            continue;
        }
        // The hard controller itself failed. Do not misreport a clean exit;
        // the nonzero status remains diagnostic if the daemon survives.
        unsafe { libc::_exit(126) }
    }
}

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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct DescriptorLeaseLocation {
    file: &'static str,
    line: u32,
    column: u32,
}

#[derive(Default)]
struct DescriptorForkBarrierState {
    retained_scopes: usize,
    retained_scope_owners: HashMap<thread::ThreadId, usize>,
    retained_scope_locations: HashMap<thread::ThreadId, HashMap<DescriptorLeaseLocation, usize>>,
    waiting_forks: usize,
    fork_quiesced: bool,
    pending_fork_control_fds: BTreeSet<i32>,
    fork_child_close_fds: BTreeSet<i32>,
    // Files, not registered wrappers: release must close+deregister without
    // recursively entering this barrier through another registered Drop.
    // At most one entry per descriptor already registered at quiescence.
    deferred_child_closes: Vec<(i32, std::fs::File)>,
}

struct DescriptorForkBarrier {
    state: Mutex<DescriptorForkBarrierState>,
    changed: Condvar,
    waiting_descriptor_closers: AtomicUsize,
}

fn direct_attachment_fork_barrier() -> &'static DescriptorForkBarrier {
    DIRECT_ATTACHMENT_FORK_BARRIER.get_or_init(|| DescriptorForkBarrier {
        state: Mutex::new(DescriptorForkBarrierState::default()),
        changed: Condvar::new(),
        waiting_descriptor_closers: AtomicUsize::new(0),
    })
}

/// Shared proof that the current scope may own descriptor-backed authority
/// which a pre-exec attachment child must not inherit.
pub struct ForkSensitiveDescriptorLease {
    owner: thread::ThreadId,
    location: DescriptorLeaseLocation,
    retained: bool,
    _not_send: PhantomData<Rc<()>>,
}

/// Retain the process-wide fork-sensitive descriptor lease.
///
/// Acquire this before opening or locking descriptor-backed authority and keep
/// it until those descriptors/locks have been released. Acquisition is
/// intentionally infallible after poisoning: the barrier protects process
/// topology, not data whose consistency could be invalidated by a panic.
#[track_caller]
pub fn retain_fork_sensitive_descriptors() -> ForkSensitiveDescriptorLease {
    retain_fork_sensitive_descriptors_inner(None)
        .expect("undeadlined descriptor lease acquisition cannot expire")
}

/// The same fork barrier, bounded by the caller's existing operation deadline.
/// Readiness and retry code must not restart that deadline before acquisition.
///
/// Successful acquisition also proves that registered-descriptor drops which
/// completed before this call have physically settled: the exclusive fork
/// owner drains deferred closes before reopening this shared barrier. After a
/// consumed receiver has dropped, this includes its queued SCM_RIGHTS. Keep
/// the lease through the caller's settlement decision. It proves nothing about
/// active aliases, drops still executing elsewhere, or creator/child death;
/// callers must establish those separately. A timeout supplies no close proof.
#[track_caller]
pub fn retain_fork_sensitive_descriptors_until(
    deadline: crate::time::MonotonicDeadline,
) -> std::io::Result<ForkSensitiveDescriptorLease> {
    retain_fork_sensitive_descriptors_inner(Some(deadline))
}

#[track_caller]
fn retain_fork_sensitive_descriptors_inner(
    deadline: Option<crate::time::MonotonicDeadline>,
) -> std::io::Result<ForkSensitiveDescriptorLease> {
    let barrier = direct_attachment_fork_barrier();
    let owner = thread::current().id();
    let caller = std::panic::Location::caller();
    let location = DescriptorLeaseLocation {
        file: caller.file(),
        line: caller.line(),
        column: caller.column(),
    };
    let mut state = barrier
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while state.fork_quiesced
        || (state.waiting_forks != 0 && !state.retained_scope_owners.contains_key(&owner))
    {
        state = if let Some(deadline) = deadline {
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "descriptor lease deadline elapsed",
                ));
            }
            barrier
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0
        } else {
            barrier
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        };
    }
    if deadline.is_some_and(|deadline| deadline.has_elapsed()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "descriptor lease deadline elapsed",
        ));
    }
    state.retained_scopes = state
        .retained_scopes
        .checked_add(1)
        .expect("fork-sensitive descriptor lease count overflow");
    let owner_scopes = state.retained_scope_owners.entry(owner).or_default();
    *owner_scopes = owner_scopes
        .checked_add(1)
        .expect("fork-sensitive descriptor owner count overflow");
    let location_scopes = state
        .retained_scope_locations
        .entry(owner)
        .or_default()
        .entry(location)
        .or_default();
    *location_scopes = location_scopes
        .checked_add(1)
        .expect("fork-sensitive descriptor location count overflow");
    Ok(ForkSensitiveDescriptorLease {
        owner,
        location,
        retained: true,
        _not_send: PhantomData,
    })
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
        let owner_locations = state
            .retained_scope_locations
            .get_mut(&self.owner)
            .expect("fork-sensitive descriptor owner locations were not registered");
        let location_scopes = owner_locations
            .get_mut(&self.location)
            .expect("fork-sensitive descriptor lease location was not registered");
        *location_scopes = location_scopes
            .checked_sub(1)
            .expect("fork-sensitive descriptor location count underflow");
        if *location_scopes == 0 {
            owner_locations.remove(&self.location);
        }
        if owner_locations.is_empty() {
            state.retained_scope_locations.remove(&self.owner);
        }
        self.retained = false;
        if state.retained_scopes == 0 {
            barrier.changed.notify_all();
        }
    }
}

// Kernel-only probes which fork without exec must use this same barrier and
// child-close inventory. CLOEXEC alone cannot keep them from retaining a
// daemon lock, release pipe, or transferred authority for their lifetime.
pub(crate) struct QuiescedForkSensitiveDescriptors;

impl QuiescedForkSensitiveDescriptors {
    pub(crate) fn fork_child_close_fds(
        &self,
        preserved: &BTreeSet<i32>,
    ) -> Result<Vec<i32>, String> {
        let barrier = direct_attachment_fork_barrier();
        let state = barrier
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(state.fork_quiesced);
        if !state.pending_fork_control_fds.is_disjoint(preserved) {
            return Err("inherited authority aliases parent process-control authority".to_owned());
        }
        Ok(state
            .pending_fork_control_fds
            .iter()
            .chain(state.fork_child_close_fds.difference(preserved))
            .copied()
            .collect())
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

/// A long-lived exact descriptor which remains open in the parent but is
/// closed in every direct-attachment fork child before that child enters its
/// durable pre-exec hold. This is stronger than `FD_CLOEXEC`: the hold occurs
/// before exec and must not retain advisory locks or equivalent authority.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct ForkChildCloseFile {
    fd: i32,
    file: Option<std::fs::File>,
}

#[cfg(unix)]
impl std::ops::Deref for ForkChildCloseFile {
    type Target = std::fs::File;

    fn deref(&self) -> &Self::Target {
        self.file
            .as_ref()
            .expect("fork-child-close descriptor is present")
    }
}

#[cfg(unix)]
impl std::os::fd::AsRawFd for ForkChildCloseFile {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.fd
    }
}

#[cfg(unix)]
impl Drop for ForkChildCloseFile {
    fn drop(&mut self) {
        let barrier = direct_attachment_fork_barrier();
        barrier
            .waiting_descriptor_closers
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_add(1)
            })
            .expect("fork-child-close descriptor closer count overflow");
        let mut state = barrier
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.fork_quiesced {
            // Returning a timed operation must not wait in this destructor.
            // Preserve the exact live fd and its registration until the
            // snapshot owner releases its protected fork window. This is NOT
            // synchronous close evidence; use try_close_last_owner for that.
            assert!(state.fork_child_close_fds.contains(&self.fd));
            state
                .deferred_child_closes
                .push((self.fd, self.file.take().expect("registered file present")));
            assert!(state.deferred_child_closes.len() <= state.fork_child_close_fds.len());
        } else {
            // Close before deregistration, while no fork can consume a stale
            // coordinate or see an unregistered live authority.
            drop(self.file.take());
            assert!(
                state.fork_child_close_fds.remove(&self.fd),
                "fork-child-close descriptor was not registered"
            );
        }
        let previous_closers = barrier
            .waiting_descriptor_closers
            .fetch_sub(1, Ordering::SeqCst);
        assert_ne!(
            previous_closers, 0,
            "fork-child-close descriptor closer count underflow"
        );
        barrier.changed.notify_all();
    }
}

/// Register an already-open descriptor while its caller retains a
/// fork-sensitive lease acquired before opening it.
#[cfg(unix)]
pub(crate) fn register_fork_child_close_file(file: std::fs::File) -> ForkChildCloseFile {
    let fd = file.as_raw_fd();
    let barrier = direct_attachment_fork_barrier();
    let mut state = barrier
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        !state.fork_quiesced,
        "fork-child-close descriptor registered during a fork"
    );
    assert!(
        state.fork_child_close_fds.insert(fd),
        "fork-child-close descriptor was already registered"
    );
    ForkChildCloseFile {
        fd,
        file: Some(file),
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
            .waiting_descriptor_closers
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
            .waiting_descriptor_closers
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
        for (fd, file) in std::mem::take(&mut state.deferred_child_closes) {
            drop(file);
            assert!(
                state.fork_child_close_fds.remove(&fd),
                "deferred descriptor was not registered"
            );
        }
        state.fork_quiesced = false;
        barrier.changed.notify_all();
    }
}

fn retained_descriptor_scope_diagnostic(state: &DescriptorForkBarrierState) -> String {
    let mut owners = state
        .retained_scope_locations
        .iter()
        .map(|(owner, locations)| {
            let mut locations = locations
                .iter()
                .map(|(location, count)| {
                    format!(
                        "{}:{}:{} ({} scope{})",
                        location.file,
                        location.line,
                        location.column,
                        count,
                        if *count == 1 { "" } else { "s" }
                    )
                })
                .collect::<Vec<_>>();
            locations.sort();
            format!("{owner:?}: {}", locations.join(", "))
        })
        .collect::<Vec<_>>();
    owners.sort();
    if owners.is_empty() {
        "no retained scope owner was recorded".to_string()
    } else {
        owners.join("; ")
    }
}

pub(crate) fn quiesce_fork_sensitive_descriptors(
    deadline: Instant,
) -> Result<QuiescedForkSensitiveDescriptors, String> {
    let barrier = direct_attachment_fork_barrier();
    let owner = thread::current().id();
    let mut state = barrier
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.retained_scope_owners.contains_key(&owner) {
        return Err(format!(
            "process-control fork requested while the calling thread retains fork-sensitive descriptor authority ({})",
            retained_descriptor_scope_diagnostic(&state)
        ));
    }
    state.waiting_forks = state
        .waiting_forks
        .checked_add(1)
        .expect("fork-sensitive descriptor waiter count overflow");
    while state.fork_quiesced
        || state.retained_scopes != 0
        || barrier.waiting_descriptor_closers.load(Ordering::SeqCst) != 0
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            state.waiting_forks = state
                .waiting_forks
                .checked_sub(1)
                .expect("fork-sensitive descriptor waiter count underflow");
            barrier.changed.notify_all();
            return Err(format!(
                "timed out waiting for fork-sensitive descriptor authority to quiesce; retained scopes: {}; fork already quiesced: {}; pending fork-control closers: {}",
                retained_descriptor_scope_diagnostic(&state),
                state.fork_quiesced,
                barrier.waiting_descriptor_closers.load(Ordering::SeqCst)
            ));
        }
        let (next, _) = barrier
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
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
    cwd_directory: Option<i32>,
    child_status_reader_fd: i32,
    child_release_writer_fd: i32,
    inherited_child_close_fds: Vec<i32>,
    prepared_mappings: Option<PreparedInheritedMappings>,
}

/// A running subprocess that can be waited on later.
pub struct RunningProcess {
    // Platform ownership stays here. Applications retain the opaque recovery
    // evidence; they must not add OS handles to generic subprocess requests.
    process_scope: Option<crate::ProcessScope>,
    scope_cleanup_error: Option<String>,
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
    stdout_reader_taken: bool,
    drain_stop: Arc<AtomicBool>,
    output_overflow_rx: std::sync::mpsc::Receiver<CapturedStream>,
    start: Instant,
    timeout: f64,
    /// Present only for a trusted-launcher spawn whose target is blocked at
    /// the backend's final pre-exec boundary. Authoritative lifecycle callers
    /// release it only after persisting the exact reported process identity.
    attachment_release: Option<ProcessAttachmentRelease>,
    /// Independent Lillux-owned lifetime enforcement. The helper retains only
    /// the exact target pidfd, exact process-scope kill authority, and
    /// timer/cancel descriptors; it survives daemon death and owns no project
    /// authority.
    #[cfg(target_os = "linux")]
    occupancy_watchdog: Option<OccupancyWatchdog>,
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
    process_scope: Option<crate::ProcessScope>,
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

/// Proof that an attachment-pending process (or its pre-identity supervisor)
/// was aborted, its owned group proved quiescent, and its exact child reaped
/// without allowing target execution. Numeric fields identify the settled
/// operation; they are not a new signalling authority after reap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbortedProcess {
    pub pid: u32,
    pub pgid: i64,
}

/// Failure while crossing the attachment-to-running lifecycle boundary.
///
/// Callers may settle durable attachment only when `cleanup_is_settled()`
/// proves the exact child/wrapper and selected process scope are stopped.
/// A scoped cleanup failure retains an unresolved recovery obligation; its
/// diagnostic text and an absent target PID are not cleanup testimony.
#[derive(Debug)]
pub struct AttachmentReleaseError {
    pub phase: &'static str,
    pub result: SubprocessResult,
    cleanup_is_settled: bool,
}

impl AttachmentReleaseError {
    /// Exact cleanup testimony issued by this release owner, never inferred
    /// from the error string or the target's current liveness.
    pub fn cleanup_is_settled(&self) -> bool {
        self.cleanup_is_settled
    }
}

impl std::fmt::Display for AttachmentReleaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.phase, self.result.stderr)
    }
}

impl std::error::Error for AttachmentReleaseError {}

#[cfg(target_os = "linux")]
fn scoped_release_cleanup_outcome(
    process_cleanup: Result<(), String>,
    scope_cleanup: Result<(), String>,
) -> (String, bool) {
    let errors: Vec<_> = process_cleanup
        .err()
        .into_iter()
        .chain(scope_cleanup.err())
        .collect();
    if errors.is_empty() {
        (String::new(), true)
    } else {
        (
            format!(
                "; scoped attachment cleanup remains unproved: {}",
                errors.join("; ")
            ),
            false,
        )
    }
}

/// Failure of the caller-owned cleanup attempt. The attachment boundary has
/// been revoked, but selected scope or wrapper cleanup may remain unproved.
/// Only a successful abort result authorizes durable attachment settlement.
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
    /// Bind this evidence into the same durable attachment as the target.
    /// Only the configured Lillux provider may interpret it during recovery.
    pub fn scope_recovery(&self) -> Option<&crate::ProcessScopeRecovery> {
        self.process_scope
            .as_ref()
            .map(crate::ProcessScope::recovery)
    }
    /// Exact PID reported while the child was held after session creation.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Exact process group, proved to be led by [`Self::pid`].
    pub fn pgid(&self) -> i64 {
        self.pgid
    }

    /// Capture a portable exact identity through the held launch descriptor.
    /// This is the only valid route for attachment-pending children: reopening
    /// a numeric PID would lose the launch barrier's incarnation proof.
    pub fn exact_process_identity(&self) -> Result<crate::ExactProcessIdentity, String> {
        #[cfg(target_os = "linux")]
        {
            crate::process_control::capture_exact_process_identity_from_pidfd(
                self.pid,
                Some(
                    u32::try_from(self.pgid)
                        .map_err(|_| "attachment-pending process group is outside range")?,
                ),
                self.pidfd.as_fd(),
            )
        }
        #[cfg(not(target_os = "linux"))]
        Err("attachment-pending exact process identity is unavailable on this OS".to_owned())
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
    pub fn release_after_attachment(self) -> Result<RunningProcess, AttachmentReleaseError> {
        self.release_after_attachment_inner(None)
    }

    /// Release through a Lillux-owned finite-lifetime boundary. This is
    /// intentionally available only for the supervised containment route: a
    /// bare direct child can leave its process group, so it cannot support the
    /// hard crash-surviving occupancy contract.
    pub fn release_after_attachment_with_occupancy(
        self,
        limit: crate::time::OccupancyLimit,
        cleanup_allowance: Duration,
    ) -> Result<RunningProcess, AttachmentReleaseError> {
        self.release_after_attachment_inner(Some((limit, cleanup_allowance)))
    }

    fn release_after_attachment_inner(
        mut self,
        occupancy: Option<(crate::time::OccupancyLimit, Duration)>,
    ) -> Result<RunningProcess, AttachmentReleaseError> {
        if self
            .request_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let cleanup = self
                .abort_and_reap_inner()
                .map(|_| ())
                .map_err(|error| error.to_string());
            let (cleanup, cleanup_is_settled) = self.cleanup_failure_detail(cleanup);
            let result = spawn_failure(
                Instant::now(),
                format!(
                    "release after attachment refused: request deadline expired before release{cleanup}",
                ),
            );
            return Err(AttachmentReleaseError {
                phase: "release after attachment",
                cleanup_is_settled,
                result,
            });
        }
        if let Err(error) = self.check_exact_process_alive() {
            let cleanup = self
                .abort_and_reap_inner()
                .map(|_| ())
                .map_err(|error| error.to_string());
            let (cleanup, cleanup_is_settled) = self.cleanup_failure_detail(cleanup);
            let result = spawn_failure(
                Instant::now(),
                format!("release after attachment refused: {error}{cleanup}"),
            );
            return Err(AttachmentReleaseError {
                phase: "release after attachment",
                cleanup_is_settled,
                result,
            });
        }

        let owner = self.owner.take().expect("attachment owner is present");
        match owner {
            AttachmentPendingOwner::Direct {
                worker,
                mut release_registration,
            } => {
                if occupancy.is_some() {
                    drop(release_registration);
                    let settlement = prove_attachment_cleanup(
                        self.pidfd.as_raw_fd(),
                        settle_direct_attachment_worker(self.pid, worker),
                    );
                    let (detail, cleanup_is_settled) = self.cleanup_failure_detail(settlement);
                    return Err(AttachmentReleaseError {
                        phase: "release after attachment",
                        cleanup_is_settled,
                        result: spawn_failure(
                            Instant::now(),
                            format!(
                                "hard occupancy requires Lillux supervised containment{detail}"
                            ),
                        ),
                    });
                }
                if let Err(error) = release_registration.write_release() {
                    drop(release_registration);
                    let settlement = prove_attachment_cleanup(
                        self.pidfd.as_raw_fd(),
                        settle_direct_attachment_worker(self.pid, worker),
                    );
                    let (detail, cleanup_is_settled) = self.cleanup_failure_detail(settlement);
                    return Err(AttachmentReleaseError {
                        phase: "release after attachment",
                        cleanup_is_settled,
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
                        let (detail, cleanup_is_settled) = self.cleanup_failure_detail(cleanup);
                        Err(AttachmentReleaseError {
                            phase: "exec after attachment release",
                            cleanup_is_settled,
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
                            self.process_scope.as_ref(),
                        );
                        let (detail, cleanup_is_settled) = self.cleanup_failure_detail(cleanup);
                        Err(AttachmentReleaseError {
                            phase: "exec after attachment release",
                            cleanup_is_settled,
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
                    let (detail, cleanup_is_settled) = self.cleanup_failure_detail(cleanup);
                    return Err(AttachmentReleaseError {
                        phase: "release after attachment",
                        cleanup_is_settled,
                        result: spawn_failure(
                            Instant::now(),
                            format!(
                                "supervised target was not releasable after attachment: {error}{detail}",
                            ),
                        ),
                    });
                }
                if let Some((limit, cleanup_allowance)) = occupancy {
                    let enforcement = running
                        .process_scope
                        .as_ref()
                        .ok_or_else(|| {
                            "hard occupancy requires an exact Lillux process scope".to_owned()
                        })
                        .and_then(|scope| scope.occupancy_watchdog_kill_descriptor())
                        .and_then(|kill| {
                            arm_occupancy_watchdog(
                                self.pidfd.as_fd(),
                                kill,
                                &limit,
                                cleanup_allowance,
                            )
                        });
                    match enforcement {
                        Ok(watchdog) => running.occupancy_watchdog = Some(watchdog),
                        Err(error) => {
                            let cleanup = prove_attachment_cleanup(
                                self.pidfd.as_raw_fd(),
                                running.abort_and_reap_checked(),
                            );
                            let (detail, cleanup_is_settled) = self.cleanup_failure_detail(cleanup);
                            return Err(AttachmentReleaseError {
                                phase: "release after attachment",
                                cleanup_is_settled,
                                result: spawn_failure(
                                    Instant::now(),
                                    format!(
                                        "arm crash-surviving occupancy enforcement: {error}{detail}"
                                    ),
                                ),
                            });
                        }
                    }
                }
                match running.release_attachment_boundary() {
                    Ok(()) => Ok(*running),
                    Err(error) => {
                        let cleanup = prove_attachment_cleanup(
                            self.pidfd.as_raw_fd(),
                            running.abort_and_reap_checked(),
                        );
                        let (detail, cleanup_is_settled) = self.cleanup_failure_detail(cleanup);
                        Err(AttachmentReleaseError {
                            phase: "release after attachment",
                            cleanup_is_settled,
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
            Err(error) if self.process_scope.is_some() => Err(error),
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
    fn cleanup_failure_detail(&self, cleanup: Result<(), String>) -> (String, bool) {
        if let Some(scope) = &self.process_scope {
            // A pidfd/group proof never replaces the explicitly selected scope
            // proof, including exec failure and a panicked spawn worker. Keep
            // recovery evidence with the attachment if either duty is unproved.
            let scope_cleanup = scope.terminate_and_wait(ATTACHMENT_ABORT_SETTLE_TIMEOUT);
            return scoped_release_cleanup_outcome(cleanup, scope_cleanup);
        }
        match cleanup {
            Ok(()) => (String::new(), true),
            Err(error) => {
                // A release error may escape only after exact cleanup proof;
                // otherwise RyeOS could compare-clear the durable attachment
                // while this process remained live.
                complete_attachment_cleanup(self.pidfd.as_raw_fd(), self.pgid);
                (
                    format!(
                        "; initial cleanup proof failed: {error}; cleanup completed synchronously"
                    ),
                    true,
                )
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn cleanup_failure_detail(&self, cleanup: Result<(), String>) -> (String, bool) {
        match cleanup {
            Ok(()) => (String::new(), true),
            Err(error) => (format!("; cleanup failed: {error}"), false),
        }
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
        if let Some(scope) = &self.process_scope {
            // The scope includes all descendants; the structured owner also
            // owes wrapper reap. Never turn a failed scope settlement into a
            // successful group-only AbortedProcess testimony.
            let scope_result = scope.terminate_and_wait(ATTACHMENT_ABORT_SETTLE_TIMEOUT);
            return match (result, scope_result) {
                (Ok(()), Ok(())) => Ok(AbortedProcess {
                    pid: self.pid,
                    pgid: self.pgid,
                }),
                (result, scope_result) => Err(AttachmentAbortError {
                    pid: self.pid,
                    detail: result
                        .err()
                        .into_iter()
                        .chain(scope_result.err())
                        .collect::<Vec<_>>()
                        .join("; "),
                }),
            };
        }
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
        if let Some(scope) = &self.process_scope {
            let timeout =
                self.request_deadline
                    .map_or(SUPERVISED_STATUS_SETUP_TIMEOUT, |deadline| {
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(SUPERVISED_STATUS_SETUP_TIMEOUT)
                    });
            scope.require_held_member(self.pid, timeout)?;
        }
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
        if self.abort_and_reap_inner().is_err() && self.process_scope.is_none() {
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
    pub fn scope_recovery(&self) -> Option<&crate::ProcessScopeRecovery> {
        self.process_scope
            .as_ref()
            .map(crate::ProcessScope::recovery)
    }
    /// Observe raw stdout from its first byte, including bytes already captured
    /// before this call. Available once, after the attachment/release boundary.
    /// The process must be waited or aborted concurrently with blocking reads
    /// so its existing deadline and overflow supervision remain active.
    pub fn take_stdout_reader(&mut self) -> Option<ProcessStdoutReader> {
        if self.stdout_reader_taken {
            return None;
        }
        self.stdout_reader_taken = true;
        Some(ProcessStdoutReader {
            capture: Arc::clone(&self.stdout_capture),
            offset: 0,
        })
    }

    /// Return the bounded tail currently captured from stderr without waiting
    /// for, signalling, or otherwise changing the process lifecycle.
    ///
    /// This is intentionally a fixed-size diagnostic view. Protocol owners
    /// can use it when a separate control channel fails, while the ordinary
    /// `wait`/`abort` boundary remains the sole owner of process settlement.
    pub fn stderr_diagnostic_tail(&self) -> Option<String> {
        const DIAGNOSTIC_TAIL_BYTES: usize = 2 * 1024;

        let capture = self
            .stderr_capture
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if capture.bytes.is_empty() {
            return None;
        }
        let start = capture.bytes.len().saturating_sub(DIAGNOSTIC_TAIL_BYTES);
        let body = String::from_utf8_lossy(&capture.bytes[start..]);
        if capture.truncated || start != 0 {
            Some(format!(
                "… (bounded stderr tail; earlier bytes omitted)\n{body}"
            ))
        } else {
            Some(body.into_owned())
        }
    }

    /// Wait up to `timeout` for a natural process exit without terminating a
    /// still-running process. Ownership is returned on timeout, failed
    /// observation, or unproved cleanup. An Ok result proves both descendant
    /// settlement and launcher reap, not just the original target's exit.
    ///
    /// Protocols with a separate control channel use this after channel EOF:
    /// a naturally exited child can be settled with its captured output,
    /// while a child that merely dropped the channel remains available for
    /// the caller's ordinary abort policy.
    pub fn wait_for_natural_exit(mut self, timeout: Duration) -> Result<SubprocessResult, Self> {
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return Err(self);
        };
        loop {
            match poll_wrapper(&mut self.child) {
                Ok(WrapperPoll::ExitedUnreaped) => {
                    // Readiness callers treat Ok as completed cleanup. Do
                    // not turn a failed scope/group barrier into that proof
                    // merely by attaching a diagnostic to an exit result.
                    // Keep the typed owner available for checked abort/retry.
                    if self.settle_processes_before_drains().is_err() {
                        return Err(self);
                    }
                    // Child retains its reaped status; this does not reap a
                    // second process or reopen a numeric PID.
                    return match self.child.wait() {
                        Ok(status) => Ok(self.completed_result(status)),
                        Err(_) => Err(self),
                    };
                }
                #[cfg(not(target_os = "linux"))]
                Ok(WrapperPoll::ExitedReaped(status)) => {
                    self.wrapper_reaped = true;
                    if self.settle_processes_before_drains().is_err() {
                        return Err(self);
                    }
                    return Ok(self.completed_result(status));
                }
                Ok(WrapperPoll::Running) => {
                    if Instant::now() >= deadline {
                        return Err(self);
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(_) => return Err(self),
            }
        }
    }

    fn validate_attachment_release_ready(&mut self) -> Result<(), String> {
        let stdout_truncated = self
            .stdout_capture
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .truncated;
        let stderr_truncated = self
            .stderr_capture
            .state
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
            .write_release()
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

    /// Settle a supervised setup failure through the same owner as a running
    /// process. Keep the unreaped wrapper as the PGID fence until every group
    /// member is quiescent. Only an unconsumed attachment boundary proves that
    /// target execution was never released; ordinary running failures do not.
    fn into_spawn_failure(mut self, mut result: SubprocessResult) -> SubprocessResult {
        let held = self.attachment_release.take().is_some();
        let identity = AbortedProcess {
            pid: self.pid,
            pgid: self.pgid,
        };
        let settlement = self.settle_processes_before_drains();
        let (_, stderr) = self.finish_drains();
        // Launcher status and stderr are independent pipes. Read diagnostics
        // only after the existing bounded drain has settled; an EOF on status
        // does not imply the stderr drainer has observed the final bytes.
        result.stderr = append_captured_stderr(result.stderr, &stderr);
        match settlement {
            Ok(()) if held => result.aborted_before_attachment = Some(identity),
            Ok(()) => {}
            Err(error) => {
                result.stderr = append_diagnostic(
                    &result.stderr,
                    &format!("held-launch cleanup remains unproved: {error}"),
                );
            }
        }
        result
    }

    /// Wait for the process to finish (or time out) and return the result.
    pub fn wait(self) -> SubprocessResult {
        self.wait_interruptible(|| false)
    }

    /// Observe the existing capture concurrently with the sole wait owner.
    /// Use one blocking caller, not two jobs in a bounded executor pool: a
    /// silent observer could otherwise occupy the only slot needed to start
    /// deadline supervision. OS thread lifetime belongs here, while byte
    /// interpretation and publication remain in the caller's closure.
    ///
    /// The closure must finish after capture closes and must not depend on
    /// this method returning. Observer failure/panic interrupts the existing
    /// waiter, which alone terminates, reaps and closes capture before join.
    pub fn wait_with_stdout<T: Send, E: Send>(
        self,
        observe: impl FnOnce(ProcessStdoutReader) -> Result<T, E> + Send,
    ) -> (SubprocessResult, Result<T, ProcessObservationError<E>>) {
        self.wait_with_stdout_interruptible(observe, || false)
    }

    /// Observe stdout while also honoring a caller-owned semantic stop
    /// predicate. Lillux remains the only signal/reap owner and combines the
    /// predicate with observer failure under the same supervised wait.
    pub fn wait_with_stdout_interruptible<T: Send, E: Send>(
        mut self,
        observe: impl FnOnce(ProcessStdoutReader) -> Result<T, E> + Send,
        mut interrupted: impl FnMut() -> bool,
    ) -> (SubprocessResult, Result<T, ProcessObservationError<E>>) {
        let Some(reader) = self.take_stdout_reader() else {
            return (
                self.wait_interruptible(|| true),
                Err(ProcessObservationError::AlreadyConsumed),
            );
        };
        let failed = AtomicBool::new(false);
        thread::scope(|scope| {
            let failure = &failed;
            let observer = thread::Builder::new().spawn_scoped(scope, move || {
                let mut guard = InterruptFailedObservation(Some(failure));
                let result = observe(reader);
                if result.is_ok() {
                    guard.0 = None;
                }
                result
            });
            match observer {
                Ok(observer) => {
                    let completion =
                        self.wait_interruptible(|| failed.load(Ordering::Acquire) || interrupted());
                    let observed = match observer.join() {
                        Ok(result) => result.map_err(ProcessObservationError::Observation),
                        Err(_) => Err(ProcessObservationError::Panicked),
                    };
                    (completion, observed)
                }
                Err(error) => (
                    self.wait_interruptible(|| true),
                    Err(ProcessObservationError::Start(error)),
                ),
            }
        })
    }

    /// Wait under the same deadline, output and exact-child ownership as
    /// `wait`, allowing the protocol observer to report a fatal failure.
    /// The predicate grants no signal handle: this owner alone terminates
    /// and reaps the supervised process before returning its failed result.
    pub fn wait_interruptible(mut self, mut interrupted: impl FnMut() -> bool) -> SubprocessResult {
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
                aborted_before_attachment: None,
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
                    if interrupted() {
                        return self.wait_error_result(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "process observation failed",
                        ));
                    }
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
            success: code == 0 && self.scope_cleanup_error.is_none(),
            stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
            stderr: self.with_scope_cleanup_diagnostic(&String::from_utf8_lossy(&err.bytes)),
            exit_code: code,
            duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
            pid: self.pid,
            timed_out: false,
            launcher_refusal: None,
            aborted_before_attachment: None,
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
            aborted_before_attachment: None,
            output_limit_exceeded: output_limit_exceeded(&out, &err),
            stdout_truncated: out.truncated,
            stderr_truncated: err.truncated,
        }
    }

    fn kill_supervised_processes(&mut self) {
        if self.groups_terminated {
            return;
        }
        if let Some(scope) = &self.process_scope {
            self.scope_cleanup_error = scope
                .terminate_and_wait(ATTACHMENT_ABORT_SETTLE_TIMEOUT)
                .err();
            self.groups_terminated = self.scope_cleanup_error.is_none();
            // An explicitly selected scope never degrades to numeric group
            // cleanup. Retain failed scope evidence for the durable owner.
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
        if self.wrapper_reaped || self.scope_cleanup_error.is_some() {
            return;
        }
        if self.child.wait().is_ok() {
            self.wrapper_reaped = true;
        }
    }

    fn abort_and_reap_inner(&mut self) -> Result<(), String> {
        let result = self.settle_processes_before_drains();
        let _ = self.finish_drains();
        result
    }

    fn settle_processes_before_drains(&mut self) -> Result<(), String> {
        self.kill_supervised_processes();
        if let Some(error) = self.scope_cleanup_error.clone() {
            return Err(format!("execution scope cleanup remains unproved: {error}"));
        }
        #[cfg(target_os = "linux")]
        let group_result = if self.process_scope.is_some() {
            // The scope's completed termination includes every descendant and
            // the wrapper. Reaping remains a separate owned-child obligation.
            Ok(())
        } else {
            self.settle_owned_group_before_wrapper_reap()
        };
        #[cfg(not(target_os = "linux"))]
        let group_result = Ok(());
        // The unreaped wrapper is the process-group identity fence. Reaping
        // it before group quiescence is proved would leave only a numeric
        // PGID, which may later be reused. Keep it owned across retries.
        let reap_result = match &group_result {
            Ok(()) => self.reap_wrapper_checked(),
            Err(_) => Ok(()),
        };
        match (group_result, reap_result) {
            (Ok(()), Ok(())) => {
                #[cfg(target_os = "linux")]
                if let Some(mut watchdog) = self.occupancy_watchdog.take() {
                    watchdog.cancel_and_reap()?;
                }
                Ok(())
            }
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
        // Consumed JoinHandles already record which captures were settled.
        // Drop retries process cleanup, but must not clone retained reader
        // output a second time merely to discard it.
        let settle_stdout = self.stdout_thread.is_some();
        let settle_stderr = self.stderr_thread.is_some();
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
            if settle_stdout {
                take_capture(&self.stdout_capture)
            } else {
                BoundedCapture::default()
            },
            if settle_stderr {
                take_capture(&self.stderr_capture)
            } else {
                BoundedCapture::default()
            },
        )
    }

    fn timeout_result(&self, out: BoundedCapture, err: BoundedCapture) -> SubprocessResult {
        SubprocessResult {
            success: false,
            stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
            stderr: self.with_scope_cleanup_diagnostic(&append_diagnostic(
                &String::from_utf8_lossy(&err.bytes),
                &format!("Command timed out after {} seconds", self.timeout),
            )),
            exit_code: -1,
            duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
            pid: self.pid,
            timed_out: true,
            launcher_refusal: None,
            aborted_before_attachment: None,
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
            stderr: self.with_scope_cleanup_diagnostic(&append_diagnostic(
                &String::from_utf8_lossy(&err.bytes),
                &format!(
                    "Command exceeded the node-owned {} output retention limit; termination was requested",
                    exceeded.as_str()
                ),
            )),
            exit_code: -1,
            duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
            pid: self.pid,
            timed_out: false,
            launcher_refusal: None,
            aborted_before_attachment: None,
            output_limit_exceeded: Some(exceeded),
            stdout_truncated: out.truncated,
            stderr_truncated: err.truncated,
        }
    }

    fn with_scope_cleanup_diagnostic(&self, stderr: &str) -> String {
        match &self.scope_cleanup_error {
            Some(error) => append_diagnostic(
                stderr,
                &format!("execution scope cleanup remains unproved: {error}"),
            ),
            None => stderr.to_owned(),
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
    if request.supervised_status.as_ref().is_some_and(|status| {
        matches!(
            status.state,
            SupervisedProcessStatusState::AwaitingAttachment { .. }
        )
    }) {
        return Err(spawn_failure(
            Instant::now(),
            "Failed to spawn: attachment-bearing supervision requires spawn_awaiting_attachment",
        ));
    }
    lib_spawn_with_stdio(request, false, None, None, None)
}

/// Spawn with inherited terminal stdio while retaining the same session,
/// supervised-launcher status, timeout, process-group cleanup, and wait
/// contract as captured execution.
pub fn lib_spawn_inherited_stdio(
    request: SubprocessRequest,
) -> Result<RunningProcess, SubprocessResult> {
    if request.supervised_status.as_ref().is_some_and(|status| {
        matches!(
            status.state,
            SupervisedProcessStatusState::AwaitingAttachment { .. }
        )
    }) {
        return Err(spawn_failure(
            Instant::now(),
            "Failed to spawn: attachment-bearing supervision requires spawn_awaiting_attachment",
        ));
    }
    lib_spawn_with_stdio(request, true, None, None, None)
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
    request: SubprocessRequest,
) -> Result<ProcessAwaitingAttachment, SubprocessResult> {
    lib_spawn_awaiting_attachment_in_scope(request, None)
}

#[cfg(target_os = "linux")]
pub(crate) fn lib_spawn_awaiting_attachment_in_scope(
    mut request: SubprocessRequest,
    process_scope: Option<crate::ProcessScope>,
) -> Result<ProcessAwaitingAttachment, SubprocessResult> {
    let start = Instant::now();
    let attachment_scope = process_scope
        .as_ref()
        .map(crate::ProcessScope::control_authority);
    if let Some(status) = request.supervised_status.as_ref() {
        if !matches!(
            status.state,
            SupervisedProcessStatusState::AwaitingAttachment { .. }
        ) {
            return Err(spawn_failure(
                start,
                "Failed to spawn awaiting attachment: supervised backend omitted its required target attachment boundary",
            ));
        }
        let timeout = request.timeout;
        let running = lib_spawn_with_stdio(request, false, None, process_scope, None)?;
        if running.attachment_release.is_none() {
            return Err(running.into_spawn_failure(spawn_failure(
                start,
                "Failed to spawn awaiting attachment: supervised target attachment boundary disappeared",
            )));
        }
        let observed_birth = match read_linux_process_birth(running.pid) {
            Ok(birth) => birth,
            Err(error) => {
                return Err(running.into_spawn_failure(spawn_failure(
                    start,
                    format!("Failed to inspect supervised target awaiting attachment: {error}"),
                )));
            }
        };
        let pidfd = match open_pidfd(running.pid) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                return Err(running.into_spawn_failure(spawn_failure(
                    start,
                    format!("Failed to pin supervised target awaiting attachment: {error}"),
                )));
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
            return Err(running.into_spawn_failure(spawn_failure(
                start,
                format!("Invalid supervised target awaiting attachment: {error}"),
            )));
        }
        return verify_scoped_attachment(
            ProcessAwaitingAttachment {
                process_scope: attachment_scope,
                pid: running.pid,
                pgid: running.pgid,
                owner: Some(AttachmentPendingOwner::Supervised {
                    running: Box::new(running),
                }),
                pidfd,
                request_deadline: request_timeout_duration(timeout)
                    .and_then(|duration| start.checked_add(duration)),
            },
            start,
            supervised_setup_deadline(start, timeout),
        );
    }
    let timeout = request.timeout;
    let setup_deadline = supervised_setup_deadline(start, timeout);
    let cwd_directory = match request.cwd.take() {
        Some(path) => Some(open_attachment_cwd(&path, start)?),
        None => None,
    };
    let raw_inherited = normalize_inherited_descriptors(&request.inherited_fds)
        .map_err(|error| spawn_failure(start, error))?;
    let prepared_mappings = prepare_inherited_fd_mappings(
        &request.inherited_fd_mappings,
        &raw_inherited,
        &inherited_mapping_control_descriptors(None, request.supervised_status.as_ref()),
    )
    .map_err(|error| spawn_failure(start, error))?;
    // Keep exact request/mapping lifelines through the protected setup window.
    // An early worker error cannot retire aliases before main has joined and
    // observed its outcome; deferred Drop is not synchronous close evidence.
    // Never unquiesce before joining a worker which may not have forked yet.
    let mut parent_lifelines = request.inherited_fds.clone();
    parent_lifelines.extend(
        request
            .inherited_fd_mappings
            .iter()
            .map(|mapping| mapping.source.clone()),
    );
    parent_lifelines.extend(prepared_mappings.lifelines.iter().cloned());
    if let Some(directory) = &cwd_directory {
        parent_lifelines.push(directory.clone());
    }
    let preserved = normalize_inherited_descriptors(&parent_lifelines)
        .map_err(|error| spawn_failure(start, error))?
        .into_iter()
        .collect();
    // No other direct child may fork while these control pipes are created.
    // Snapshot the control descriptors of already-held children so the new
    // child can close only those known authorities at its final setup hook.
    let fork_sensitive_descriptors =
        quiesce_fork_sensitive_descriptors(setup_deadline).map_err(|error| {
            spawn_failure(
                start,
                format!("Failed to spawn awaiting attachment: {error}"),
            )
        })?;
    let inherited_child_close_fds = fork_sensitive_descriptors
        .fork_child_close_fds(&preserved)
        .map_err(|error| spawn_failure(start, error))?;
    let (status_reader, status_writer) = attachment_pipe("readiness", start)?;
    let (release_reader, release_writer) = attachment_pipe("release", start)?;
    let child_status_reader_fd = status_reader.as_raw_fd();
    let child_release_writer_fd = release_writer.as_raw_fd();
    let gate = AttachmentWorkerGate {
        status_writer,
        release_reader,
        cwd_directory: cwd_directory
            .as_ref()
            .map(|directory| directory.file().as_raw_fd()),
        child_status_reader_fd,
        child_release_writer_fd,
        inherited_child_close_fds,
        prepared_mappings: Some(prepared_mappings),
    };

    // A child held before exec retains every CLOEXEC descriptor inherited at
    // fork. Quiesce scopes which own descriptor-backed authority until the
    // worker reports the final hold boundary; otherwise a concurrent child can
    // inherit an advisory lock and deadlock the owner's durable attach path.
    let worker = thread::Builder::new()
        .name("lillux-attachment-spawn".to_string())
        .spawn(move || lib_spawn_with_stdio(request, false, Some(gate), process_scope, None))
        .map_err(|error| {
            spawn_failure(
                start,
                format!("Failed to spawn awaiting attachment worker: {error}"),
            )
        })?;

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

    verify_scoped_attachment(
        ProcessAwaitingAttachment {
            process_scope: attachment_scope,
            pid: ready.pid,
            pgid: ready.pgid,
            owner: Some(AttachmentPendingOwner::Direct {
                worker,
                release_registration,
            }),
            pidfd,
            request_deadline: request_timeout_duration(timeout)
                .and_then(|duration| start.checked_add(duration)),
        },
        start,
        setup_deadline,
    )
}

#[cfg(target_os = "linux")]
fn verify_scoped_attachment(
    pending: ProcessAwaitingAttachment,
    start: Instant,
    deadline: Instant,
) -> Result<ProcessAwaitingAttachment, SubprocessResult> {
    let membership = match &pending.process_scope {
        Some(scope) => scope.require_held_member(
            pending.pid,
            deadline.saturating_duration_since(Instant::now()),
        ),
        None => Ok(()),
    };
    if let Err(error) = membership {
        let cleanup = pending.abort_and_reap();
        let mut result = spawn_failure(start, format!("invalid scoped attachment: {error}"));
        match cleanup {
            Ok(aborted) => result.aborted_before_attachment = Some(aborted),
            Err(error) => result
                .stderr
                .push_str(&format!("; cleanup remains unproved: {error}")),
        }
        return Err(result);
    }
    Ok(pending)
}

#[cfg(target_os = "linux")]
fn inherited_mapping_control_descriptors(
    attachment_gate: Option<&AttachmentWorkerGate>,
    supervised_status: Option<&SupervisedProcessStatus>,
) -> BTreeSet<i32> {
    use std::os::fd::AsRawFd as _;

    let mut descriptors = BTreeSet::new();
    if let Some(gate) = attachment_gate {
        descriptors.insert(gate.status_writer.as_raw_fd());
        descriptors.insert(gate.release_reader.as_raw_fd());
        descriptors.insert(gate.child_status_reader_fd);
        descriptors.insert(gate.child_release_writer_fd);
        if let Some(directory) = gate.cwd_directory {
            descriptors.insert(directory);
        }
        descriptors.extend(gate.inherited_child_close_fds.iter().copied());
    }
    if let Some(status) = supervised_status {
        match &status.state {
            SupervisedProcessStatusState::Run { reader } => {
                descriptors.insert(reader.file().as_raw_fd());
            }
            SupervisedProcessStatusState::AwaitingAttachment {
                reader,
                attachment_release,
            } => {
                descriptors.insert(reader.file().as_raw_fd());
                if let Some(writer) = attachment_release.writer.as_ref() {
                    descriptors.insert(writer.fd);
                }
            }
        }
    }
    descriptors
}

#[cfg(all(unix, not(target_os = "linux")))]
fn inherited_mapping_control_descriptors(
    supervised_status: Option<&SupervisedProcessStatus>,
) -> BTreeSet<i32> {
    let _ = supervised_status;
    BTreeSet::new()
}

/// Prepare collision-free source copies and reserve every otherwise-free
/// target descriptor before `Command` allocates its private exec-error pipe.
/// Every temporary alias shares the existing fork-close lifetime owner.
/// Prepare before attachment quiescence, not inside its blocked spawn worker:
/// an unrelated held child must close these copies even while spawn is held.
#[cfg(unix)]
struct PreparedInheritedMappings {
    pairs: Vec<(i32, i32)>,
    lifelines: Vec<InheritedDescriptorAuthority>,
}

#[cfg(unix)]
fn normalize_inherited_descriptors(
    authorities: &[InheritedDescriptorAuthority],
) -> Result<Vec<i32>, String> {
    authorities
        .iter()
        .map(|authority| {
            i32::try_from(authority.inherited_descriptor()?)
                .map_err(|_| "inherited descriptor exceeds platform range".to_owned())
        })
        .collect()
}

#[cfg(unix)]
fn prepare_inherited_fd_mappings(
    mappings: &[InheritedDescriptorMapping],
    ordinary_inherited: &[i32],
    forbidden_targets: &BTreeSet<i32>,
) -> Result<PreparedInheritedMappings, String> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    if mappings.is_empty() {
        return Ok(PreparedInheritedMappings {
            pairs: Vec::new(),
            lifelines: Vec::new(),
        });
    }
    let mut targets = BTreeSet::new();
    let mut sources = BTreeSet::new();
    let mut normalized = Vec::with_capacity(mappings.len());
    let mut maximum_target = 2_i32;
    for mapping in mappings {
        let source = i32::try_from(mapping.source_descriptor()?)
            .map_err(|_| "mapped inherited source exceeds the platform descriptor range")?;
        let target = i32::try_from(mapping.target_fd)
            .map_err(|_| "mapped inherited target exceeds the platform descriptor range")?;
        if target == libc::STDOUT_FILENO || target == libc::STDERR_FILENO {
            return Err(format!(
                "mapped inherited target descriptor {target} overlaps stdout or stderr"
            ));
        }
        if !targets.insert(target) {
            return Err(format!(
                "duplicate mapped inherited target descriptor {target}"
            ));
        }
        if !sources.insert(source) {
            return Err(format!(
                "duplicate mapped inherited source descriptor {source}"
            ));
        }
        if ordinary_inherited.contains(&target) {
            return Err(format!(
                "mapped target descriptor {target} aliases an ordinary inherited authority"
            ));
        }
        if ordinary_inherited.contains(&source) {
            return Err(format!(
                "mapped source descriptor {source} aliases an ordinary inherited authority"
            ));
        }
        if forbidden_targets.contains(&target) {
            return Err(format!(
                "mapped target descriptor {target} aliases Lillux process-control authority"
            ));
        }
        maximum_target = maximum_target.max(target);
        normalized.push((source, target));
    }

    let temporary_floor = maximum_target
        .checked_add(1)
        .ok_or_else(|| "mapped inherited target descriptor overflows".to_owned())?
        .max(3);
    let lease = retain_fork_sensitive_descriptors();
    let mut source_copies = Vec::with_capacity(normalized.len());
    for (source, _) in &normalized {
        let duplicate = unsafe { libc::fcntl(*source, libc::F_DUPFD_CLOEXEC, temporary_floor) };
        if duplicate < 0 {
            return Err(format!(
                "duplicate mapped inherited descriptor {source}: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: F_DUPFD_CLOEXEC returned one new uniquely owned descriptor.
        source_copies.push(InheritedDescriptorAuthority::from_owned_file(
            unsafe { std::fs::File::from_raw_fd(duplicate) },
            &lease,
        )?);
    }

    let mut lifelines = Vec::with_capacity(source_copies.len() + normalized.len());
    let mut prepared = Vec::with_capacity(normalized.len());
    for ((_, target), source_copy) in normalized.into_iter().zip(source_copies) {
        let source = source_copy.file().as_raw_fd();
        let target_flags = unsafe { libc::fcntl(target, libc::F_GETFD) };
        if target_flags < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EBADF) {
                return Err(format!(
                    "inspect mapped target descriptor {target}: {error}"
                ));
            }
            // `Command` owns standard-input setup and protects its private
            // exec-error channel before our pre-exec hook runs. A closed
            // parent stdin therefore needs no process-global reservation;
            // the hook below replaces the child's configured fd 0 exactly.
            if target != libc::STDIN_FILENO {
                // F_DUPFD_CLOEXEC never overwrites a concurrently allocated
                // parent fd. A check followed by dup3 is not a reservation.
                let reservation = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, target) };
                if reservation < 0 {
                    return Err(format!(
                        "reserve mapped target descriptor {target}: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                let reservation_file = unsafe { std::fs::File::from_raw_fd(reservation) };
                if reservation != target {
                    return Err(format!(
                        "mapped target descriptor {target} was concurrently allocated"
                    ));
                }
                lifelines.push(InheritedDescriptorAuthority::from_owned_file(
                    reservation_file,
                    &lease,
                )?);
            }
        } else if target != libc::STDIN_FILENO && !sources.contains(&target) {
            return Err(format!(
                "mapped target descriptor {target} is occupied without request-owned authority"
            ));
        }
        prepared.push((source, target));
        lifelines.push(source_copy);
    }
    Ok(PreparedInheritedMappings {
        pairs: prepared,
        lifelines,
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
    #[cfg(target_os = "linux")] mut attachment_gate: Option<AttachmentWorkerGate>,
    #[cfg(not(target_os = "linux"))] _attachment_gate: Option<()>,
    process_scope: Option<crate::ProcessScope>,
    account: Option<&crate::ControllerAccount>,
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
        inherited_fd_mappings,
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
    let raw_inherited_fds = normalize_inherited_descriptors(&inherited_fds)
        .map_err(|error| spawn_failure(start, format!("Failed to spawn: {error}")))?;
    #[cfg(not(unix))]
    if !inherited_fds.is_empty() || !inherited_fd_mappings.is_empty() {
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

    #[cfg(target_os = "linux")]
    let forbidden_mapping_targets =
        inherited_mapping_control_descriptors(attachment_gate.as_ref(), supervised_status.as_ref());
    #[cfg(all(unix, not(target_os = "linux")))]
    let forbidden_mapping_targets =
        inherited_mapping_control_descriptors(supervised_status.as_ref());
    #[cfg(unix)]
    let prepared_mappings = {
        #[cfg(target_os = "linux")]
        let prepared = attachment_gate.as_mut().map(|gate| {
            gate.prepared_mappings
                .take()
                .expect("attachment mappings prepared before quiescence")
        });
        #[cfg(not(target_os = "linux"))]
        let prepared: Option<PreparedInheritedMappings> = None;
        match prepared {
            Some(prepared) => prepared,
            None => prepare_inherited_fd_mappings(
                &inherited_fd_mappings,
                &raw_inherited_fds,
                &forbidden_mapping_targets,
            )
            .map_err(|error| spawn_failure(start, format!("Failed to spawn: {error}")))?,
        }
    };
    #[cfg(unix)]
    for (_, target) in &prepared_mappings.pairs {
        if forbidden_mapping_targets.contains(target) {
            return Err(spawn_failure(
                start,
                format!("mapped target {target} aliases process-control authority"),
            ));
        }
    }
    #[cfg(unix)]
    let PreparedInheritedMappings {
        pairs: raw_inherited_fd_mappings,
        lifelines: inherited_fd_mapping_lifelines,
    } = prepared_mappings;
    #[cfg(unix)]
    let mapped_temporary_close_fds = {
        let targets: BTreeSet<_> = raw_inherited_fd_mappings
            .iter()
            .map(|(_, target)| *target)
            .collect();
        let mut copies: BTreeSet<_> = raw_inherited_fd_mappings
            .iter()
            .map(|(source, _)| *source)
            .collect();
        for mapping in &inherited_fd_mappings {
            copies.insert(mapping.source.file().as_raw_fd());
        }
        copies.retain(|fd| !targets.contains(fd) && !raw_inherited_fds.contains(fd));
        copies.into_iter().collect::<Vec<_>>()
    };

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
    // Placement must precede inherited-descriptor remapping, final attachment
    // hold and exec. Platform code owns the exact operation and closes its
    // controls at exec; they never enter workload channel/mount authority.
    if let Some(scope) = &process_scope {
        scope
            .configure_command(&mut command)
            .map_err(|error| spawn_failure(start, format!("configure process scope: {error}")))?;
    }
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
                        gate.cwd_directory,
                        gate.inherited_child_close_fds.clone(),
                    )
                });
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if !inherit_stdio {
                    preserve_configured_stdio_across_exec()?;
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
                for (source, target) in &raw_inherited_fd_mappings {
                    if libc::dup3(*source, *target, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // Temporary copies must not survive a durable pre-exec hold.
                // Final targets are deliberately excluded, including cycles.
                for fd in &mapped_temporary_close_fds {
                    libc::close(*fd);
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
                if !inherit_stdio {
                    preserve_configured_stdio_across_exec()?;
                }
                for fd in &raw_inherited_fds {
                    let flags = libc::fcntl(*fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (source, target) in &raw_inherited_fd_mappings {
                    if libc::dup2(*source, *target) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for fd in &mapped_temporary_close_fds {
                    libc::close(*fd);
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

    if let Some(account) = account {
        account
            .configure_command(&mut command)
            .map_err(|error| spawn_failure(start, error))?;
    }
    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Err(spawn_failure(start, format!("Failed to spawn: {e}"))),
    };
    #[cfg(unix)]
    drop(inherited_fd_mapping_lifelines);
    #[cfg(target_os = "linux")]
    drop(attachment_gate);
    // The forked child now owns its inherited descriptor copies. Close the
    // request-owned parent copies promptly: in particular, keeping the status
    // writer open here would hide a launcher's pre-target EOF and force every
    // failed setup to wait for the full supervision timeout.
    drop(inherited_fds);
    drop(inherited_fd_mappings);
    let wrapper_pid = child.id();

    // On Unix with setsid, pid == pgid since the child is its own process group leader.
    #[cfg(unix)]
    let wrapper_pgid = wrapper_pid as i64;
    #[cfg(not(unix))]
    let wrapper_pgid = -1i64;

    let stdout_capture = Arc::new(OutputCapture::default());
    let stderr_capture = Arc::new(OutputCapture::default());
    let drain_stop = Arc::new(AtomicBool::new(false));
    let (output_overflow_tx, output_overflow_rx) = std::sync::mpsc::channel();
    let (status_reader, attachment_release) = match supervised_status.map(|status| status.state) {
        Some(SupervisedProcessStatusState::Run { reader }) => (Some(reader), None),
        Some(SupervisedProcessStatusState::AwaitingAttachment {
            reader,
            attachment_release,
        }) => (Some(reader), Some(attachment_release)),
        None => (None, None),
    };
    // The wrapper is already an owned process, even before its target report.
    // Reuse that owner for every subsequent setup failure instead of reaping
    // the wrapper first and losing the exact process-group cleanup fence.
    let mut running = RunningProcess {
        process_scope,
        scope_cleanup_error: None,
        pid: wrapper_pid,
        pgid: wrapper_pgid,
        wrapper_pid,
        wrapper_pgid,
        child,
        stdin_thread: None,
        stdout_thread: None,
        stderr_thread: None,
        status_thread: None,
        stdout_capture,
        stderr_capture,
        stdout_reader_taken: false,
        drain_stop,
        output_overflow_rx,
        start,
        timeout,
        attachment_release,
        #[cfg(target_os = "linux")]
        occupancy_watchdog: None,
        groups_terminated: false,
        wrapper_reaped: false,
    };
    let (stdout_thread, stderr_thread) = if inherit_stdio {
        running.stdout_capture.state.lock().unwrap().closed = true;
        running.stderr_capture.state.lock().unwrap().closed = true;
        (thread::spawn(|| {}), thread::spawn(|| {}))
    } else {
        let mut stdout_handle = running
            .child
            .stdout
            .take()
            .expect("stdout configured as piped");
        let mut stderr_handle = running
            .child
            .stderr
            .take()
            .expect("stderr configured as piped");
        if let Err(error) = configure_nonblocking_fd(&mut stdout_handle)
            .and_then(|_| configure_nonblocking_fd(&mut stderr_handle))
        {
            return Err(running.into_spawn_failure(spawn_failure(
                start,
                format!("Failed to spawn: configure bounded output capture: {error}"),
            )));
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
                Arc::clone(&running.stdout_capture),
                Arc::clone(&running.drain_stop),
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
                Arc::clone(&running.stderr_capture),
                Arc::clone(&running.drain_stop),
                output_overflow_tx,
            ),
        )
    };
    running.stdout_thread = Some(stdout_thread);
    running.stderr_thread = Some(stderr_thread);

    // Never write request input on the spawning thread. A child can stop
    // reading before the pipe buffer is empty; the dedicated writer may then
    // wait on WouldBlock, but it observes the same cleanup flag as the bounded
    // drainers. The request deadline can therefore terminate and join every
    // pipe worker even when the child never consumes the remaining input.
    running.stdin_thread = match spawn_stdin_writer(
        running.child.stdin.take(),
        stdin_data,
        Arc::clone(&running.drain_stop),
    ) {
        Ok(thread) => thread,
        Err(error) => {
            return Err(running.into_spawn_failure(spawn_failure(
                start,
                format!("Failed to spawn: configure nonblocking stdin: {error}"),
            )));
        }
    };

    if let Some(reader) = status_reader {
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let status_thread = match spawn_supervised_launcher_status_reader(
            reader,
            status_tx,
            Arc::clone(&running.drain_stop),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                return Err(running.into_spawn_failure(spawn_failure(
                    start,
                    format!(
                        "Failed to spawn: initialize supervised-launcher status reader: {error}"
                    ),
                )));
            }
        };
        running.status_thread = Some(status_thread);
        let setup_deadline = supervised_setup_deadline(start, timeout);
        let setup_wait = setup_deadline.saturating_duration_since(Instant::now());
        let reported_pid = match status_rx.recv_timeout(setup_wait) {
            Ok(Ok(InitialLauncherStatus::Target(pid))) => pid,
            Ok(Ok(InitialLauncherStatus::Refused(diagnostic))) => {
                return Err(running
                    .into_spawn_failure(spawn_failure_with_launcher_refusal(start, diagnostic)));
            }
            Ok(Err(error)) => {
                let failure = spawn_failure(
                    start,
                    format!("Failed to spawn: supervised launcher refused: {error}"),
                );
                return Err(running.into_spawn_failure(failure));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let failure = spawn_failure(
                    start,
                    format!(
                        "Failed to spawn: supervised launcher did not report its target PID before the bounded setup/request deadline ({:.3} seconds remaining after launch setup)",
                        setup_wait.as_secs_f64()
                    ),
                );
                return Err(running.into_spawn_failure(failure));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let failure = spawn_failure(
                    start,
                    "Failed to spawn: supervised-launcher status channel closed before reporting its target PID",
                );
                return Err(running.into_spawn_failure(failure));
            }
        };
        let identity = match resolve_supervised_identity(reported_pid, wrapper_pid, wrapper_pgid) {
            Ok(identity) => identity,
            Err(error) => {
                return Err(running.into_spawn_failure(spawn_failure(
                    start,
                    format!("Failed to spawn: invalid supervised target identity: {error}"),
                )));
            }
        };
        running.pid = identity.pid;
        running.pgid = identity.pgid;
    }

    Ok(running)
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
                            let mut state = capture
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
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
                    let mut state = capture
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
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
                    drop(state);
                    capture.changed.notify_all();
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(CAPTURE_POLL_INTERVAL);
                }
                Err(error) => {
                    capture
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .read_error = Some(error.kind());
                    break;
                }
            }
        }
        capture
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed = true;
        capture.changed.notify_all();
    })
}

#[cfg(unix)]
fn configure_nonblocking_fd<T>(reader: &T) -> Result<(), String>
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
fn configure_nonblocking_fd<T>(_reader: &T) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn spawn_supervised_launcher_status_reader(
    reader: InheritedDescriptorAuthority,
    initial_tx: std::sync::mpsc::Sender<Result<InitialLauncherStatus, String>>,
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, String> {
    configure_nonblocking_fd(reader.file())
        .map_err(|error| format!("configure nonblocking status channel: {error}"))?;
    Ok(thread::spawn(move || {
        let mut initial_tx = Some(initial_tx);
        let mut pending = Vec::new();
        let mut buffer = [0u8; 4096];

        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            match reader.file().read(&mut buffer) {
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

#[cfg(not(unix))]
fn spawn_supervised_launcher_status_reader(
    _reader: InheritedDescriptorAuthority,
    _initial_tx: std::sync::mpsc::Sender<Result<InitialLauncherStatus, String>>,
    _stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, String> {
    Err("supervised launcher status descriptors are unsupported on this platform".to_owned())
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
    let (reader, writer) = unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    };
    // A previously adopted fd0 can leave parent stdin closed. Control
    // coordinates must not then occupy standard I/O: Command sets child
    // stdin before our hook closes the parent-only control descriptors.
    let reader = move_owned_descriptor_above_stdio(reader).map_err(|error| {
        spawn_failure(
            start,
            format!("Failed to relocate attachment {label} reader: {error}"),
        )
    })?;
    let writer = move_owned_descriptor_above_stdio(writer).map_err(|error| {
        spawn_failure(
            start,
            format!("Failed to relocate attachment {label} writer: {error}"),
        )
    })?;
    Ok((reader, writer))
}

#[cfg(target_os = "linux")]
fn open_attachment_cwd(
    path: &str,
    start: Instant,
) -> Result<InheritedDescriptorAuthority, SubprocessResult> {
    let path = std::ffi::CString::new(path).map_err(|_| {
        spawn_failure(
            start,
            "Failed to spawn awaiting attachment: cwd contains an interior NUL byte",
        )
    })?;
    let lease = retain_fork_sensitive_descriptors();
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
    InheritedDescriptorAuthority::from_owned_file(unsafe { std::fs::File::from_raw_fd(fd) }, &lease)
        .map_err(|error| spawn_failure(start, error))
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
    inherited_child_close_fds: &[i32],
) -> std::io::Result<()> {
    unsafe {
        libc::close(status_reader_fd);
        libc::close(release_writer_fd);
        for fd in inherited_child_close_fds {
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
    fn release_cleanup_requires_both_process_and_scope_proof() {
        for process_settled in [false, true] {
            for scope_settled in [false, true] {
                let process = process_settled
                    .then_some(())
                    .ok_or_else(|| "wrapper".to_owned());
                let scope = scope_settled
                    .then_some(())
                    .ok_or_else(|| "scope".to_owned());
                let (detail, settled) = super::scoped_release_cleanup_outcome(process, scope);
                let error = super::AttachmentReleaseError {
                    phase: "release after attachment",
                    result: super::spawn_failure(super::Instant::now(), detail),
                    cleanup_is_settled: settled,
                };
                assert_eq!(error.cleanup_is_settled(), process_settled && scope_settled);
            }
        }
    }

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
    if let Some(expected_parent) = expected_parent
        && observed_after_pin.parent_pid != expected_parent
    {
        return Err(format!(
            "process {pid} parent changed before identity pin (expected {expected_parent}, observed {})",
            observed_after_pin.parent_pid
        ));
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
    scope: Option<&crate::ProcessScope>,
) -> Result<(), String> {
    // A panicked spawn thread does not change the admitted lifecycle owner.
    // In particular, scope cleanup must never fall through to the old group
    // signal/scan path merely because RunningProcess was not returned.
    if scope.is_none() {
        kill_owned_process_group(pid, pgid, true);
    }
    let signal = match pidfd_send_signal_io(pidfd, ATTACHMENT_ABORT_SIGNAL) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        Err(error) => Err(format!(
            "pidfd_send_signal({ATTACHMENT_ABORT_SIGNAL}): {error}"
        )),
    };
    let group = match scope {
        Some(scope) => scope.terminate_and_wait(ATTACHMENT_ABORT_SETTLE_TIMEOUT),
        None => wait_owned_process_group_quiescent(pgid, pid, ATTACHMENT_ABORT_SETTLE_TIMEOUT),
    };
    let exit = wait_pidfd_exit(pidfd, ATTACHMENT_ABORT_SETTLE_TIMEOUT);
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

fn take_capture(capture: &SharedCapture) -> BoundedCapture {
    // Drainers have been joined before settlement. A still-live byte reader
    // must retain its bounded bytes/EOF even if the process settles first.
    // Only that case needs a bounded snapshot for the ordinary text result;
    // no observer queue or additional capture thread is introduced.
    let has_reader = Arc::strong_count(capture) > 1;
    let mut state = capture
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if has_reader {
        state.clone()
    } else {
        std::mem::take(&mut *state)
    }
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

fn append_captured_stderr(reason: String, capture: &BoundedCapture) -> String {
    const DIAGNOSTIC_BYTES: usize = 4 * 1024;
    if capture.bytes.is_empty() {
        return reason;
    }
    let diagnostic = if capture.bytes.len() > DIAGNOSTIC_BYTES {
        let half = DIAGNOSTIC_BYTES / 2;
        let head = String::from_utf8_lossy(&capture.bytes[..half]);
        let tail = String::from_utf8_lossy(&capture.bytes[capture.bytes.len() - half..]);
        format!("{head}\n… (bounded launcher stderr; middle bytes omitted) …\n{tail}")
    } else {
        let body = String::from_utf8_lossy(&capture.bytes);
        if capture.truncated {
            format!("{body}\n… (launcher stderr exceeded its capture limit)")
        } else {
            body.into_owned()
        }
    };
    append_diagnostic(&reason, &diagnostic)
}

pub(crate) fn spawn_failure(start: Instant, reason: impl Into<String>) -> SubprocessResult {
    SubprocessResult {
        success: false,
        stdout: String::new(),
        stderr: reason.into(),
        exit_code: -1,
        duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        pid: 0,
        timed_out: false,
        launcher_refusal: None,
        aborted_before_attachment: None,
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
        aborted_before_attachment: None,
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
        validated_rlimits(limits).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        if limits.is_some_and(|limits| {
            limits.max_open_files.is_some()
                || limits.max_address_space_bytes.is_some()
                || limits.max_cpu_seconds.is_some()
                || limits.max_processes.is_some()
        }) {
            return Err("subprocess kernel limits are unsupported on this platform".to_string());
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

        let installed = validated_rlimits(limits)?;
        if installed.iter().any(Option::is_some) {
            unsafe {
                command.pre_exec(move || {
                    for (resource, value) in [
                        (libc::RLIMIT_NOFILE, installed[0]),
                        (libc::RLIMIT_AS, installed[1]),
                        (libc::RLIMIT_CPU, installed[2]),
                        (libc::RLIMIT_NPROC, installed[3]),
                    ] {
                        if let Some(value) = value {
                            let limit = libc::rlimit {
                                rlim_cur: value,
                                rlim_max: value,
                            };
                            if libc::setrlimit(resource, &limit) != 0 {
                                return Err(std::io::Error::last_os_error());
                            }
                        }
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
fn validated_rlimits(
    limits: Option<&SubprocessLimits>,
) -> Result<[Option<libc::rlim_t>; 4], String> {
    let values = limits.map_or([None; 4], |limits| {
        [
            limits.max_open_files,
            limits.max_address_space_bytes,
            limits.max_cpu_seconds,
            limits.max_processes,
        ]
    });
    let names = [
        "max_open_files",
        "max_address_space_bytes",
        "max_cpu_seconds",
        "max_processes",
    ];
    let resources = [
        libc::RLIMIT_NOFILE,
        libc::RLIMIT_AS,
        libc::RLIMIT_CPU,
        libc::RLIMIT_NPROC,
    ];
    let mut output = [None; 4];
    for index in 0..values.len() {
        let Some(value) = values[index] else { continue };
        if value == 0 {
            return Err(format!("{} must be positive", names[index]));
        }
        let platform_limit = value as libc::rlim_t;
        if platform_limit as u128 != value as u128 || platform_limit == libc::RLIM_INFINITY {
            return Err(format!(
                "{} {value} must be finite and representable",
                names[index]
            ));
        }
        let mut parent_limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(resources[index], &mut parent_limit) } != 0 {
            return Err(format!(
                "failed to inspect parent {} limit: {}",
                names[index],
                std::io::Error::last_os_error()
            ));
        }
        if parent_limit.rlim_max != libc::RLIM_INFINITY && platform_limit > parent_limit.rlim_max {
            return Err(format!(
                "{} {value} exceeds parent hard limit {}",
                names[index], parent_limit.rlim_max
            ));
        }
        output[index] = Some(platform_limit);
    }
    Ok(output)
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

/// Administrator-owned maintenance subprocess, using the same bounded output,
/// deadline and exact child cleanup as ordinary execution. Account selection is
/// not a serializable workload request field and cannot come from a worker.
pub fn lib_run_as_account(
    request: SubprocessRequest,
    account: &crate::ControllerAccount,
) -> SubprocessResult {
    if request.supervised_status.is_some() {
        return spawn_failure(
            Instant::now(),
            "maintenance account execution cannot carry worker supervision",
        );
    }
    match lib_spawn_with_stdio(request, false, None, None, Some(account)) {
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
    /// Host-supervisor bootstrap: provision one explicit delegation and exec
    /// an unprivileged controller. Not a worker command or setuid entrypoint.
    ScopeController {
        #[arg(long)]
        configuration: String,
        #[arg(long)]
        uid: u32,
        #[arg(long)]
        gid: u32,
        #[arg(long)]
        cmd: std::path::PathBuf,
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long)]
        cwd: std::path::PathBuf,
        #[arg(long = "env")]
        envs: Vec<String>,
    },
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
    if let Some(data) = data
        && let Some(mut s) = child.stdin.take()
    {
        let _ = s.write_all(data.as_bytes());
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
        ExecAction::ScopeController {
            configuration,
            uid,
            gid,
            cmd,
            args,
            cwd,
            envs,
        } => {
            let outcome = (|| -> Result<std::convert::Infallible, String> {
                let configuration: crate::ProcessScopeConfiguration =
                    serde_json::from_str(&configuration).map_err(|error| error.to_string())?;
                let environment: Vec<(String, String)> = envs
                    .iter()
                    .map(|entry| {
                        entry
                            .split_once('=')
                            .map(|(key, value)| (key.to_owned(), value.to_owned()))
                            .ok_or_else(|| "controller environment requires NAME=VALUE".to_owned())
                    })
                    .collect::<Result<_, _>>()?;
                let cwd = crate::PinnedDirectory::open(&cwd)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "controller working directory is absent".to_owned())?;
                if !cmd.is_absolute() {
                    return Err(
                        "controller executable must be an explicit absolute path".to_owned()
                    );
                }
                let executable_parent = crate::PinnedDirectory::open(
                    cmd.parent().ok_or("controller executable has no parent")?,
                )
                .map_err(|error| error.to_string())?
                .ok_or("controller executable directory is absent")?;
                let executable = executable_parent
                    .open_pinned_regular(
                        cmd.file_name()
                            .ok_or("controller executable has no filename")?,
                        false,
                    )
                    .map_err(|error| error.to_string())?
                    .ok_or("controller executable is absent")?;
                configuration.exec_controller(
                    &crate::ControllerAccount::unix(uid, gid),
                    &executable,
                    &args,
                    &cwd,
                    &environment,
                )
            })();
            match outcome {
                Ok(never) => match never {},
                Err(error) => serde_json::json!({"error": error}),
            }
        }
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
        inherited_fd_mappings: Vec::new(),
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
