//! Descriptor-pinned control of exact Linux processes and namespace lifetime.
//!
//! Higher layers may retain durable process coordinates, but `/proc`
//! enumeration, birth-identity verification, pidfds, group signalling, and
//! bounded settle waits are kernel mechanics.  Keeping them here prevents an
//! application service from growing a second, numeric-PID process authority.

use std::ffi::OsStr;
use std::time::Duration;

#[cfg(target_os = "linux")]
mod cgroup;
// This is the diagnostic witness that a namespace can outlive a process
// group. It is not a second production lifecycle backend: membership scans
// cannot provide the kernel freeze barrier required by workspace capture.
#[cfg(all(test, target_os = "linux"))]
mod pid_namespace;
mod scope;
pub use scope::{
    ControllerAccount, ProcessHostLifetime, ProcessScope, ProcessScopeAllocation,
    ProcessScopeCapability, ProcessScopeConfiguration, ProcessScopeLaunchError,
    ProcessScopeProvider, ProcessScopeRecovery, QuiescedProcessScope, require_administrator,
};

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

/// Durable coordinates required to recover one exact process incarnation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactProcessIdentity {
    pub boot_id: String,
    pub target_pid: u32,
    pub target_start_time_ticks: u64,
    pub group_leader_pid: u32,
    pub group_leader_start_time_ticks: u64,
}

/// Best-effort liveness classification for diagnostic metadata. This never
/// returns signal authority and must not be used to authorize a lifecycle
/// mutation; callers that need that use an authenticated/pinned process.
pub fn diagnostic_process_is_live(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(pidfd) = linux::open_pidfd(pid) else {
            return false;
        };
        linux::pidfd_signal(pidfd.as_raw_fd(), 0, 0).is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// Best-effort executable-name classification fenced by one live pidfd.
/// The result remains diagnostics only: the retained descriptor does not
/// escape Lillux and cannot be promoted into termination authority.
pub fn diagnostic_process_matches_executable_name(pid: u32, expected: &OsStr) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let Ok(pidfd) = linux::open_pidfd(pid) else {
            return false;
        };
        if linux::pidfd_signal(pidfd.as_raw_fd(), 0, 0).is_err() {
            return false;
        }
        let Ok(executable) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
            return false;
        };
        let Some(name) = executable.file_name() else {
            return false;
        };
        let bytes = name.as_bytes();
        let bytes = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
        bytes == expected.as_bytes() && linux::pidfd_signal(pidfd.as_raw_fd(), 0, 0).is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, expected);
        false
    }
}

/// Capture an exact birth/group coordinate using a freshly opened kernel
/// authority. Applications that already hold an authenticated peer or launch
/// authority use their corresponding Lillux method instead.
pub fn capture_exact_process_identity(
    target_pid: u32,
    expected_group_leader: Option<u32>,
) -> Result<ExactProcessIdentity, String> {
    #[cfg(target_os = "linux")]
    {
        let pidfd = linux::open_pidfd(target_pid)?;
        use std::os::fd::AsFd as _;
        linux::capture_from_pidfd(target_pid, expected_group_leader, pidfd.as_fd())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (target_pid, expected_group_leader);
        Err("exact process identity capture is unavailable on this OS".to_owned())
    }
}

/// Kernel consumers which already retain the exact target descriptor call
/// this internally; no application imports a pidfd or procfs branch.
#[cfg(target_os = "linux")]
pub(crate) fn capture_exact_process_identity_from_pidfd(
    target_pid: u32,
    expected_group_leader: Option<u32>,
    target_pidfd: std::os::fd::BorrowedFd<'_>,
) -> Result<ExactProcessIdentity, String> {
    linux::capture_from_pidfd(target_pid, expected_group_leader, target_pidfd)
}

/// One completed process barrier, retaining whichever exact lifecycle
/// authority admission selected. Applications need no backend or OS branch.
pub struct QuiescedProcesses {
    authority: QuiescedAuthority,
}

enum QuiescedAuthority {
    Group(QuiescedProcessGroup),
    Scope(QuiescedProcessScope),
}

impl From<QuiescedProcessGroup> for QuiescedProcesses {
    fn from(authority: QuiescedProcessGroup) -> Self {
        Self {
            authority: QuiescedAuthority::Group(authority),
        }
    }
}

impl From<QuiescedProcessScope> for QuiescedProcesses {
    fn from(authority: QuiescedProcessScope) -> Self {
        Self {
            authority: QuiescedAuthority::Scope(authority),
        }
    }
}

impl QuiescedProcesses {
    pub fn resume(self, timeout: Duration) -> Result<(), String> {
        match self.authority {
            QuiescedAuthority::Group(group) => group.resume(),
            QuiescedAuthority::Scope(scope) => scope.resume(timeout),
        }
    }

    pub fn resume_or_terminate(self, timeout: Duration) -> Result<(), String> {
        match self.authority {
            QuiescedAuthority::Group(group) => group.resume_or_terminate(timeout),
            QuiescedAuthority::Scope(scope) => scope.resume_or_terminate(timeout),
        }
    }

    pub fn terminate(self, timeout: Duration) -> Result<(), String> {
        match self.authority {
            QuiescedAuthority::Group(group) => group.terminate(timeout),
            QuiescedAuthority::Scope(scope) => scope.terminate(timeout),
        }
    }
}

/// A descriptor-pinned set whose live members have all crossed the stop
/// barrier. Dropping an unsettled set resumes those exact members. Consuming
/// it through `terminate` keeps that recovery armed until every retained
/// member is proved dead or each surviving member has been resumed.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct QuiescedProcessGroup {
    members: Vec<PinnedMember>,
    settled: bool,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct QuiescedProcessGroup;

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct PinnedMember {
    pid: u32,
    pidfd: std::os::fd::OwnedFd,
}

impl ExactProcessIdentity {
    fn validate(&self) -> Result<(), String> {
        if self.boot_id.trim().is_empty()
            || self.target_pid <= 1
            || self.group_leader_pid <= 1
            || self.target_start_time_ticks == 0
            || self.group_leader_start_time_ticks == 0
        {
            return Err("exact process identity is incomplete".to_owned());
        }
        Ok(())
    }
}

/// Prepare this controller for the existing exact process-group lifecycle.
/// This is a startup operation, not a read-only health check: it establishes
/// the caller's own group when necessary, then probes group signaling through
/// a retained pidfd with signal zero. No workload or foreign group is signaled.
///
/// This does NOT provision or qualify whole-execution scopes. Those require
/// an explicitly authorized `ProcessScopeProvider` and its placement probe;
/// group support can never stand in for unavailable scope capabilities.
pub fn prepare_process_group_controller() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let pid = unsafe { libc::getpid() };
        if unsafe { libc::getpgrp() } != pid {
            if unsafe { libc::setpgid(0, 0) } != 0 {
                return Err(format!(
                    "place process controller in its own group: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if unsafe { libc::getpgrp() } != pid {
                return Err("process controller did not acquire its exact own group".to_owned());
            }
        }
        let pidfd = linux::open_pidfd(pid as u32)?;
        linux::pidfd_signal(pidfd.as_raw_fd(), 0, libc::PIDFD_SIGNAL_PROCESS_GROUP)
            .map_err(|error| format!("exact process-group signaling is unavailable: {error}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("exact process-group control is unavailable on this OS".to_owned())
    }
}

/// Stop the exact process group and return only after every live member is
/// descriptor-pinned and observably stopped. Signal delivery alone is not a
/// filesystem-freeze barrier.
///
/// This is NOT a descendant-tree primitive. Its callers must retain the
/// admitted no-group-escape contract; a private PID namespace does not make
/// group membership complete. Escapable groups require the admitted
/// [`ProcessScope`] kernel barrier, never a namespace-membership scan.
pub fn quiesce_exact_process_group(
    identity: &ExactProcessIdentity,
    timeout: Duration,
) -> Result<QuiescedProcessGroup, String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (identity, timeout);
        Err("exact process-group quiescence requires Linux pidfds and procfs".to_owned())
    }

    #[cfg(target_os = "linux")]
    {
        linux::quiesce(identity, timeout)
    }
}

#[cfg(target_os = "linux")]
impl QuiescedProcessGroup {
    /// Resume every exact member retained by the stop barrier.
    pub fn resume(mut self) -> Result<(), String> {
        let result = resume_members(&self.members);
        if result.is_ok() {
            self.settled = true;
        }
        result
    }

    /// Resume every retained member, or fail closed by terminating the exact
    /// set when complete resume cannot be proved. This is the release primitive
    /// for durable workspace barriers: returning while abandoning a possibly
    /// stopped group would wedge the workspace and lose process authority.
    pub fn resume_or_terminate(mut self, timeout: Duration) -> Result<(), String> {
        match resume_members(&self.members) {
            Ok(()) => {
                self.settled = true;
                Ok(())
            }
            Err(resume_error) => match self.terminate(timeout) {
                Ok(()) => Err(format!(
                    "exact process-group resume was not proved; group was terminated instead: {resume_error}"
                )),
                Err(terminate_error) => Err(format!(
                    "exact process-group resume was not proved ({resume_error}); fallback termination was not proved ({terminate_error})"
                )),
            },
        }
    }

    /// Irrevocably terminate every exact member and prove exit. If a kernel
    /// failure prevents complete termination, every surviving pinned member
    /// is resumed before this authority is released.
    pub fn terminate(mut self, timeout: Duration) -> Result<(), String> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "exact-process termination deadline overflow".to_owned())?;
        let mut failures = Vec::new();
        for member in &self.members {
            match linux::pidfd_signal(member.pidfd.as_raw_fd(), libc::SIGKILL, 0) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                Err(error) => failures.push(format!(
                    "terminate exact process {} through pidfd: {error}",
                    member.pid
                )),
            }
        }
        for member in &self.members {
            if let Err(error) = linux::wait_pidfd_exit(member.pidfd.as_raw_fd(), deadline) {
                failures.push(format!(
                    "prove exact process {} terminated: {error}",
                    member.pid
                ));
            }
        }
        if failures.is_empty() {
            self.settled = true;
            return Ok(());
        }
        // `terminate` consumes the authority, so an error must not abandon a
        // still-stopped exact member. ESRCH members are already dead; every
        // survivor that could not be killed is resumed. Leaving `settled`
        // false on a resume error also makes Drop retry the same exact set.
        match resume_members(&self.members) {
            Ok(()) => self.settled = true,
            Err(error) => failures.push(format!(
                "resume surviving exact members after incomplete termination: {error}"
            )),
        }
        Err(format!(
            "exact process-set termination was not completely proved: {}",
            failures.join("; ")
        ))
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
    }
}

#[cfg(not(target_os = "linux"))]
impl QuiescedProcessGroup {
    pub fn resume(self) -> Result<(), String> {
        Err("exact process-group resume requires Linux pidfds".to_owned())
    }

    pub fn resume_or_terminate(self, _timeout: Duration) -> Result<(), String> {
        Err("exact process-group resume requires Linux pidfds".to_owned())
    }

    pub fn terminate(self, _timeout: Duration) -> Result<(), String> {
        Err("exact process-group termination requires Linux pidfds".to_owned())
    }

    pub fn member_count(&self) -> usize {
        0
    }
}

#[cfg(target_os = "linux")]
impl Drop for QuiescedProcessGroup {
    fn drop(&mut self) {
        if !self.settled {
            let _ = resume_members(&self.members);
        }
    }
}

#[cfg(target_os = "linux")]
fn resume_members<'a>(members: impl IntoIterator<Item = &'a PinnedMember>) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    let mut failures = Vec::new();
    for member in members {
        match linux::pidfd_signal(member.pidfd.as_raw_fd(), libc::SIGCONT, 0) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => failures.push(format!("{}: {error}", member.pid)),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "resume exact quiesced process members failed: {}",
            failures.join(", ")
        ))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{BTreeMap, BTreeSet};
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::time::{Duration, Instant};

    use super::{ExactProcessIdentity, PinnedMember, QuiescedProcessGroup, resume_members};

    const QUIESCE_POLL_INTERVAL: Duration = Duration::from_millis(5);

    #[derive(Debug, Clone, Copy)]
    pub(super) struct ProcessStat {
        pub(super) state: char,
        pub(super) process_group: i64,
        pub(super) start_time_ticks: u64,
    }

    pub(super) fn quiesce(
        identity: &ExactProcessIdentity,
        timeout: Duration,
    ) -> Result<QuiescedProcessGroup, String> {
        identity.validate()?;
        let current_pid = std::process::id();
        let current_group = unsafe { libc::getpgrp() };
        if identity.target_pid == current_pid
            || i64::from(identity.group_leader_pid) == i64::from(current_group)
        {
            return Err("exact process identity aliases the calling process or group".to_owned());
        }
        if read_boot_id()? != identity.boot_id {
            return Err("exact process identity belongs to another boot".to_owned());
        }
        let leader = pin_expected(
            identity.group_leader_pid,
            identity.group_leader_start_time_ticks,
            i64::from(identity.group_leader_pid),
        )?;
        let target = pin_expected(
            identity.target_pid,
            identity.target_start_time_ticks,
            i64::from(identity.group_leader_pid),
        )?;
        pidfd_signal(
            leader.pidfd.as_raw_fd(),
            libc::SIGSTOP,
            libc::PIDFD_SIGNAL_PROCESS_GROUP,
        )
        .map_err(|error| format!("stop exact process group through leader pidfd: {error}"))?;
        let mut retained = BTreeMap::new();
        retained.insert(leader.pid, leader);
        retained.insert(target.pid, target);

        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "process-group quiescence deadline overflow".to_owned())?;
        let mut prior_stopped_members = None;
        loop {
            match pin_stopped_members(identity, &mut retained) {
                Ok((true, observed)) if prior_stopped_members.as_ref() == Some(&observed) => {
                    return Ok(QuiescedProcessGroup {
                        members: retained.into_values().collect(),
                        settled: false,
                    });
                }
                Ok((true, observed)) => prior_stopped_members = Some(observed),
                Ok((false, _)) => prior_stopped_members = None,
                Err(error) => {
                    let cleanup = resume_failed_quiesce(&retained, identity.group_leader_pid);
                    return Err(match cleanup {
                        Ok(()) => error,
                        Err(cleanup) => {
                            format!("{error}; stopped-group recovery failed: {cleanup}")
                        }
                    });
                }
            }
            if Instant::now() >= deadline {
                let error = format!(
                    "process group {} did not reach a descriptor-pinned stopped state",
                    identity.group_leader_pid
                );
                let cleanup = resume_failed_quiesce(&retained, identity.group_leader_pid);
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; stopped-group recovery failed: {cleanup}"),
                });
            }
            std::thread::sleep(QUIESCE_POLL_INTERVAL);
        }
    }

    fn pin_stopped_members(
        identity: &ExactProcessIdentity,
        retained: &mut BTreeMap<u32, PinnedMember>,
    ) -> Result<(bool, BTreeSet<u32>), String> {
        let mut observed_group = false;
        let mut observed_live = BTreeSet::new();
        let mut all_stopped = true;
        let entries = std::fs::read_dir("/proc")
            .map_err(|error| format!("enumerate procfs process group: {error}"))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("enumerate procfs member: {error}"))?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let stat = match read_process_stat(pid) {
                Ok(stat) => stat,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("inspect process-group member {pid}: {error}")),
            };
            if stat.process_group != i64::from(identity.group_leader_pid) {
                continue;
            }
            observed_group = true;
            if matches!(stat.state, 'Z' | 'X') {
                continue;
            }
            observed_live.insert(pid);
            if !retained.contains_key(&pid) {
                let member = match pin_expected(pid, stat.start_time_ticks, stat.process_group) {
                    Ok(member) => member,
                    Err(error) if error.contains("vanished") || error.contains("changed") => {
                        return Ok((false, observed_live));
                    }
                    Err(error) => return Err(error),
                };
                retained.insert(pid, member);
            }
            let after = match read_process_stat(pid) {
                Ok(after) => after,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok((false, observed_live));
                }
                Err(error) => {
                    return Err(format!(
                        "reinspect pinned process-group member {pid}: {error}"
                    ));
                }
            };
            if after.start_time_ticks != stat.start_time_ticks
                || after.process_group != stat.process_group
            {
                return Ok((false, observed_live));
            }
            all_stopped &= matches!(after.state, 'T' | 't');
        }
        if !observed_group {
            return Err("exact process group vanished before the stop barrier".to_owned());
        }
        // A fresh `/proc` enumeration must observe the same stopped live set
        // twice before authority is returned. This closes the fork-before-
        // SIGSTOP race where the first directory snapshot omits a newly
        // created (but now stopped) group member.
        Ok((all_stopped, observed_live))
    }

    fn resume_failed_quiesce(
        retained: &BTreeMap<u32, PinnedMember>,
        group_leader_pid: u32,
    ) -> Result<(), String> {
        let mut group_failures = Vec::new();
        let mut group_resumed = false;
        // Any still-live pinned member is a safe process-group authority. Try
        // every retained member rather than relying on the leader remaining
        // live after SIGSTOP; a leader pidfd may legitimately report ESRCH
        // while stopped descendants still require recovery.
        for member in retained.values() {
            match pidfd_signal(
                member.pidfd.as_raw_fd(),
                libc::SIGCONT,
                libc::PIDFD_SIGNAL_PROCESS_GROUP,
            ) {
                Ok(()) => {
                    group_resumed = true;
                    break;
                }
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                Err(error) => group_failures.push(format!("{}: {error}", member.pid)),
            }
        }
        let member_result = resume_members(retained.values());
        if group_resumed {
            return member_result;
        }
        let group = if group_failures.is_empty() {
            format!("all retained pidfds, including leader {group_leader_pid}, had exited")
        } else {
            group_failures.join(", ")
        };
        match member_result {
            Ok(()) => Err(format!(
                "could not prove process-group resume through any retained exact member: {group}"
            )),
            Err(members) => Err(format!(
                "group resume failed: {group}; exact-member resume failed: {members}"
            )),
        }
    }

    fn pin_expected(
        pid: u32,
        expected_start_time_ticks: u64,
        expected_process_group: i64,
    ) -> Result<PinnedMember, String> {
        let before = read_process_stat(pid)
            .map_err(|error| format!("exact process {pid} vanished before pin: {error}"))?;
        if matches!(before.state, 'Z' | 'X') {
            return Err(format!("exact process {pid} exited before pin"));
        }
        if before.start_time_ticks != expected_start_time_ticks
            || before.process_group != expected_process_group
        {
            return Err(format!(
                "exact process {pid} birth identity changed before pin"
            ));
        }
        let pidfd = open_pidfd(pid)?;
        pidfd_signal(pidfd.as_raw_fd(), 0, 0)
            .map_err(|error| format!("probe exact process {pid} pidfd: {error}"))?;
        let after = read_process_stat(pid)
            .map_err(|error| format!("exact process {pid} vanished after pin: {error}"))?;
        if matches!(after.state, 'Z' | 'X')
            || before.start_time_ticks != after.start_time_ticks
            || before.process_group != after.process_group
        {
            return Err(format!(
                "exact process {pid} birth identity changed while pinned"
            ));
        }
        Ok(PinnedMember { pid, pidfd })
    }

    pub(super) fn read_boot_id() -> Result<String, String> {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|value| value.trim().to_owned())
            .map_err(|error| format!("read Linux boot identity: {error}"))
    }

    pub(super) fn capture_from_pidfd(
        target_pid: u32,
        expected_group_leader: Option<u32>,
        target_pidfd: BorrowedFd<'_>,
    ) -> Result<ExactProcessIdentity, String> {
        if target_pid <= 1 {
            return Err("exact target PID is unsafe".to_owned());
        }
        pidfd_signal(target_pidfd.as_raw_fd(), 0, 0)
            .map_err(|error| format!("probe exact target pidfd: {error}"))?;
        let target = read_process_stat(target_pid)
            .map_err(|error| format!("read exact target process stat: {error}"))?;
        if matches!(target.state, 'Z' | 'X') {
            return Err("exact target exited during identity capture".to_owned());
        }
        let group_leader_pid = expected_group_leader.unwrap_or_else(|| target.process_group as u32);
        if group_leader_pid <= 1 || i64::from(group_leader_pid) != target.process_group {
            return Err("exact target is not in the required process group".to_owned());
        }
        let group_pidfd = open_pidfd(group_leader_pid)?;
        pidfd_signal(group_pidfd.as_raw_fd(), 0, 0)
            .map_err(|error| format!("probe exact group leader pidfd: {error}"))?;
        let group = read_process_stat(group_leader_pid)
            .map_err(|error| format!("read exact group leader process stat: {error}"))?;
        if matches!(group.state, 'Z' | 'X') || group.process_group != i64::from(group_leader_pid) {
            return Err("exact group leader changed during identity capture".to_owned());
        }
        // Re-probe after every numeric /proc observation. This is what makes
        // the retained descriptor, rather than a recycled PID, authoritative.
        pidfd_signal(target_pidfd.as_raw_fd(), 0, 0)
            .map_err(|error| format!("reprobe exact target pidfd: {error}"))?;
        pidfd_signal(group_pidfd.as_raw_fd(), 0, 0)
            .map_err(|error| format!("reprobe exact group leader pidfd: {error}"))?;
        Ok(ExactProcessIdentity {
            boot_id: read_boot_id()?,
            target_pid,
            target_start_time_ticks: target.start_time_ticks,
            group_leader_pid,
            group_leader_start_time_ticks: group.start_time_ticks,
        })
    }

    pub(super) fn read_process_stat(pid: u32) -> std::io::Result<ProcessStat> {
        let raw = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let close = raw.rfind(')').ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed procfs stat comm",
            )
        })?;
        let fields: Vec<_> = raw[close + 1..].split_whitespace().collect();
        let state = fields
            .first()
            .and_then(|value| value.chars().next())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "missing procfs state")
            })?;
        let process_group = fields
            .get(2)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "missing procfs group")
            })?
            .parse::<i64>()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let start_time_ticks = fields
            .get(19)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "missing procfs birth")
            })?
            .parse::<u64>()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if start_time_ticks == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "zero procfs birth identity",
            ));
        }
        Ok(ProcessStat {
            state,
            process_group,
            start_time_ticks,
        })
    }

    pub(super) fn open_pidfd(pid: u32) -> Result<OwnedFd, String> {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
        if raw < 0 {
            return Err(format!(
                "open exact process {pid} pidfd: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: pidfd_open returned one new uniquely owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    pub(super) fn pidfd_signal(pidfd: i32, signal: i32, flags: u32) -> std::io::Result<()> {
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
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    pub(super) fn wait_pidfd_exit(pidfd: i32, deadline: Instant) -> Result<(), String> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let mut pollfd = libc::pollfd {
                fd: pidfd,
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if result > 0 && pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                return Ok(());
            }
            if result == 0 {
                return Err("exact process did not exit before deadline".to_owned());
            }
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(format!("poll exact process pidfd: {error}"));
            }
            return Err(format!(
                "exact process pidfd reported unexpected events {:#x}",
                pollfd.revents
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_identity_is_rejected_before_os_contact() {
        let identity = ExactProcessIdentity {
            boot_id: String::new(),
            target_pid: 0,
            target_start_time_ticks: 0,
            group_leader_pid: 0,
            group_leader_start_time_ticks: 0,
        };
        assert!(quiesce_exact_process_group(&identity, Duration::ZERO).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn diagnostic_process_observation_stays_name_bounded() {
        let pid = std::process::id();
        assert!(diagnostic_process_is_live(pid));
        let executable = std::env::current_exe().unwrap();
        assert!(diagnostic_process_matches_executable_name(
            pid,
            executable.file_name().unwrap()
        ));
        assert!(!diagnostic_process_matches_executable_name(
            pid,
            OsStr::new("definitely-not-this-process")
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn libc_process_group_pidfd_flag_matches_linux_uapi() {
        assert_eq!(libc::PIDFD_SIGNAL_PROCESS_GROUP, 1_u32 << 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_group_controller_preparation_is_idempotent_and_child_local() {
        use std::os::unix::process::CommandExt;
        const CHILD: &str = "LILLUX_TEST_CONTROLLER_PREPARATION";
        if let Ok(mode) = std::env::var(CHILD) {
            let pid = std::process::id() as i32;
            assert_eq!(unsafe { libc::getpgrp() } == pid, mode == "leader");
            prepare_process_group_controller().unwrap();
            assert_eq!(unsafe { libc::getpgrp() }, pid);
            prepare_process_group_controller().unwrap();
            assert_eq!(unsafe { libc::getpgrp() }, pid);
            return;
        }
        let caller_group = unsafe { libc::getpgrp() };
        for mode in ["inherited", "leader"] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "process_control::tests::process_group_controller_preparation_is_idempotent_and_child_local",
                    "--nocapture",
                ])
                .env_clear()
                .env(CHILD, mode);
            if mode == "leader" {
                command.process_group(0);
            }
            assert!(command.status().unwrap().success(), "{mode}");
            assert_eq!(unsafe { libc::getpgrp() }, caller_group);
        }
    }
}
