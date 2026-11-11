//! Test-only witness for the rejected namespace-membership capture approach.
//!
//! The namespace descriptor, not a parent-PID walk or a numeric process group,
//! identifies descendants, including double forks and nested PID namespaces.
//! Membership is not an atomic writer freeze. Keep the original diagnostic
//! evidence without exporting an unused alternate lifecycle API or making
//! namespace-translation ioctls a production host requirement.

//! Descendant containment by retained PID-namespace init.
//!
//! Every enforced launch runs its target as PID 1 of a private PID namespace.
//! The kernel ends that namespace with its init: `zap_pid_ns_processes` kills
//! and awaits every member, including processes that called `setsid`, forked
//! twice, or created nested PID namespaces, before the init's exit becomes
//! observable. The namespace descriptor, not a parent-PID walk or a numeric
//! process group, identifies those descendants. Retaining the init's pidfd
//! therefore gives termination, recovery and settlement of the whole tree
//! without a cgroup delegation.
//!
//! This is NOT a quiescence barrier: namespace membership is not an atomic
//! writer freeze. Callers which need to observe a stable workspace while the
//! tree runs must refuse this containment (see [`ProcessScope::quiesce`]).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use super::{ExactProcessIdentity, PinnedMember, linux};

/// Control budget retained with a PID-namespace containment record. The
/// mechanism has no provider configuration to carry an administrator choice.
pub(super) const PID_NAMESPACE_CONTROL_TIMEOUT: Duration = Duration::from_secs(15);

// Linux UAPI <linux/nsfs.h>. The translation ioctls take a PID value (not a
// pointer despite _IOR) and return the translated PID. ESRCH means outside
// the namespace or gone; unsupported ioctls are errors, never a group fallback.
const NS_GET_NSTYPE: libc::c_ulong = 0xb703;
const NS_GET_PID_FROM_PIDNS: libc::c_ulong = 0x8004_b706;
const NS_GET_PID_IN_PIDNS: libc::c_ulong = 0x8004_b708;

/// Kernel namespace coordinate. This is meaningful only alongside the existing
/// exact boot/target-birth identity, never as a standalone reusable inode token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PidNamespaceIdentity {
    pub device: u64,
    pub inode: u64,
}

/// A private descendant namespace and its exact, live PID-1 incarnation.
/// Captured while a trusted launcher holds the target before workload exec.
#[derive(Debug)]
pub struct PinnedPidNamespace {
    namespace: File,
    init: PinnedMember,
    identity: PidNamespaceIdentity,
    init_pid: u32,
}

impl PinnedPidNamespace {
    /// Prove that the exact target is PID 1 of a namespace which excludes the
    /// caller. A workload in the caller's namespace cannot grant tree authority.
    pub fn pin(process: &ExactProcessIdentity) -> Result<Self, String> {
        // Namespace authority consumes only the existing target incarnation.
        // Its caller's group is deliberately not a membership or death proof.
        if process.boot_id.is_empty()
            || process.target_pid <= 1
            || process.target_start_time_ticks == 0
        {
            return Err("PID namespace target identity is incomplete".to_owned());
        }
        if linux::read_boot_id()? != process.boot_id {
            return Err("PID namespace owner belongs to another boot".to_owned());
        }
        let init = pin_incarnation(process.target_pid, process.target_start_time_ticks)?;
        // Procfs namespace links are intentional kernel magic links. Pin the
        // target with its pidfd before opening and recheck it after all reads.
        let namespace = File::open(format!("/proc/{}/ns/pid", process.target_pid))
            .map_err(|error| format!("pin target PID namespace: {error}"))?;
        if unsafe { libc::ioctl(namespace.as_raw_fd(), NS_GET_NSTYPE) } != libc::CLONE_NEWPID {
            return Err("target namespace descriptor is not a PID namespace".to_owned());
        }
        if translate(&namespace, NS_GET_PID_IN_PIDNS, std::process::id())?.is_some() {
            return Err("target PID namespace includes the caller".to_owned());
        }
        if translate(&namespace, NS_GET_PID_FROM_PIDNS, 1)? != Some(process.target_pid) {
            return Err("exact target is not the PID namespace init".to_owned());
        }
        if translate(&namespace, NS_GET_PID_IN_PIDNS, process.target_pid)? != Some(1) {
            return Err("PID namespace init translation disagrees with target".to_owned());
        }
        let metadata = namespace
            .metadata()
            .map_err(|error| format!("inspect pinned PID namespace: {error}"))?;
        require_incarnation(&init, process.target_start_time_ticks)?;
        Ok(Self {
            namespace,
            init,
            identity: PidNamespaceIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            init_pid: process.target_pid,
        })
    }

    pub(super) fn init_pid(&self) -> u32 {
        self.init_pid
    }

    /// Nonblocking: has the retained init exited? Exit is published only
    /// after the kernel has killed and awaited every namespace member.
    pub fn init_exited(&self) -> Result<bool, String> {
        let mut pollfd = libc::pollfd {
            fd: self.init.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
            if result == 0 {
                return Ok(false);
            }
            if result > 0 {
                return if pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                    Ok(true)
                } else {
                    Err(format!(
                        "namespace init pidfd reported unexpected events {:#x}",
                        pollfd.revents
                    ))
                };
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(format!("poll namespace init pidfd: {error}"));
            }
        }
    }

    pub fn identity(&self) -> PidNamespaceIdentity {
        self.identity
    }

    /// Reattach only the same namespace and exact init incarnation. No namespace
    /// descriptor or live target may be replaced by an equal-looking path.
    pub fn pin_expected(
        process: &ExactProcessIdentity,
        expected: PidNamespaceIdentity,
    ) -> Result<Self, String> {
        let pinned = Self::pin(process)?;
        if pinned.identity != expected {
            return Err("exact target PID namespace identity changed".to_owned());
        }
        Ok(pinned)
    }

    /// Read-only numeric membership observation; this grants no signal. Callers
    /// which act on a member must additionally pin and verify its incarnation.
    pub fn contains_pid(&self, pid: u32) -> Result<bool, String> {
        Ok(translate(&self.namespace, NS_GET_PID_IN_PIDNS, pid)?.is_some())
    }

    /// Kill the exact namespace init and wait for kernel exit testimony.
    /// Group/session changes cannot escape that lifetime boundary. This proves
    /// exit, not that the launcher's wait/reap obligation has been settled.
    pub fn terminate_and_wait(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "PID namespace termination deadline overflow".to_owned())?;
        match linux::pidfd_signal(self.init.pidfd.as_raw_fd(), libc::SIGKILL, 0) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => return Err(format!("terminate exact namespace init: {error}")),
        }
        linux::wait_pidfd_exit(self.init.pidfd.as_raw_fd(), deadline)
    }

    /// Observe natural or externally requested namespace termination without
    /// sending a signal. The non-thread pidfd becomes readable only after the
    /// init thread group exits. In Linux, find_child_reaper() calls
    /// zap_pid_ns_processes() before exit_notify() publishes that exit: new PID
    /// allocation is disabled, descendants are killed, and their teardown is
    /// awaited. The init may still have an unreaped zombie PID afterwards.
    ///
    /// Do not replace this retained descriptor proof with absence of a numeric
    /// PID, a process-group scan, or absence of a procfs namespace link.
    pub fn wait_for_exit(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "PID namespace exit deadline overflow".to_owned())?;
        linux::wait_pidfd_exit(self.init.pidfd.as_raw_fd(), deadline)
    }
}

/// Control authority recovered from persisted init coordinates.
#[derive(Debug)]
pub(super) enum RecoveredInit {
    /// The exact init incarnation is alive and its namespace is pinned.
    Live(PinnedPidNamespace),
    /// The exact init incarnation has exited (or its boot ended). The kernel
    /// tears the whole namespace down before an init's exit is observable, so
    /// no member of it can remain.
    Gone,
}

/// Reattach the exact recorded init. A reused PID (different birth), an
/// absent PID, or an exited init proves the namespace is gone; a live init
/// must still be in the recorded namespace or recovery refuses.
pub(super) fn recover_init(
    boot_id: &str,
    init_pid: u32,
    init_start_time_ticks: u64,
    expected: PidNamespaceIdentity,
) -> Result<RecoveredInit, String> {
    if linux::read_boot_id()? != boot_id {
        return Ok(RecoveredInit::Gone);
    }
    let process = ExactProcessIdentity {
        boot_id: boot_id.to_owned(),
        target_pid: init_pid,
        target_start_time_ticks: init_start_time_ticks,
        // Namespace authority never consumes the caller's group coordinates.
        group_leader_pid: init_pid,
        group_leader_start_time_ticks: init_start_time_ticks,
        group_basis: crate::GroupBasis::ProcessGroup,
    };
    // Never classify from the /proc state letter: a thread-group leader that
    // exits while other threads run is `Z` with namespace members alive. The
    // init is gone only when no such pid exists, its birth differs, or its
    // pidfd is readable (the whole thread group exited, which the kernel
    // publishes only after the namespace was torn down).
    for _ in 0..2 {
        if matches!(
            observe_init(init_pid, init_start_time_ticks)?,
            InitObservation::Gone
        ) {
            return Ok(RecoveredInit::Gone);
        }
        match PinnedPidNamespace::pin_expected(&process, expected) {
            Ok(pinned) => return Ok(RecoveredInit::Live(pinned)),
            // The init may have exited between observation and pin; the next
            // iteration classifies that. A live init in another namespace
            // keeps failing and is refused.
            Err(_) => continue,
        }
    }
    match observe_init(init_pid, init_start_time_ticks)? {
        InitObservation::Gone => Ok(RecoveredInit::Gone),
        InitObservation::Live => {
            Err("recorded namespace init is live but is not the recorded namespace init".to_owned())
        }
    }
}

enum InitObservation {
    Gone,
    Live,
}

fn observe_init(pid: u32, start: u64) -> Result<InitObservation, String> {
    use std::os::fd::{FromRawFd as _, OwnedFd};
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
    if raw < 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(InitObservation::Gone)
        } else {
            Err(format!("open recorded namespace init pidfd: {error}"))
        };
    }
    // SAFETY: pidfd_open returned one new uniquely owned descriptor.
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw) };
    match linux::read_process_stat(pid) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InitObservation::Gone);
        }
        Err(error) => return Err(format!("observe recorded namespace init: {error}")),
        Ok(stat) if stat.start_time_ticks != start => return Ok(InitObservation::Gone),
        Ok(_) => {}
    }
    // Birth was read after the pidfd was opened, so the pidfd names that
    // incarnation. Gone means exit of the whole thread group.
    match linux::pidfd_signal(pidfd.as_raw_fd(), 0, 0) {
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
            return Ok(InitObservation::Gone);
        }
        Err(error) => return Err(format!("probe recorded namespace init: {error}")),
        Ok(()) => {}
    }
    let mut pollfd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
        if result > 0 {
            return Ok(InitObservation::Gone);
        }
        if result == 0 {
            return Ok(InitObservation::Live);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!("poll recorded namespace init pidfd: {error}"));
        }
    }
}

/// Grade of the PID-namespace proof on this host: measured on a native kernel,
/// provider-attested where the kernel is an emulation (gVisor), whose
/// namespace answers are not observations of a real kernel.
pub fn pid_namespace_proof_source(gvisor: bool) -> super::host_capability::ProofSource {
    use super::host_capability::ProofSource;
    if gvisor {
        ProofSource::ProviderAttested
    } else {
        ProofSource::Measured
    }
}

/// The probe's outcome, run at most once per boot per process while its
/// answer is stable. An allowed outcome and a refusal by a permanent errno
/// (`EPERM`, `EINVAL`, `ENOSYS`, `ENOSPC`, `EACCES`: namespaces forbidden,
/// unsupported or capped by this kernel or policy) are cached; every other outcome (resource exhaustion,
/// a busy fork window, any non-errno failure) is transient and is probed
/// again by the next caller.
///
/// This qualifies the kernel mechanism (`CLONE_NEWUSER|CLONE_NEWPID`, a
/// retained init, kernel teardown of every member). It cannot run the
/// isolation backend's own entry point, which needs a built, signed adapter
/// and a target image. So every launch checks the same fact on the real
/// target: binding pins the held target as PID 1 of a namespace excluding the
/// daemon and refuses, naming the mismatch, if the backend did not make it so.
pub fn cached_pid_namespace_containment_probe() -> super::host_capability::ProbeOutcome {
    use super::host_capability::ProbeOutcome;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<Option<(String, ProbeOutcome)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let boot = linux::read_boot_id().ok();
    if let Some(boot) = boot.as_ref() {
        let held = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached_boot, outcome)) = held.as_ref()
            && cached_boot == boot
        {
            return outcome.clone();
        }
    }
    let outcome = probe_pid_namespace_containment();
    if let Some(boot) = boot
        && probe_outcome_is_stable(&outcome)
    {
        *cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((boot, outcome.clone()));
    }
    outcome
}

/// Whether a probe outcome is a property of this kernel and boot (cacheable)
/// rather than of the moment. Allowed is stable. A refusal is stable only
/// when the kernel named a permanent policy or support errno for creating the
/// namespaces. Resource exhaustion (`EAGAIN`, `ENOMEM`), a refusal carrying
/// no errno (a pin or timing failure inside the probe, which a retry can
/// clear) and an unobservable outcome are transient.
fn probe_outcome_is_stable(outcome: &super::host_capability::ProbeOutcome) -> bool {
    use super::host_capability::ProbeOutcome;
    match outcome {
        ProbeOutcome::Allowed => true,
        ProbeOutcome::Refused { errno, .. } => matches!(
            *errno,
            libc::EPERM | libc::EINVAL | libc::ENOSYS | libc::ENOSPC | libc::EACCES
        ),
        ProbeOutcome::Unobservable { .. } => false,
    }
}

/// Upper bound for the whole qualification probe, including its fork window.
const PROBE_BOUND: Duration = Duration::from_secs(10);

/// Qualify, on this host, that retaining a PID-namespace init contains and
/// settles descendants that leave their process group.
///
/// A disposable namespace is built natively (no host executable): its init
/// starts a descendant that calls `setsid` and double-forks, which in turn
/// creates a nested PID namespace with a further descendant. After the
/// probe pins the init, it kills only that init and requires that the init's
/// pidfd becomes ready, that every descendant has closed its lifeline (so all
/// have exited), and that after the wrapper reaps the init no retained
/// namespace descriptor translates PID 1 to any process.
pub fn probe_pid_namespace_containment() -> super::host_capability::ProbeOutcome {
    use super::host_capability::ProbeOutcome;
    let unobservable = |reason: String| ProbeOutcome::Unobservable { reason };
    let Some(window) = Instant::now().checked_add(PROBE_BOUND) else {
        return unobservable("probe bound overflow".to_owned());
    };
    let fork_guard = match crate::exec::quiesce_fork_sensitive_descriptors(window) {
        Ok(guard) => guard,
        Err(error) => return unobservable(format!("fork window for probe: {error}")),
    };
    let child_close_fds = match fork_guard.fork_child_close_fds(&std::collections::BTreeSet::new())
    {
        Ok(fds) => fds,
        Err(error) => return unobservable(format!("fork snapshot for probe: {error}")),
    };
    let pipe = || -> Result<[libc::c_int; 2], String> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!("probe pipe: {}", std::io::Error::last_os_error()));
        }
        Ok(fds)
    };
    let (report, ready, lifeline) = match (pipe(), pipe(), pipe()) {
        (Ok(report), Ok(ready), Ok(lifeline)) => (report, ready, lifeline),
        (a, b, c) => {
            for fds in [a, b, c].into_iter().flatten() {
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
            }
            return unobservable("probe pipes could not be created".to_owned());
        }
    };
    let close_all = |fds: &[libc::c_int]| {
        for fd in fds {
            unsafe { libc::close(*fd) };
        }
    };
    let witnessed = match fork_guard.witness_creation("PID namespace containment probe creation") {
        Ok(witnessed) => witnessed,
        Err(error) => {
            close_all(&[
                report[0],
                report[1],
                ready[0],
                ready[1],
                lifeline[0],
                lifeline[1],
            ]);
            return unobservable(format!("creation witness for probe: {error}"));
        }
    };
    let fork_custody = witnessed.0;
    let fork_custody_witness = witnessed.1;
    let wrapper = unsafe { libc::fork() };
    if wrapper < 0 {
        let error = std::io::Error::last_os_error();
        close_all(&[
            report[0],
            report[1],
            ready[0],
            ready[1],
            lifeline[0],
            lifeline[1],
        ]);
        return unobservable(format!("fork probe wrapper: {error}"));
    }
    if wrapper == 0 {
        // Only async-signal-safe calls from here; the wrapper never returns.
        unsafe { probe_wrapper(&child_close_fds, report, ready, lifeline) }
    }
    drop(fork_custody_witness);
    close_all(&[report[1], ready[1], lifeline[1]]);

    let outcome = probe_observe(wrapper, report[0], ready[0], lifeline[0]);

    // Always end the wrapper (its init dies with it) and settle the fork
    // window; a stranded window is part of the fact.
    unsafe { libc::kill(wrapper, libc::SIGKILL) };
    close_all(&[report[0], ready[0], lifeline[0]]);
    let reaped = Instant::now()
        .checked_add(PROBE_BOUND)
        .ok_or_else(|| "probe bound overflow".to_owned())
        .and_then(|bound| {
            crate::exec::reap_fork_holder_until(wrapper, bound, "PID namespace containment probe")
        });
    let custody = fork_custody.settle();
    match (outcome, reaped, custody) {
        (outcome, Ok(_), Ok(())) => outcome,
        (outcome, reaped, custody) => unobservable(format!(
            "probe did not settle ({outcome:?}); wrapper: {reaped:?}; fork window: {custody:?}"
        )),
    }
}

/// Wrapper process: owns the namespace's parent role (so it reaps the init)
/// and reports the init's host PID.
///
/// # Safety
/// Child of a fork only; allocation-free and async-signal-safe.
unsafe fn probe_wrapper(
    close_fds: &[libc::c_int],
    report: [libc::c_int; 2],
    ready: [libc::c_int; 2],
    lifeline: [libc::c_int; 2],
) -> ! {
    unsafe {
        for fd in close_fds {
            libc::close(*fd);
        }
        libc::close(report[0]);
        libc::close(ready[0]);
        libc::close(lifeline[0]);
        libc::setpgid(0, 0);
        if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) != 0 {
            let code = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(255);
            libc::_exit(code.clamp(1, 250));
        }
        let init = libc::fork();
        if init < 0 {
            libc::_exit(252);
        }
        if init == 0 {
            probe_init(report[1], ready[1])
        }
        libc::close(ready[1]);
        libc::close(lifeline[1]);
        let pid = init as u32;
        let written = libc::write(report[1], (&pid as *const u32).cast(), 4);
        libc::close(report[1]);
        if written != 4 {
            libc::kill(init, libc::SIGKILL);
        }
        let mut status = 0;
        libc::waitpid(init, &mut status, 0);
        libc::_exit(0)
    }
}

/// PID 1 of the disposable namespace. The `lifeline` write end it inherited
/// from the wrapper is kept open by every descendant until it exits.
///
/// # Safety
/// Child of a fork only; allocation-free and async-signal-safe.
unsafe fn probe_init(report_write: libc::c_int, ready_write: libc::c_int) -> ! {
    unsafe {
        libc::close(report_write);
        // Die with the wrapper if the probe is abandoned.
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
        let session = libc::getsid(0);
        let a = libc::fork();
        if a < 0 {
            libc::_exit(253);
        }
        if a == 0 {
            if libc::setsid() < 0 {
                libc::_exit(253);
            }
            let b = libc::fork();
            if b < 0 {
                libc::_exit(253);
            }
            if b > 0 {
                // Double fork: the detached grandchild is reparented to init.
                libc::_exit(0);
            }
            if libc::getsid(0) == session {
                libc::_exit(253);
            }
            let tag = b'B';
            libc::write(ready_write, (&tag as *const u8).cast(), 1);
            if libc::unshare(libc::CLONE_NEWPID) != 0 {
                libc::_exit(253);
            }
            let c = libc::fork();
            if c < 0 {
                libc::_exit(253);
            }
            if c == 0 {
                if libc::getpid() != 1 {
                    libc::_exit(253);
                }
                let tag = b'C';
                libc::write(ready_write, (&tag as *const u8).cast(), 1);
            }
            loop {
                libc::pause();
            }
        }
        let mut status = 0;
        libc::waitpid(a, &mut status, 0);
        loop {
            libc::pause();
        }
    }
}

fn poll_readable(fd: libc::c_int, deadline: Instant) -> Result<bool, String> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe {
            libc::poll(
                &mut pollfd,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if result > 0 {
            return Ok(pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0);
        }
        if result == 0 {
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!("poll probe pipe: {error}"));
        }
    }
}

fn read_exact_bounded(fd: libc::c_int, buffer: &mut [u8], deadline: Instant) -> Result<(), String> {
    let mut filled = 0;
    while filled < buffer.len() {
        if !poll_readable(fd, deadline)? {
            return Err("probe step did not report before its deadline".to_owned());
        }
        let count = unsafe {
            libc::read(
                fd,
                buffer[filled..].as_mut_ptr().cast(),
                buffer.len() - filled,
            )
        };
        if count == 0 {
            return Err("probe step ended before reporting (setup refused)".to_owned());
        }
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("read probe pipe: {error}"));
        }
        filled += count as usize;
    }
    Ok(())
}

fn probe_observe(
    wrapper: libc::pid_t,
    report: libc::c_int,
    ready: libc::c_int,
    lifeline: libc::c_int,
) -> super::host_capability::ProbeOutcome {
    use super::host_capability::ProbeOutcome;
    use std::os::fd::AsFd as _;
    let Some(deadline) = Instant::now().checked_add(PROBE_BOUND / 2) else {
        return ProbeOutcome::Unobservable {
            reason: "probe bound overflow".to_owned(),
        };
    };
    let refused = |reason: String| ProbeOutcome::Refused { errno: 0, reason };
    let mut init_pid = [0u8; 4];
    if let Err(error) = read_exact_bounded(report, &mut init_pid, deadline) {
        // The wrapper's exit status names an unshare refusal precisely.
        let mut status = 0;
        let waited = unsafe { libc::waitpid(wrapper, &mut status, libc::WNOHANG) };
        if waited == wrapper && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) != 0 {
            let errno = libc::WEXITSTATUS(status);
            return ProbeOutcome::Refused {
                errno,
                reason: format!(
                    "creating a private user and PID namespace: {}",
                    std::io::Error::from_raw_os_error(errno)
                ),
            };
        }
        return ProbeOutcome::Unobservable { reason: error };
    }
    let init_pid = u32::from_ne_bytes(init_pid);
    // Both detached descendants must exist and have escaped before the kill.
    let mut ready_tags = [0u8; 2];
    if let Err(error) = read_exact_bounded(ready, &mut ready_tags, deadline) {
        return ProbeOutcome::Unobservable {
            reason: format!("descendants were not established: {error}"),
        };
    }
    ready_tags.sort_unstable();
    if ready_tags != [b'B', b'C'] {
        return ProbeOutcome::Unobservable {
            reason: "descendant readiness was malformed".to_owned(),
        };
    }
    if matches!(poll_readable_now(lifeline), Ok(true)) {
        return ProbeOutcome::Unobservable {
            reason: "a descendant exited before containment was exercised".to_owned(),
        };
    }
    let pinned = (|| -> Result<PinnedPidNamespace, String> {
        let pidfd = linux::open_pidfd(init_pid)?;
        let identity =
            super::capture_exact_process_identity_from_pidfd(init_pid, None, pidfd.as_fd())?;
        PinnedPidNamespace::pin(&identity)
    })();
    let pinned = match pinned {
        Ok(pinned) => pinned,
        Err(error) => {
            return refused(format!(
                "pin the namespace init of a private namespace: {error}"
            ));
        }
    };
    if let Err(error) = pinned.terminate_and_wait(Duration::from_secs(5)) {
        return refused(format!("terminate the namespace init: {error}"));
    }
    match pinned.init_exited() {
        Ok(true) => {}
        other => return refused(format!("init pidfd not ready after termination: {other:?}")),
    }
    // Every descendant held a lifeline write end: hang-up proves each exited,
    // including the setsid'd, double-forked and nested-namespace members.
    match poll_readable_now(lifeline) {
        Ok(true) => {}
        other => {
            return refused(format!(
                "descendants outlived their namespace init: {other:?}"
            ));
        }
    }
    let mut byte = [0u8; 1];
    let drained = unsafe { libc::read(lifeline, byte.as_mut_ptr().cast(), 1) };
    if drained != 0 {
        return refused("a descendant still held the lifeline after init exit".to_owned());
    }
    // Let the wrapper reap the init, then require that the retained
    // descriptor no longer translates PID 1 to any process.
    let reap_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match translate(&pinned.namespace, NS_GET_PID_FROM_PIDNS, 1) {
            Ok(None) => break,
            Ok(Some(_)) if Instant::now() < reap_deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            other => {
                return refused(format!(
                    "namespace still names a PID 1 after the init was reaped: {other:?}"
                ));
            }
        }
    }
    ProbeOutcome::Allowed
}

fn poll_readable_now(fd: libc::c_int) -> Result<bool, String> {
    poll_readable(fd, Instant::now())
}

fn translate(namespace: &File, operation: libc::c_ulong, pid: u32) -> Result<Option<u32>, String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err("PID namespace translation requires a positive PID".to_owned());
    }
    let result = unsafe { libc::ioctl(namespace.as_raw_fd(), operation, libc::c_ulong::from(pid)) };
    if result > 0 {
        return Ok(Some(result as u32));
    }
    let error = std::io::Error::last_os_error();
    if result == -1 && error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(None);
    }
    Err(format!("kernel PID namespace translation failed: {error}"))
}

fn pin_incarnation(pid: u32, start: u64) -> Result<PinnedMember, String> {
    let member = PinnedMember {
        pid,
        pidfd: linux::open_pidfd(pid)?,
    };
    require_incarnation(&member, start)?;
    Ok(member)
}

fn require_incarnation(member: &PinnedMember, start: u64) -> Result<(), String> {
    linux::pidfd_signal(member.pidfd.as_raw_fd(), 0, 0)
        .map_err(|error| format!("probe namespace process incarnation: {error}"))?;
    let stat = linux::read_process_stat(member.pid)
        .map_err(|error| format!("read namespace process incarnation: {error}"))?;
    // Liveness is the pidfd's, not the state letter's: a zombie leader may
    // still lead live threads. Only a dead (`X`) task is not an incarnation.
    if stat.start_time_ticks != start || stat.state == 'X' {
        return Err("PID namespace process incarnation changed or exited".to_owned());
    }
    linux::pidfd_signal(member.pidfd.as_raw_fd(), 0, 0)
        .map_err(|error| format!("reprobe namespace process incarnation: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(pid: u32) -> ExactProcessIdentity {
        let stat = linux::read_process_stat(pid).unwrap();
        let leader = linux::read_process_stat(stat.process_group as u32).unwrap();
        ExactProcessIdentity {
            boot_id: linux::read_boot_id().unwrap(),
            target_pid: pid,
            target_start_time_ticks: stat.start_time_ticks,
            group_leader_pid: stat.process_group as u32,
            group_leader_start_time_ticks: leader.start_time_ticks,
            group_basis: crate::GroupBasis::ProcessGroup,
        }
    }

    #[test]
    fn pid_namespace_authority_refuses_callers_own_namespace() {
        let error = PinnedPidNamespace::pin(&identity(std::process::id())).unwrap_err();
        assert!(error.contains("includes the caller"), "{error}");
    }

    #[test]
    fn pid_namespace_authority_refuses_changed_birth_and_boot() {
        let mut process = identity(std::process::id());
        process.target_start_time_ticks += 1;
        assert!(
            PinnedPidNamespace::pin(&process)
                .unwrap_err()
                .contains("incarnation")
        );
        process.boot_id = "not-this-boot".to_owned();
        assert!(
            PinnedPidNamespace::pin(&process)
                .unwrap_err()
                .contains("another boot")
        );
    }

    #[test]
    fn pid_namespace_translation_has_no_unsupported_descriptor_fallback() {
        let file = tempfile::tempfile().unwrap();
        assert!(translate(&file, NS_GET_PID_IN_PIDNS, std::process::id()).is_err());
    }

    extern "C" fn park_forever(_: *mut libc::c_void) -> *mut libc::c_void {
        loop {
            unsafe { libc::pause() };
        }
    }

    /// Namespace init that starts a second thread, reports readiness, then on
    /// command exits ONLY its leader thread (`SYS_exit`), leaving a zombie
    /// thread-group leader with a live member.
    fn spawn_leader_exit_fixture() -> (libc::pid_t, u32, libc::c_int) {
        let pipe = || {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            fds
        };
        let (report, ready, control) = (pipe(), pipe(), pipe());
        let wrapper = unsafe { libc::fork() };
        assert!(wrapper >= 0);
        if wrapper == 0 {
            unsafe {
                libc::setpgid(0, 0);
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) != 0 {
                    libc::_exit(1);
                }
                let init = libc::fork();
                if init == 0 {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
                    let mut thread: libc::pthread_t = std::mem::zeroed();
                    if libc::pthread_create(
                        &mut thread,
                        std::ptr::null(),
                        park_forever,
                        std::ptr::null_mut(),
                    ) != 0
                    {
                        libc::_exit(2);
                    }
                    libc::write(ready[1], b"T".as_ptr().cast(), 1);
                    let mut byte = 0u8;
                    libc::read(control[0], (&mut byte as *mut u8).cast(), 1);
                    libc::syscall(libc::SYS_exit, 0);
                    libc::_exit(3);
                }
                let pid = init as u32;
                libc::write(report[1], (&pid as *const u32).cast(), 4);
                let mut status = 0;
                libc::waitpid(init, &mut status, 0);
                libc::_exit(0);
            }
        }
        let mut pid = [0u8; 4];
        assert_eq!(
            unsafe { libc::read(report[0], pid.as_mut_ptr().cast(), 4) },
            4,
            "wrapper reported its namespace init"
        );
        let mut tag = 0u8;
        assert_eq!(
            unsafe { libc::read(ready[0], (&mut tag as *mut u8).cast(), 1) },
            1,
            "the init started its second thread"
        );
        (wrapper, u32::from_ne_bytes(pid), control[1])
    }

    #[test]
    fn recovery_does_not_report_empty_when_the_init_leader_exits_but_threads_live() {
        use std::os::fd::AsFd as _;
        let (wrapper, init_pid, control) = spawn_leader_exit_fixture();
        struct Reap(libc::pid_t);
        impl Drop for Reap {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    let mut status = 0;
                    libc::waitpid(self.0, &mut status, 0);
                }
            }
        }
        let _reap = Reap(wrapper);
        let pidfd = linux::open_pidfd(init_pid).unwrap();
        let identity =
            super::super::capture_exact_process_identity_from_pidfd(init_pid, None, pidfd.as_fd())
                .unwrap();
        let namespace = PinnedPidNamespace::pin(&identity).unwrap().identity();
        // The leader thread exits; the thread group (and namespace) lives on.
        assert_eq!(unsafe { libc::write(control, b"x".as_ptr().cast(), 1) }, 1);
        let deadline = Instant::now() + Duration::from_secs(3);
        while linux::read_process_stat(init_pid).unwrap().state != 'Z' {
            assert!(Instant::now() < deadline, "leader did not become a zombie");
            std::thread::sleep(Duration::from_millis(5));
        }
        let recovered = recover_init(
            &identity.boot_id,
            init_pid,
            identity.target_start_time_ticks,
            namespace,
        )
        .unwrap();
        let RecoveredInit::Live(live) = recovered else {
            panic!("a zombie leader with live threads was reported as an empty namespace");
        };
        assert!(!live.init_exited().unwrap());
        live.terminate_and_wait(Duration::from_secs(5)).unwrap();
        assert!(live.init_exited().unwrap());
        // Only now, after the whole thread group exited, is it gone.
        assert!(matches!(
            recover_init(
                &identity.boot_id,
                init_pid,
                identity.target_start_time_ticks,
                namespace
            )
            .unwrap(),
            RecoveredInit::Gone
        ));
    }

    #[test]
    fn only_kernel_stable_probe_outcomes_are_cached() {
        use super::super::host_capability::ProbeOutcome;
        let refused = |errno| ProbeOutcome::Refused {
            errno,
            reason: "x".to_owned(),
        };
        assert!(probe_outcome_is_stable(&ProbeOutcome::Allowed));
        for permanent in [
            libc::EPERM,
            libc::EINVAL,
            libc::ENOSYS,
            libc::ENOSPC,
            libc::EACCES,
        ] {
            assert!(probe_outcome_is_stable(&refused(permanent)), "{permanent}");
        }
        // Resource exhaustion and errno-less probe-internal failures retry.
        for transient in [libc::EAGAIN, libc::ENOMEM, 0] {
            assert!(!probe_outcome_is_stable(&refused(transient)), "{transient}");
        }
        assert!(!probe_outcome_is_stable(&ProbeOutcome::Unobservable {
            reason: "busy".to_owned()
        }));
    }

    #[test]
    fn qualification_probe_contains_detached_double_forked_and_nested_descendants() {
        use super::super::host_capability::ProbeOutcome;
        match probe_pid_namespace_containment() {
            ProbeOutcome::Allowed => {}
            // A host that forbids unprivileged user namespaces cannot run
            // the probe; the mechanism is then simply not qualified there.
            ProbeOutcome::Refused {
                errno: libc::EPERM | libc::EINVAL | libc::ENOSPC | libc::EACCES,
                reason,
            } => {
                eprintln!("skipped: private namespaces refused here: {reason}");
            }
            other => panic!("PID namespace containment probe failed: {other:?}"),
        }
    }

    #[test]
    #[ignore = "native Linux namespace qualification; uses host util-linux only as a test fixture"]
    fn pid_namespace_authority_covers_detached_and_nested_descendants() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command, Stdio};

        struct Fixture(Child);
        impl Drop for Fixture {
            fn drop(&mut self) {
                // The Child retains the exact unreaped wrapper. util-linux's
                // explicit kill-child option binds namespace-init lifetime.
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        // Host executables are fixture construction, not launch dependencies
        // or a substitute for the native sandbox's separate E2E acceptance.
        let mut fixture = Fixture(
            Command::new("unshare")
                .args([
                    "--user", "--map-root-user", "--pid", "--fork", "--kill-child=KILL",
                    "/bin/sh", "-c",
                    "setsid unshare --pid --fork --kill-child=KILL /bin/sh -c 'echo ready; exec sleep 60' & wait",
                ])
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn().unwrap(),
        );
        let output = fixture.0.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(output).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            "ready\n"
        );
        reader.join().unwrap();
        let children = |pid: u32| -> Vec<u32> {
            std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                .unwrap()
                .split_whitespace()
                .map(|pid| pid.parse().unwrap())
                .collect()
        };
        let init_pid = children(fixture.0.id())[0];
        let process = identity(init_pid);
        let owner = PinnedPidNamespace::pin(&process).unwrap();
        let detached = children(init_pid)[0];
        let nested = children(detached)[0];
        let detached_fd = linux::open_pidfd(detached).unwrap();
        let nested_fd = linux::open_pidfd(nested).unwrap();
        assert_ne!(
            linux::read_process_stat(detached).unwrap().process_group,
            linux::read_process_stat(init_pid).unwrap().process_group
        );
        assert!(owner.contains_pid(init_pid).unwrap());
        assert!(owner.contains_pid(detached).unwrap());
        assert!(owner.contains_pid(nested).unwrap());
        assert!(!owner.contains_pid(std::process::id()).unwrap());
        assert!(!owner.contains_pid(fixture.0.id()).unwrap());
        // A successful group barrier is NOT a whole-namespace barrier. Keep
        // this counterexample beside the primitive so future callers cannot
        // allow setsid/setpgid and silently retain the old capture authority.
        let group =
            super::super::quiesce_exact_process_group(&process, Duration::from_secs(2)).unwrap();
        assert!(matches!(
            linux::read_process_stat(init_pid).unwrap().state,
            'T' | 't'
        ));
        assert!(!matches!(
            linux::read_process_stat(nested).unwrap().state,
            'T' | 't'
        ));
        group.resume().unwrap();
        PinnedPidNamespace::pin_expected(&process, owner.identity()).unwrap();
        let mut wrong = owner.identity();
        wrong.inode += 1;
        assert!(PinnedPidNamespace::pin_expected(&process, wrong).is_err());
        assert!(owner.wait_for_exit(Duration::ZERO).is_err());
        owner.terminate_and_wait(Duration::from_secs(5)).unwrap();
        owner.wait_for_exit(Duration::ZERO).unwrap();
        // Assert exact descendant exit, not disappearance of reusable numeric
        // PIDs. The init can legitimately remain a zombie until its parent
        // reaps it; that does not imply a live descendant or pending writes.
        linux::wait_pidfd_exit(detached_fd.as_raw_fd(), Instant::now()).unwrap();
        linux::wait_pidfd_exit(nested_fd.as_raw_fd(), Instant::now()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while fixture.0.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "launcher did not reap namespace init"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // After the parent has reaped init, even a retained namespace fd cannot
        // translate PID 1 into a resurrected or reused process incarnation.
        assert_eq!(
            translate(&owner.namespace, NS_GET_PID_FROM_PIDNS, 1).unwrap(),
            None
        );
        assert!(!owner.contains_pid(detached).unwrap());
        assert!(!owner.contains_pid(nested).unwrap());
    }
}

/// Test-only construction of a disposable namespace with a detached and a
/// nested descendant. Host util-linux is fixture construction, not a launch
/// dependency or a substitute for the native probe.
#[cfg(test)]
pub(super) mod test_fixture {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    pub(in crate::process_control) struct NamespaceFixture {
        pub(in crate::process_control) wrapper: Child,
        pub(in crate::process_control) init_pid: u32,
        pub(in crate::process_control) detached: u32,
        pub(in crate::process_control) nested: u32,
    }

    impl Drop for NamespaceFixture {
        fn drop(&mut self) {
            let _ = self.wrapper.kill();
            let _ = self.wrapper.wait();
        }
    }

    pub(in crate::process_control) fn children(pid: u32) -> Vec<u32> {
        std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
            .unwrap()
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    pub(in crate::process_control) fn spawn() -> NamespaceFixture {
        let mut wrapper = Command::new("unshare")
            .args([
                "--user", "--map-root-user", "--pid", "--fork", "--kill-child=KILL",
                "/bin/sh", "-c",
                "setsid unshare --pid --fork --kill-child=KILL /bin/sh -c 'echo ready; exec sleep 60' & wait",
            ])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = wrapper.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(output).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            "ready\n"
        );
        reader.join().unwrap();
        let init_pid = children(wrapper.id())[0];
        let detached = children(init_pid)[0];
        let nested = children(detached)[0];
        NamespaceFixture {
            wrapper,
            init_pid,
            detached,
            nested,
        }
    }
}
