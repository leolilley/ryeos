use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use crate::env_contract::BASE_ALLOWLIST_NAMES;

/// Poll interval (ms) when waiting for a process group to exit after SIGTERM.
const KILL_POLL_INTERVAL_MS: u64 = 100;

/// Wait time (ms) after SIGKILL before checking if the process group is dead.
const POST_SIGKILL_WAIT_MS: u64 = 200;

/// Workload-authored cancellation policy cannot control daemon availability.
/// Grace remains cooperative, but the node owns this hard upper bound before
/// escalating the exact process group.
pub const MAX_GRACEFUL_SHUTDOWN_GRACE_SECS: u64 = 5;

pub const PROCESS_IDENTITY_SCHEMA_VERSION: u32 = 1;

/// Durable identity for the exact target and its retained process-group leader.
///
/// Numeric PIDs/PGIDs are not identities: the kernel may recycle them after a
/// process is reaped. `/proc` start time distinguishes process incarnations
/// within one boot, and `boot_id` prevents a false match after restart. Signal
/// paths additionally pin the verified incarnation with a pidfd before using
/// either numeric ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProcessIdentity {
    pub schema_version: u32,
    pub boot_id: String,
    pub target_pid: i64,
    pub target_start_time_ticks: i64,
    /// The process-group leader PID, equal to the signalable PGID.
    pub group_leader_pid: i64,
    pub group_leader_start_time_ticks: i64,
}

impl ExecutionProcessIdentity {
    pub fn pgid(&self) -> i64 {
        self.group_leader_pid
    }
}

pub fn validate_execution_process_identity_shape(
    identity: &ExecutionProcessIdentity,
) -> Result<()> {
    if identity.schema_version != PROCESS_IDENTITY_SCHEMA_VERSION {
        anyhow::bail!("unsupported process identity schema version");
    }
    if identity.boot_id.is_empty()
        || identity.target_start_time_ticks <= 0
        || identity.group_leader_start_time_ticks <= 0
    {
        anyhow::bail!("incomplete process identity birth fields");
    }
    validate_pid(identity.target_pid).context("invalid process identity target PID")?;
    validate_pid(identity.group_leader_pid).context("invalid process identity group-leader PID")?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ProcessStat {
    pgrp: i64,
    start_time_ticks: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityPinError {
    AlreadyDead,
    Stale,
    Unavailable,
}

/// Why durable identity capture failed.
///
/// Callers may recover only from [`Self::TargetAlreadyDeadOrStale`], which
/// means the specifically requested target vanished or changed while it was
/// being pinned. Every host-capability, procfs, group-leader, or shape failure
/// remains an opaque hard failure so callers cannot accidentally weaken the
/// exact-process proof.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionProcessIdentityCaptureError {
    #[error("target process exited or changed during identity capture")]
    TargetAlreadyDeadOrStale,
    #[error(transparent)]
    CaptureFailed(#[from] anyhow::Error),
}

#[cfg(target_os = "linux")]
struct PinnedProcess {
    pidfd: OwnedFd,
}

pub fn remove_stale_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect daemon socket {}", path.display()))
        }
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to replace non-socket daemon control path {}",
            path.display()
        );
    }

    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => bail!(
            "refusing to replace live daemon control socket {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            // Re-check the directory entry after the liveness probe. A process
            // may have replaced the refused inode while we were connecting;
            // unlinking that replacement would orphan its live listener.
            let current = match std::fs::symlink_metadata(path) {
                Ok(current) => current,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reinspect daemon socket {}", path.display()))
                }
            };
            if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
                bail!(
                    "daemon control socket changed during stale probe at {}; refusing replacement",
                    path.display()
                );
            }
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "daemon socket ownership is uncertain at {}; refusing replacement",
                path.display()
            )
        }),
    }
}

/// Check if a process group is alive.
pub fn pgid_alive(pgid: i64) -> bool {
    // kill(0, -pgid) checks if any process in the group exists
    unsafe { libc::kill(-(pgid as i32), 0) == 0 }
}

/// Prove that a persisted PID/PGID still identifies the RyeOS runtime for the
/// expected thread before restart recovery sends any signal to it.
///
/// A bare numeric PID or process-group ID is not an identity: Linux may reuse
/// it after the recorded process exits. Every RyeOS runtime is launched with a
/// thread-id protocol binding, so recovery verifies that binding in the
/// process's original environment and also verifies the process still belongs
/// to the recorded group. An unreadable or mismatched identity fails closed;
/// callers must never signal the numeric group in that case.
#[cfg(target_os = "linux")]
pub fn thread_process_identity_matches(pid: i64, pgid: i64, thread_id: &str) -> Result<bool> {
    let pid = i32::try_from(pid).context("persisted runtime pid is out of range")?;
    let pgid = i32::try_from(pgid).context("persisted runtime pgid is out of range")?;
    if pid <= 0 || pgid <= 0 {
        anyhow::bail!("persisted runtime pid and pgid must be positive");
    }
    if pid == std::process::id() as i32 || i64::from(pgid) == daemon_pgid() {
        anyhow::bail!("persisted runtime identity aliases the daemon process group");
    }
    if thread_id.is_empty() || thread_id.as_bytes().contains(&0) {
        anyhow::bail!("expected runtime thread id is empty or contains NUL");
    }

    let actual_pgid = unsafe { libc::getpgid(pid) };
    if actual_pgid < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(error)
            .with_context(|| format!("inspect process group for persisted runtime pid {pid}"));
    }
    if actual_pgid != pgid {
        return Ok(false);
    }

    let environ_path = std::path::PathBuf::from(format!("/proc/{pid}/environ"));
    let environ = match std::fs::read(&environ_path) {
        Ok(environ) => environ,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read runtime identity from {}", environ_path.display()))
        }
    };
    let expected_uds = format!("RYEOSD_THREAD_ID={thread_id}");
    let expected_engine = format!("RYEOS_THREAD_ID={thread_id}");
    let matches = environ
        .split(|byte| *byte == 0)
        .any(|entry| entry == expected_uds.as_bytes() || entry == expected_engine.as_bytes());
    Ok(matches)
}

#[cfg(not(target_os = "linux"))]
pub fn thread_process_identity_matches(_pid: i64, _pgid: i64, _thread_id: &str) -> Result<bool> {
    anyhow::bail!("restart process identity verification is unsupported on this platform")
}

pub fn verify_thread_process_identity(pid: i64, pgid: i64, thread_id: &str) -> Result<()> {
    if thread_process_identity_matches(pid, pgid, thread_id)? {
        return Ok(());
    }
    anyhow::bail!(
        "persisted runtime pid {pid}/pgid {pgid} does not carry the expected RyeOS thread identity"
    )
}

/// Fail-closed process-group liveness for retention decisions.
///
/// Unlike the hot-path boolean probe, only `ESRCH` proves absence. Permission
/// denial still proves that an identity exists, while malformed identifiers,
/// the daemon's own group, and unexpected OS failures make the inspection
/// indeterminate and therefore abort retirement.
pub fn pgid_live_for_retention(pgid: i64) -> Result<bool> {
    let pgid = i32::try_from(pgid).context("persisted process-group id is out of range")?;
    if pgid <= 0 {
        anyhow::bail!("persisted process-group id must be positive");
    }
    if i64::from(pgid) == daemon_pgid() {
        anyhow::bail!("persisted runtime process group aliases the daemon process group");
    }
    probe_identity(-pgid, "process group")
}

/// PID counterpart used when a runtime row has not recorded a process group.
pub fn pid_live_for_retention(pid: i64) -> Result<bool> {
    let pid = i32::try_from(pid).context("persisted process id is out of range")?;
    if pid <= 0 {
        anyhow::bail!("persisted process id must be positive");
    }
    if pid == std::process::id() as i32 {
        anyhow::bail!("persisted runtime process id aliases the daemon process");
    }
    probe_identity(pid, "process")
}

fn probe_identity(signal_target: i32, label: &str) -> Result<bool> {
    if unsafe { libc::kill(signal_target, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).with_context(|| format!("inspect persisted {label} liveness")),
    }
}
/// Return the daemon's own process group ID.
pub fn daemon_pgid() -> i64 {
    unsafe { libc::getpgid(0) as i64 }
}

/// Probe the exact kernel primitives required by durable runtime attachment and
/// race-free process-group cancellation. RyeOS intentionally has no numeric-PID
/// fallback: a node without these capabilities must fail before launching work.
pub fn validate_durable_process_control_support() -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("durable process control requires Linux 6.9 or newer");

    #[cfg(target_os = "linux")]
    {
        let self_pid = unsafe { libc::getpid() };
        let self_pgid = unsafe { libc::getpgrp() };
        if self_pgid != self_pid {
            // PIDFD_SIGNAL_PROCESS_GROUP is valid only when the pidfd refers
            // to a process-group leader. Lifecycle launchers do not guarantee
            // that topology, so isolate ryeosd before probing or supervising
            // any workload groups.
            if unsafe { libc::setpgid(0, 0) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("place ryeosd in its own process group");
            }
            let isolated_pgid = unsafe { libc::getpgrp() };
            if isolated_pgid != self_pid {
                anyhow::bail!(
                    "ryeosd process-group isolation returned PGID {isolated_pgid}, expected {self_pid}"
                );
            }
        }
        let raw_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, self_pid, 0u32) } as i32;
        if raw_pidfd < 0 {
            return Err(std::io::Error::last_os_error()).context("pidfd_open is unavailable");
        }
        // SAFETY: pidfd_open returned a new owned descriptor.
        let self_pidfd = unsafe { OwnedFd::from_raw_fd(raw_pidfd) };
        if pidfd_send_signal_raw(self_pidfd.as_raw_fd(), 0, libc::PIDFD_SIGNAL_PROCESS_GROUP)
            != SignalResult::Delivered
        {
            anyhow::bail!(
                "PIDFD_SIGNAL_PROCESS_GROUP is unavailable; RyeOS requires Linux 6.9 or newer"
            );
        }

        let (peer, _other) = std::os::unix::net::UnixStream::pair()
            .context("create Unix socket pair for SO_PEERPIDFD probe")?;
        let mut peer_pidfd: libc::c_int = -1;
        let mut value_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                peer.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut peer_pidfd as *mut libc::c_int).cast(),
                &mut value_len,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("SO_PEERPIDFD is unavailable; RyeOS requires Linux 6.9 or newer");
        }
        if value_len as usize != std::mem::size_of::<libc::c_int>() || peer_pidfd < 0 {
            anyhow::bail!("SO_PEERPIDFD returned an invalid descriptor");
        }
        // SAFETY: successful SO_PEERPIDFD installed a new descriptor.
        drop(unsafe { OwnedFd::from_raw_fd(peer_pidfd) });
        Ok(())
    }
}

/// Capture the exact target and group-leader incarnations before persisting a
/// runtime attachment. The expected PGID is mandatory for in-process spawns;
/// UDS self-attach passes `None` and accepts the kernel-derived group.
pub fn capture_execution_process_identity(
    target_pid: i64,
    expected_pgid: Option<i64>,
) -> std::result::Result<ExecutionProcessIdentity, ExecutionProcessIdentityCaptureError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (target_pid, expected_pgid);
        return Err(ExecutionProcessIdentityCaptureError::CaptureFailed(
            anyhow::anyhow!("durable process identity requires Linux pidfds and procfs"),
        ));
    }

    #[cfg(target_os = "linux")]
    {
        validate_pid(target_pid)
            .context("invalid target PID")
            .map_err(ExecutionProcessIdentityCaptureError::CaptureFailed)?;
        let target_pin = open_pidfd(target_pid).map_err(target_identity_capture_error)?;
        capture_execution_process_identity_from_pin(
            target_pid,
            expected_pgid,
            target_pin.pidfd.as_fd(),
        )
    }
}

/// Capture a runtime identity from a kernel-authenticated pidfd, such as an
/// `SO_PEERPIDFD` obtained from the accepted Unix socket. The pidfd remains the
/// authority; the numeric PID is used only to read the matching procfs record.
#[cfg(target_os = "linux")]
pub fn capture_execution_process_identity_from_pidfd(
    target_pid: i64,
    expected_pgid: Option<i64>,
    target_pidfd: BorrowedFd<'_>,
) -> std::result::Result<ExecutionProcessIdentity, ExecutionProcessIdentityCaptureError> {
    validate_pid(target_pid)
        .context("invalid target PID")
        .map_err(ExecutionProcessIdentityCaptureError::CaptureFailed)?;
    capture_execution_process_identity_from_pin(target_pid, expected_pgid, target_pidfd)
}

#[cfg(target_os = "linux")]
fn capture_execution_process_identity_from_pin(
    target_pid: i64,
    expected_pgid: Option<i64>,
    target_pidfd: BorrowedFd<'_>,
) -> std::result::Result<ExecutionProcessIdentity, ExecutionProcessIdentityCaptureError> {
    let target_stat = read_verified_process_stat(target_pid, target_pidfd)
        .map_err(target_identity_capture_error)?;
    let pgid = expected_pgid.unwrap_or(target_stat.pgrp);
    validate_pid(pgid)
        .context("invalid process-group leader PID")
        .map_err(ExecutionProcessIdentityCaptureError::CaptureFailed)?;
    if target_stat.pgrp != pgid {
        return Err(ExecutionProcessIdentityCaptureError::CaptureFailed(
            anyhow::anyhow!(
                "target PID {target_pid} belongs to process group {}, expected {pgid}",
                target_stat.pgrp
            ),
        ));
    }
    if pgid == daemon_pgid() {
        return Err(ExecutionProcessIdentityCaptureError::CaptureFailed(
            anyhow::anyhow!("refusing daemon process group {pgid} as a runtime identity"),
        ));
    }

    let group_pin = open_pidfd(pgid).map_err(non_target_identity_capture_error)?;
    let group_stat = read_verified_process_stat(pgid, group_pin.pidfd.as_fd())
        .map_err(non_target_identity_capture_error)?;
    if group_stat.pgrp != pgid {
        return Err(ExecutionProcessIdentityCaptureError::CaptureFailed(
            anyhow::anyhow!("process-group identity {pgid} is not its current group leader"),
        ));
    }
    let boot_id = read_boot_id()
        .context("read kernel boot identity")
        .map_err(ExecutionProcessIdentityCaptureError::CaptureFailed)?;

    // Re-probe both pidfds after collecting every numeric/procfs field. If a
    // process exited and its numeric ID was recycled during capture, the old
    // pidfd reports ESRCH and the attachment fails rather than persisting the
    // replacement's metadata.
    require_live_pidfd(target_pidfd).map_err(target_identity_capture_error)?;
    require_live_pidfd(group_pin.pidfd.as_fd()).map_err(non_target_identity_capture_error)?;

    Ok(ExecutionProcessIdentity {
        schema_version: PROCESS_IDENTITY_SCHEMA_VERSION,
        boot_id,
        target_pid,
        target_start_time_ticks: target_stat.start_time_ticks,
        group_leader_pid: pgid,
        group_leader_start_time_ticks: group_stat.start_time_ticks,
    })
}

pub struct KillResult {
    pub success: bool,
    pub method: &'static str,
}

/// Policy for shutting down a thread's process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownAction {
    /// SIGKILL immediately — no SIGTERM, no grace period.
    Hard,
    /// SIGTERM first, then SIGKILL after `grace` if the group survives.
    Graceful(std::time::Duration),
}

/// Map a tool-declared `CancellationMode` to a `ShutdownAction`.
///
/// - `Some(Hard)` → SIGKILL only.
/// - `Some(Graceful { grace_secs })` → SIGTERM, wait up to the node-owned cap,
///   then SIGKILL.
/// - `None` → default 3-second graceful.
pub fn resolve_shutdown_action(
    mode: Option<ryeos_engine::contracts::CancellationMode>,
) -> ShutdownAction {
    use ryeos_engine::contracts::CancellationMode;
    match mode {
        Some(CancellationMode::Hard) => ShutdownAction::Hard,
        Some(CancellationMode::Graceful { grace_secs }) => ShutdownAction::Graceful(
            std::time::Duration::from_secs(grace_secs.min(MAX_GRACEFUL_SHUTDOWN_GRACE_SECS)),
        ),
        None => ShutdownAction::Graceful(std::time::Duration::from_secs(3)),
    }
}

/// Terminate one exact execution without ever signalling a recycled PID/PGID.
///
/// Graceful shutdown signals only the reported target. This leaves the retained
/// Bubblewrap group leader alive while the target uses its declared grace. If
/// the target/group survives the deadline, the already-pinned group is killed.
pub fn kill_by_action(identity: &ExecutionProcessIdentity, action: ShutdownAction) -> KillResult {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (identity, action);
        return KillResult {
            success: false,
            method: "identity_unavailable",
        };
    }

    #[cfg(target_os = "linux")]
    {
        if identity.group_leader_pid == daemon_pgid() {
            return KillResult {
                success: false,
                method: "skipped_daemon_pgid",
            };
        }
        let group = match pin_group_leader(identity) {
            Ok(group) => group,
            Err(error) => return group_pin_failure(identity, error),
        };
        match action {
            ShutdownAction::Hard => hard_kill_pinned_group(identity, &group, "hard_killed"),
            ShutdownAction::Graceful(grace) => graceful_kill(identity, &group, grace),
        }
    }
}

/// Outcome of delivering a one-shot signal to an exact execution identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalResult {
    /// The signal was delivered to the verified target or group.
    Delivered,
    /// No usable process group was recorded (`pgid <= 0`). Retained for callers
    /// that distinguish an unattached runtime from an incomplete identity.
    MissingPgid,
    /// The runtime row predates durable process identities or is incomplete.
    MissingIdentity,
    /// The verified target/group no longer exists — the runtime already exited.
    AlreadyDead,
    /// Refused: `pgid` is the daemon's own group; signalling it would hit the
    /// daemon (and `kill(-0, …)` signals the *caller's* group, which is why
    /// `pgid == 0` is rejected as `MissingPgid` above).
    SkippedDaemonPgid,
    /// The numeric ID now belongs to a different process incarnation.
    StaleIdentity,
    /// The host cannot provide the pidfd/procfs primitives needed to prove the
    /// identity. Signalling fails closed.
    IdentityUnavailable,
    /// Signal delivery failed for some other reason.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityLiveness {
    Alive,
    /// The exact incarnation no longer occupies the persisted identity.
    DeadOrStale,
    /// The host could not prove either state; callers must fail closed.
    Unavailable,
}

impl SignalResult {
    /// A stable wire/log tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::MissingPgid => "missing_pgid",
            Self::MissingIdentity => "missing_identity",
            Self::AlreadyDead => "already_dead",
            Self::SkippedDaemonPgid => "skipped_daemon_pgid",
            Self::StaleIdentity => "stale_identity",
            Self::IdentityUnavailable => "identity_unavailable",
            Self::Failed => "failed",
        }
    }
}

/// Deliver the live-intervention nudge (`SIGUSR1`) to the exact runtime target.
/// Its handler sets the interrupt flag, cutting the in-flight cognition so the
/// queued input folds into a fresh one.
pub fn interrupt_process(identity: &ExecutionProcessIdentity) -> SignalResult {
    signal_exact_target(identity, libc::SIGUSR1)
}

/// Send one signal to the exact target process through a verified pidfd.
pub fn signal_exact_target(identity: &ExecutionProcessIdentity, signal: i32) -> SignalResult {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (identity, signal);
        return SignalResult::IdentityUnavailable;
    }

    #[cfg(target_os = "linux")]
    {
        if identity.group_leader_pid == daemon_pgid() {
            return SignalResult::SkippedDaemonPgid;
        }
        let target = match pin_target(identity) {
            Ok(target) => target,
            Err(error) => return signal_pin_failure(error),
        };
        pidfd_send_signal(&target, signal)
    }
}

/// Send one signal to the exact, verified process group. Used by cascade hard
/// cancellation; graceful cascade targets only the runtime PID.
pub fn signal_exact_group(identity: &ExecutionProcessIdentity, signal: i32) -> SignalResult {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (identity, signal);
        return SignalResult::IdentityUnavailable;
    }

    #[cfg(target_os = "linux")]
    {
        if identity.group_leader_pid == daemon_pgid() {
            return SignalResult::SkippedDaemonPgid;
        }
        let group = match pin_group_leader(identity) {
            Ok(group) => group,
            Err(error) => return signal_pin_failure(error),
        };
        signal_pinned_group(&group, signal)
    }
}

/// Whether the persisted target still names the exact live incarnation.
pub fn execution_alive(identity: &ExecutionProcessIdentity) -> bool {
    execution_liveness(identity) == IdentityLiveness::Alive
}

pub fn execution_liveness(identity: &ExecutionProcessIdentity) -> IdentityLiveness {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = identity;
        IdentityLiveness::Unavailable
    }

    #[cfg(target_os = "linux")]
    {
        pin_liveness(pin_target(identity))
    }
}

/// Whether the retained wrapper still names the exact live group identity.
///
/// This is stronger than target liveness for duplicate-launch guards: the
/// target may have exited while Lillux still owns group cleanup and foldback.
pub fn execution_group_alive(identity: &ExecutionProcessIdentity) -> bool {
    execution_group_liveness(identity) == IdentityLiveness::Alive
}

pub fn execution_group_liveness(identity: &ExecutionProcessIdentity) -> IdentityLiveness {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = identity;
        IdentityLiveness::Unavailable
    }

    #[cfg(target_os = "linux")]
    {
        pin_liveness(pin_group_leader(identity))
    }
}

/// Whether an identity was captured during the current kernel boot.
///
/// A dead group leader from an earlier boot is safe to clear: no process can
/// survive the reboot. On the same boot, leader death does not prove the whole
/// process group is empty, so reconcile must quarantine rather than relaunch.
pub fn execution_identity_is_current_boot(identity: &ExecutionProcessIdentity) -> Result<bool> {
    validate_execution_process_identity_shape(identity)?;
    Ok(read_boot_id()? == identity.boot_id)
}

#[cfg(target_os = "linux")]
fn pin_liveness(result: std::result::Result<PinnedProcess, IdentityPinError>) -> IdentityLiveness {
    match result {
        Ok(_) => IdentityLiveness::Alive,
        Err(IdentityPinError::AlreadyDead | IdentityPinError::Stale) => {
            IdentityLiveness::DeadOrStale
        }
        Err(IdentityPinError::Unavailable) => IdentityLiveness::Unavailable,
    }
}

fn validate_pid(pid: i64) -> Result<()> {
    if pid <= 1 || pid > i32::MAX as i64 {
        anyhow::bail!("unsafe host PID {pid}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: i64) -> std::result::Result<PinnedProcess, IdentityPinError> {
    validate_pid(pid).map_err(|_| IdentityPinError::Stale)?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) } as i32;
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ESRCH) => Err(IdentityPinError::AlreadyDead),
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EPERM) => {
                Err(IdentityPinError::Unavailable)
            }
            _ => Err(IdentityPinError::Unavailable),
        };
    }
    // SAFETY: pidfd_open returned a new owned descriptor on success.
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(PinnedProcess { pidfd })
}

fn read_boot_id() -> std::io::Result<String> {
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let value = value.trim();
    if value.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty kernel boot_id",
        ));
    }
    Ok(value.to_string())
}

fn read_process_stat(pid: i64) -> std::io::Result<ProcessStat> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // `comm` is parenthesized and may itself contain spaces or `)`. Split at
    // the final close-paren; the remaining fields then start at field 3.
    let close = raw.rfind(')').ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed /proc stat comm")
    })?;
    let fields = raw[close + 1..].split_whitespace().collect::<Vec<_>>();
    let pgrp = fields
        .get(2)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing /proc stat pgrp")
        })?
        .parse::<i64>()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let start_time_ticks = fields
        .get(19)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing /proc stat starttime",
            )
        })?
        .parse::<i64>()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if start_time_ticks <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "non-positive /proc stat starttime",
        ));
    }
    Ok(ProcessStat {
        pgrp,
        start_time_ticks,
    })
}

#[cfg(target_os = "linux")]
fn validate_identity_shape(
    identity: &ExecutionProcessIdentity,
) -> std::result::Result<(), IdentityPinError> {
    validate_execution_process_identity_shape(identity).map_err(|_| IdentityPinError::Stale)
}

#[cfg(target_os = "linux")]
fn require_live_pidfd(pidfd: BorrowedFd<'_>) -> std::result::Result<(), IdentityPinError> {
    match pidfd_send_signal_raw(pidfd.as_raw_fd(), 0, 0) {
        SignalResult::Delivered => Ok(()),
        SignalResult::AlreadyDead => Err(IdentityPinError::AlreadyDead),
        SignalResult::StaleIdentity => Err(IdentityPinError::Stale),
        _ => Err(IdentityPinError::Unavailable),
    }
}

#[cfg(target_os = "linux")]
fn read_verified_process_stat(
    pid: i64,
    pidfd: BorrowedFd<'_>,
) -> std::result::Result<ProcessStat, IdentityPinError> {
    let stat = read_process_stat(pid).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            IdentityPinError::AlreadyDead
        } else {
            IdentityPinError::Unavailable
        }
    })?;
    // Probe *after* the numeric procfs lookup. If the pidfd's task exited and
    // the number was recycled before/during that lookup, the old pidfd returns
    // ESRCH and the replacement's stat record is discarded.
    require_live_pidfd(pidfd)?;
    Ok(stat)
}

#[cfg(target_os = "linux")]
fn pin_process(
    identity: &ExecutionProcessIdentity,
    pid: i64,
    expected_start_time_ticks: i64,
    expected_pgrp: i64,
) -> std::result::Result<PinnedProcess, IdentityPinError> {
    validate_identity_shape(identity)?;
    let pin = open_pidfd(pid)?;
    let boot_id = read_boot_id().map_err(|_| IdentityPinError::Unavailable)?;
    if boot_id != identity.boot_id {
        return Err(IdentityPinError::Stale);
    }
    let stat = read_verified_process_stat(pid, pin.pidfd.as_fd())?;
    if stat.start_time_ticks != expected_start_time_ticks || stat.pgrp != expected_pgrp {
        return Err(IdentityPinError::Stale);
    }
    Ok(pin)
}

#[cfg(target_os = "linux")]
fn pin_target(
    identity: &ExecutionProcessIdentity,
) -> std::result::Result<PinnedProcess, IdentityPinError> {
    pin_process(
        identity,
        identity.target_pid,
        identity.target_start_time_ticks,
        identity.group_leader_pid,
    )
}

#[cfg(target_os = "linux")]
fn pin_group_leader(
    identity: &ExecutionProcessIdentity,
) -> std::result::Result<PinnedProcess, IdentityPinError> {
    pin_process(
        identity,
        identity.group_leader_pid,
        identity.group_leader_start_time_ticks,
        identity.group_leader_pid,
    )
}

fn identity_capture_error(error: IdentityPinError) -> anyhow::Error {
    match error {
        IdentityPinError::AlreadyDead => anyhow::anyhow!("process exited during identity capture"),
        IdentityPinError::Stale => anyhow::anyhow!("process identity changed during capture"),
        IdentityPinError::Unavailable => {
            anyhow::anyhow!("pidfd process identity is unavailable on this host")
        }
    }
}

fn target_identity_capture_error(error: IdentityPinError) -> ExecutionProcessIdentityCaptureError {
    match error {
        IdentityPinError::AlreadyDead | IdentityPinError::Stale => {
            ExecutionProcessIdentityCaptureError::TargetAlreadyDeadOrStale
        }
        IdentityPinError::Unavailable => {
            ExecutionProcessIdentityCaptureError::CaptureFailed(identity_capture_error(error))
        }
    }
}

fn non_target_identity_capture_error(
    error: IdentityPinError,
) -> ExecutionProcessIdentityCaptureError {
    ExecutionProcessIdentityCaptureError::CaptureFailed(identity_capture_error(error))
}

#[cfg(target_os = "linux")]
fn pidfd_send_signal_raw(pidfd: i32, signal: i32, flags: u32) -> SignalResult {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            flags,
        )
    };
    if result == 0 {
        return SignalResult::Delivered;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        SignalResult::AlreadyDead
    } else {
        SignalResult::Failed
    }
}

#[cfg(target_os = "linux")]
fn pidfd_send_signal(target: &PinnedProcess, signal: i32) -> SignalResult {
    pidfd_send_signal_raw(target.pidfd.as_raw_fd(), signal, 0)
}

#[cfg(target_os = "linux")]
fn signal_pinned_group(leader: &PinnedProcess, signal: i32) -> SignalResult {
    // Address the process group through the verified leader's struct-pid
    // reference. A raw `kill(-pgid, ...)` is forbidden here: an open pidfd does
    // not reserve the numeric ID after reaping, so the number could name a
    // replacement group between verification and delivery.
    pidfd_send_signal_raw(
        leader.pidfd.as_raw_fd(),
        signal,
        libc::PIDFD_SIGNAL_PROCESS_GROUP,
    )
}

#[cfg(target_os = "linux")]
fn pidfd_has_exited(target: &PinnedProcess) -> std::io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd: target.pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
        if result >= 0 {
            return Ok(result > 0 && pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(target_os = "linux")]
fn wait_for_target_exit(
    target: &PinnedProcess,
    deadline: std::time::Instant,
) -> std::io::Result<bool> {
    while std::time::Instant::now() < deadline {
        if pidfd_has_exited(target)? {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(KILL_POLL_INTERVAL_MS));
    }
    pidfd_has_exited(target)
}

#[cfg(target_os = "linux")]
fn wait_for_group_exit(
    group: &PinnedProcess,
    deadline: std::time::Instant,
) -> std::result::Result<bool, ()> {
    while std::time::Instant::now() < deadline {
        match signal_pinned_group(group, 0) {
            SignalResult::Delivered => {}
            SignalResult::AlreadyDead => return Ok(true),
            _ => return Err(()),
        }
        std::thread::sleep(Duration::from_millis(KILL_POLL_INTERVAL_MS));
    }
    match signal_pinned_group(group, 0) {
        SignalResult::Delivered => Ok(false),
        SignalResult::AlreadyDead => Ok(true),
        _ => Err(()),
    }
}

#[cfg(target_os = "linux")]
fn graceful_kill(
    identity: &ExecutionProcessIdentity,
    group: &PinnedProcess,
    grace: Duration,
) -> KillResult {
    let deadline = std::time::Instant::now()
        .checked_add(grace)
        .unwrap_or_else(std::time::Instant::now);
    let target = match pin_target(identity) {
        Ok(target) => Some(target),
        Err(IdentityPinError::AlreadyDead) => None,
        Err(IdentityPinError::Stale) => {
            return KillResult {
                success: false,
                method: "stale_target_identity",
            }
        }
        Err(IdentityPinError::Unavailable) => {
            return KillResult {
                success: false,
                method: "identity_unavailable",
            }
        }
    };

    if let Some(target) = target.as_ref() {
        match pidfd_send_signal(target, libc::SIGTERM) {
            SignalResult::Delivered | SignalResult::AlreadyDead => {}
            _ => {
                return KillResult {
                    success: false,
                    method: "sigterm_failed",
                }
            }
        }
        match wait_for_target_exit(target, deadline) {
            Ok(true) => {}
            Ok(false) => {
                return hard_kill_pinned_group(identity, group, "killed");
            }
            Err(_) => {
                return KillResult {
                    success: false,
                    method: "target_wait_failed",
                }
            }
        }
    }

    // The target has exited. Leave the wrapper alive so Lillux can observe its
    // real exit status and perform owned group cleanup; wait only within the
    // caller's remaining grace, then force the pinned group if necessary.
    match wait_for_group_exit(group, deadline) {
        Ok(true) => KillResult {
            success: true,
            method: "terminated",
        },
        Ok(false) => hard_kill_pinned_group(identity, group, "killed"),
        Err(()) => KillResult {
            success: false,
            method: "group_wait_failed",
        },
    }
}

#[cfg(target_os = "linux")]
fn hard_kill_pinned_group(
    _identity: &ExecutionProcessIdentity,
    group: &PinnedProcess,
    method: &'static str,
) -> KillResult {
    match signal_pinned_group(group, libc::SIGKILL) {
        SignalResult::AlreadyDead => {
            return KillResult {
                success: true,
                method: "already_dead",
            }
        }
        SignalResult::Delivered => {}
        _ => {
            return KillResult {
                success: false,
                method: "sigkill_failed",
            }
        }
    }
    let deadline = std::time::Instant::now()
        .checked_add(Duration::from_millis(POST_SIGKILL_WAIT_MS))
        .unwrap_or_else(std::time::Instant::now);
    match wait_for_group_exit(group, deadline) {
        Ok(success) => KillResult { success, method },
        Err(()) => KillResult {
            success: false,
            method: "group_wait_failed",
        },
    }
}

fn signal_pin_failure(error: IdentityPinError) -> SignalResult {
    match error {
        IdentityPinError::AlreadyDead => SignalResult::AlreadyDead,
        IdentityPinError::Stale => SignalResult::StaleIdentity,
        IdentityPinError::Unavailable => SignalResult::IdentityUnavailable,
    }
}

fn group_pin_failure(_identity: &ExecutionProcessIdentity, error: IdentityPinError) -> KillResult {
    match error {
        IdentityPinError::AlreadyDead => KillResult {
            success: false,
            method: "group_identity_lost",
        },
        IdentityPinError::Stale => KillResult {
            success: false,
            method: "stale_group_identity",
        },
        IdentityPinError::Unavailable => KillResult {
            success: false,
            method: "identity_unavailable",
        },
    }
}

/// Validate an env var name before injecting it as a declared secret.
///
/// Declared secrets are real subprocess env vars, so they must not be
/// allowed to shadow daemon/runtime control env, root discovery, proxy,
/// CA, logging, or other inherited infrastructure names. Ordinary
/// application secrets such as `SUPABASE_SERVICE_KEY` and
/// `OXYLABS_PASSWORD` remain valid.
pub fn validate_spawn_secret_name(name: &str) -> anyhow::Result<()> {
    crate::env_contract::validate_secret_name(name)
        .map_err(|e| anyhow::anyhow!("invalid subprocess secret env name `{name}`: {e:#}"))
}

pub fn subprocess_base_allowlist_names() -> &'static [&'static str] {
    BASE_ALLOWLIST_NAMES
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    #[test]
    fn stale_socket_cleanup_never_unlinks_a_live_listener() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ryeosd.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let error = remove_stale_socket(&path).unwrap_err();
        assert!(error.to_string().contains("live daemon control socket"));
        assert!(path.exists());
    }

    #[test]
    fn stale_socket_cleanup_removes_a_refused_socket() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ryeosd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(listener);

        remove_stale_socket(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn stale_socket_cleanup_refuses_a_non_socket_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ryeosd.sock");
        std::fs::write(&path, b"not a socket").unwrap();

        let error = remove_stale_socket(&path).unwrap_err();
        assert!(error.to_string().contains("non-socket daemon control path"));
        assert!(path.exists());
    }

    /// Spawn a shell that:
    ///   1. Installs a SIGTERM trap which writes a marker file then exits 0.
    ///   2. Sleeps long enough that, absent any signal, the test would time out.
    ///      Returns (child, pgid, marker_path).
    fn spawn_signal_target(
        tmp: &TempDir,
    ) -> (
        std::process::Child,
        ExecutionProcessIdentity,
        std::path::PathBuf,
    ) {
        let marker = tmp.path().join("got_term");
        let marker_str = marker.display().to_string();
        // Keep the signalable shell itself in the loop. A shell blocked on an
        // infinite foreground subshell defers its trap until that child exits,
        // which does not model a runtime handling a signal on the exact PID.
        let script = format!(
            r#"trap 'echo term > "{m}"; exit 0' TERM; while true; do sleep 0.05; done"#,
            m = marker_str
        );
        let child = unsafe {
            // process_group(0) starts the child in its own new process group
            // whose PGID equals its PID — exactly what the daemon kills.
            Command::new("sh")
                .arg("-c")
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                })
                .spawn()
                .expect("spawn signal target")
        };
        let pgid = child.id() as i64;
        // Give the shell a moment to install the trap before signalling.
        std::thread::sleep(Duration::from_millis(150));
        let identity =
            capture_execution_process_identity(pgid, Some(pgid)).expect("capture identity");
        (child, identity, marker)
    }

    fn read_marker(path: &std::path::Path) -> Option<String> {
        let mut f = std::fs::File::open(path).ok()?;
        let mut s = String::new();
        f.read_to_string(&mut s).ok()?;
        Some(s)
    }

    #[test]
    fn spawn_secret_policy_allows_application_secret_names() {
        validate_spawn_secret_name("SUPABASE_SERVICE_KEY").unwrap();
        validate_spawn_secret_name("OXYLABS_PASSWORD").unwrap();
    }

    #[test]
    fn spawn_secret_policy_rejects_protected_names() {
        for name in [
            "RYEOS_APP_ROOT",
            "RYEOSD_THREAD_AUTH_TOKEN",
            "RYEOS_PROJECT_SECRET",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "SSL_CERT_FILE",
        ] {
            let err = validate_spawn_secret_name(name).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("invalid subprocess secret env name"),
                "expected protected-name rejection for {name}, got: {msg}"
            );
        }
    }

    /// Run the kill in a background thread while we concurrently
    /// `waitpid` the child in the foreground. Without this, the child
    /// becomes a zombie that a group-existence probe still reports as alive,
    /// confusing the daemon's bounded group-exit poll. (In production the
    /// reaper is the engine's spawn handle.)
    fn run_kill_with_reaper<R: Send + 'static, F: FnOnce() -> R + Send + 'static>(
        kill: F,
        child: &mut std::process::Child,
    ) -> (R, std::process::ExitStatus) {
        let kill_handle = std::thread::spawn(kill);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll child") {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                let _ = kill_handle.join();
                panic!("signal target did not exit within the bounded test deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let result = kill_handle.join().expect("kill thread join");
        (result, status)
    }

    #[test]
    fn graceful_kill_sends_sigterm_first_and_marker_is_written() {
        let tmp = TempDir::new().unwrap();
        let (mut child, identity, marker) = spawn_signal_target(&tmp);

        let (_result, status) = run_kill_with_reaper(
            move || kill_by_action(&identity, ShutdownAction::Graceful(Duration::from_secs(2))),
            &mut child,
        );

        // SIGTERM trap fired → marker present, child exited 0.
        assert!(
            marker.exists(),
            "marker file missing — SIGTERM handler did not run"
        );
        assert_eq!(read_marker(&marker).as_deref(), Some("term\n"));
        assert!(status.success(), "expected clean exit, got {:?}", status);
    }

    #[test]
    fn hard_kill_skips_sigterm_and_marker_is_not_written() {
        use std::os::unix::process::ExitStatusExt;
        let tmp = TempDir::new().unwrap();
        let (mut child, identity, marker) = spawn_signal_target(&tmp);

        let (result, status) = run_kill_with_reaper(
            move || kill_by_action(&identity, ShutdownAction::Hard),
            &mut child,
        );

        // The verified hard-kill either completed in time (`hard_killed`) or
        // the kernel reaped the entry between SIGKILL and the exit poll
        // (`already_dead`).
        assert!(
            matches!(result.method, "hard_killed" | "already_dead"),
            "unexpected method: {}",
            result.method
        );
        // SIGKILL is uncatchable → trap MUST NOT have run → no marker.
        assert!(
            !marker.exists(),
            "marker file present — SIGTERM handler ran when it should not have"
        );
        // Child terminated by signal (no clean exit code), and that
        // signal was SIGKILL — not SIGTERM.
        assert!(
            status.code().is_none(),
            "expected signal exit, got {:?}",
            status
        );
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "expected SIGKILL, got {:?}",
            status.signal()
        );
    }

    #[test]
    fn graceful_kill_with_stubborn_process_falls_back_to_sigkill() {
        use std::os::unix::process::ExitStatusExt;
        // Trap SIGTERM but do NOT exit — forces grace expiry → SIGKILL path.
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("got_term");
        let marker_str = marker.display().to_string();
        // No subshell — the parent sh installs the trap and loops
        // directly so SIGTERM is fully absorbed by the trap (which
        // intentionally does not exit). The kernel's SIGKILL is then
        // the only thing that can terminate the process group, which
        // is exactly the path under test.
        let script = format!(
            r#"trap 'echo term > "{m}"' TERM; while true; do sleep 0.05; done"#,
            m = marker_str
        );
        let mut child = unsafe {
            Command::new("sh")
                .arg("-c")
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                })
                .spawn()
                .expect("spawn stubborn target")
        };
        let pgid = child.id() as i64;
        std::thread::sleep(Duration::from_millis(150));
        let identity =
            capture_execution_process_identity(pgid, Some(pgid)).expect("capture identity");

        let (_result, status) = run_kill_with_reaper(
            move || {
                kill_by_action(
                    &identity,
                    ShutdownAction::Graceful(Duration::from_millis(300)),
                )
            },
            &mut child,
        );

        // SIGTERM did fire (trap ran) — confirms graceful path tried first…
        assert!(
            marker.exists(),
            "trap marker missing — SIGTERM was not sent before SIGKILL"
        );
        // …and SIGKILL ultimately delivered the death blow.
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "expected SIGKILL fallback, got {:?}",
            status.signal()
        );
    }

    #[test]
    fn graceful_kill_with_reaped_leader_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let (mut child, identity, _marker) = spawn_signal_target(&tmp);
        // Reap it ourselves so the PGID is gone.
        let _ = unsafe { libc::kill(-(identity.pgid() as i32), libc::SIGKILL) };
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(50));

        let result = kill_by_action(
            &identity,
            ShutdownAction::Graceful(Duration::from_millis(50)),
        );
        assert!(!result.success);
        assert_eq!(result.method, "group_identity_lost");
    }

    /// Spawn a target whose SIGUSR1 trap writes a marker then exits, in its own
    /// process group. Mirrors `spawn_signal_target` but for the live-interrupt
    /// signal.
    fn spawn_usr1_target(
        tmp: &TempDir,
    ) -> (
        std::process::Child,
        ExecutionProcessIdentity,
        std::path::PathBuf,
    ) {
        let marker = tmp.path().join("got_usr1");
        let marker_str = marker.display().to_string();
        // As above, the exact target must own the loop so its trap can run;
        // an infinite foreground subshell would cause the parent shell to
        // defer SIGUSR1 indefinitely.
        let script = format!(
            r#"trap 'echo usr1 > "{m}"; exit 0' USR1; while true; do sleep 0.05; done"#,
            m = marker_str
        );
        let child = unsafe {
            Command::new("sh")
                .arg("-c")
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                })
                .spawn()
                .expect("spawn usr1 target")
        };
        let pgid = child.id() as i64;
        std::thread::sleep(Duration::from_millis(150));
        let identity =
            capture_execution_process_identity(pgid, Some(pgid)).expect("capture identity");
        (child, identity, marker)
    }

    #[test]
    fn interrupt_process_delivers_sigusr1_to_exact_target() {
        let tmp = TempDir::new().unwrap();
        let (mut child, identity, marker) = spawn_usr1_target(&tmp);

        let (result, status) =
            run_kill_with_reaper(move || interrupt_process(&identity), &mut child);

        assert_eq!(result, SignalResult::Delivered);
        assert!(marker.exists(), "SIGUSR1 trap did not run — marker missing");
        assert_eq!(read_marker(&marker).as_deref(), Some("usr1\n"));
        assert!(status.success(), "expected clean exit, got {status:?}");
    }

    #[test]
    fn signal_dead_identity_reports_already_dead() {
        let tmp = TempDir::new().unwrap();
        let (mut child, identity, _marker) = spawn_usr1_target(&tmp);
        let _ = unsafe { libc::kill(-(identity.pgid() as i32), libc::SIGKILL) };
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(interrupt_process(&identity), SignalResult::AlreadyDead);
    }

    #[test]
    fn stale_boot_identity_is_never_signalled() {
        let tmp = TempDir::new().unwrap();
        let (mut child, mut identity, _marker) = spawn_usr1_target(&tmp);
        identity.boot_id = "not-this-boot".to_string();

        assert_eq!(interrupt_process(&identity), SignalResult::StaleIdentity);

        let _ = unsafe { libc::kill(-(identity.pgid() as i32), libc::SIGKILL) };
        let _ = child.wait();
    }
}
