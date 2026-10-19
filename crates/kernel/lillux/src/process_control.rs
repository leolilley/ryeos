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

/// Locked-memory ceiling a host supervisor grants a node controller at exec.
/// The only memory a controller locks is its resident native region, one
/// fixed compile-time layout of about 64 MiB; four times that leaves room for
/// layout growth while keeping the lockable memory every spawned worker
/// inherits far below a node's memory envelope. Locked pages are still
/// charged to the envelope's memory cgroup.
pub const CONTROLLER_MEMLOCK_CEILING_BYTES: u64 = 256 * 1024 * 1024;
/// Descriptor soft ceiling for a node controller; the inherited hard limit
/// still bounds it, and no host-wide maximum is raised.
pub const CONTROLLER_NOFILE_SOFT_CEILING: u64 = 65_536;
mod host_capability;
mod oci_lifecycle;
// This is the diagnostic witness that a namespace can outlive a process
// group. It is not a second production lifecycle backend: membership scans
// cannot provide the kernel freeze barrier required by workspace capture.
#[cfg(all(test, target_os = "linux"))]
mod pid_namespace;
mod scope;
pub use host_capability::{
    ACCOUNTING, CapabilityEvidence, CapabilityRequirement, CapabilityRequirementSpec, CgroupFacts,
    CgroupLimit, DirectoryStepFacts, HOST_FACTS_VERSION, HOST_PROVIDER, HostCapabilities,
    HostFacts, HostLifetimeBound, ISOLATION_NAMESPACES, MAX_CAPABILITY_ENTRIES,
    MAX_CAPABILITY_NAME_BYTES, MAX_CAPABILITY_TEXT_BYTES, MemlockFacts, PERSISTENCE_DURABLE_STATE,
    PrivateDirectoryFacts, ProbeOutcome, ProofSource, RECOVERY_ENCLOSURE,
    RECOVERY_ENCLOSURE_TRUSTED_INIT, RESIDENCY_CAPABILITIES, RESIDENCY_LOCKED,
    RESIDENCY_SWAP_ABSENT, RESIDENCY_SWAP_LIMIT, ResidencyProof, RootStorageFacts, SCOPE_LIMITS,
    STORAGE_DURABLE_ROOT, TERMINATION_ENCLOSURE, TERMINATION_ENCLOSURE_TRUSTED_INIT,
    admit_enclosure_inits, current_host_generation, decide_durable_root, decide_durable_state,
    decide_private_directory, floor_entry_refusal, is_scope_capability, is_weaker_grade,
    met_by_strongest, satisfies,
};
pub use oci_lifecycle::{
    OciHookState, OciLifecycleGeneration, OciLifecycleIntent, OciPreparationPhase,
    OciPreparationProgress,
};
#[cfg(target_os = "linux")]
pub(crate) use scope::SelectedScopePreparation;
pub use scope::{
    ControllerAccount, ProbeBarrier, ProcessHostLifetime, ProcessScope, ProcessScopeAllocation,
    ProcessScopeCapability, ProcessScopeConfiguration, ProcessScopeLaunchError,
    ProcessScopeProvider, ProcessScopeRecovery, ProcessScopeResourceLimits, QuiescedProcessScope,
    ResourceLimitedProcessScope, require_administrator,
};

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

/// Durable coordinates required to recover one exact process incarnation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactProcessIdentity {
    pub boot_id: String,
    pub target_pid: u32,
    pub target_start_time_ticks: u64,
    pub group_leader_pid: u32,
    pub group_leader_start_time_ticks: u64,
}

/// Descriptor-owned access to one exact live process's filesystem and mount
/// namespace. This is the only Lillux operation permitted to follow procfs's
/// `root` and `ns/mnt` magic links. Ordinary secure filesystem traversal keeps
/// rejecting every symlink component.
///
/// The pidfd and proc directory remain owned for the complete lifetime of this
/// value. Every descendant is subsequently opened no-follow from the retained
/// root descriptor, and the process incarnation is checked on both sides of
/// magic-link acquisition.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct ExactProcessRoot {
    identity: ExactProcessIdentity,
    pidfd: std::os::fd::OwnedFd,
    process_directory: crate::PinnedDirectory,
    root: crate::PinnedDirectory,
    root_identity: crate::PinnedDirectoryIdentity,
    namespace_directory: crate::PinnedDirectory,
    mount_namespace: std::fs::File,
    mount_namespace_identity: (u64, u64),
    membership: std::fs::File,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct ExactProcessRoot;

impl ExactProcessRoot {
    #[cfg(target_os = "linux")]
    fn open(identity: &ExactProcessIdentity) -> Result<Self, String> {
        use std::os::fd::AsFd as _;

        identity.validate()?;
        let pidfd = linux::open_pidfd(identity.target_pid)?;
        let observed = linux::capture_from_pidfd(
            identity.target_pid,
            Some(identity.group_leader_pid),
            pidfd.as_fd(),
        )?;
        if observed != *identity {
            return Err("process incarnation changed before process-root acquisition".to_owned());
        }

        let proc = crate::PinnedDirectory::open(std::path::Path::new("/proc"))
            .map_err(|error| format!("open procfs root: {error}"))?
            .ok_or("procfs root is absent")?;
        let pid_name = identity.target_pid.to_string();
        let process = proc
            .open_child_directory(std::ffi::OsStr::new(&pid_name))
            .map_err(|error| format!("open exact proc process directory: {error}"))?
            .ok_or("exact proc process directory is absent")?;
        let process_fd = process
            .try_clone_descriptor()
            .map_err(|error| format!("retain exact proc process directory: {error}"))?;

        // These are procfs magic links. Following them is intentionally local
        // to this exact-process primitive and is fenced by the retained pidfd.
        let root_file = open_proc_magic_file(
            &process_fd,
            c"root",
            libc::O_PATH | libc::O_DIRECTORY,
            "process root",
        )?;
        let root = crate::PinnedDirectory::from_open_directory(
            std::path::PathBuf::from(format!("/proc/{}/root", identity.target_pid)),
            root_file,
        )
        .map_err(|error| format!("pin exact process root: {error}"))?;
        let root_identity = root
            .identity()
            .map_err(|error| format!("identify exact process root: {error}"))?;

        let namespace_directory = process
            .open_child_directory(std::ffi::OsStr::new("ns"))
            .map_err(|error| format!("open exact process namespace directory: {error}"))?
            .ok_or("exact process namespace directory is absent")?;
        let namespace_fd = namespace_directory
            .try_clone_descriptor()
            .map_err(|error| format!("retain exact process namespace directory: {error}"))?;
        let mount_namespace =
            open_proc_magic_file(&namespace_fd, c"mnt", libc::O_RDONLY, "mount namespace")?;
        use std::os::unix::fs::MetadataExt as _;
        let namespace_metadata = mount_namespace
            .metadata()
            .map_err(|error| format!("identify exact process mount namespace: {error}"))?;
        let mount_namespace_identity = (namespace_metadata.dev(), namespace_metadata.ino());
        let membership = process
            .open_regular(std::ffi::OsStr::new("cgroup"), false)
            .map_err(|error| format!("open exact process cgroup membership: {error}"))?
            .ok_or("exact process cgroup membership is absent")?;

        let observed = linux::capture_from_pidfd(
            identity.target_pid,
            Some(identity.group_leader_pid),
            pidfd.as_fd(),
        )?;
        if observed != *identity {
            return Err("process incarnation changed during process-root acquisition".to_owned());
        }
        Ok(Self {
            identity: identity.clone(),
            pidfd,
            process_directory: process,
            root,
            root_identity,
            namespace_directory,
            mount_namespace,
            mount_namespace_identity,
            membership,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn open(_identity: &ExactProcessIdentity) -> Result<Self, String> {
        Err("exact process-root access is unavailable on this OS".to_owned())
    }

    pub fn require_live(&self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsFd as _;
            // Keep the proc directory itself in the liveness proof: losing its
            // backing process while retaining only derived descriptors must
            // not be interpreted as a reusable numeric PID.
            self.process_directory
                .try_clone_descriptor()
                .map_err(|error| format!("retain exact proc process directory: {error}"))?;
            let observed = linux::capture_from_pidfd(
                self.identity.target_pid,
                Some(self.identity.group_leader_pid),
                self.pidfd.as_fd(),
            )?;
            if observed != self.identity {
                return Err("exact process-root owner changed incarnation".to_owned());
            }
            let process = self
                .process_directory
                .try_clone_descriptor()
                .map_err(|error| format!("retain exact proc process directory: {error}"))?;
            let current_root = open_proc_magic_file(
                &process,
                c"root",
                libc::O_PATH | libc::O_DIRECTORY,
                "process root",
            )?;
            let current_root = crate::PinnedDirectory::from_open_directory(
                self.root.path().to_path_buf(),
                current_root,
            )
            .map_err(|error| format!("repin exact process root: {error}"))?;
            if current_root
                .identity()
                .map_err(|error| format!("reidentify exact process root: {error}"))?
                != self.root_identity
            {
                return Err("exact process filesystem root changed".to_owned());
            }
            let namespace = self
                .namespace_directory
                .try_clone_descriptor()
                .map_err(|error| format!("retain exact process namespace directory: {error}"))?;
            let current_namespace =
                open_proc_magic_file(&namespace, c"mnt", libc::O_RDONLY, "mount namespace")?;
            use std::os::unix::fs::MetadataExt as _;
            let current_namespace = current_namespace
                .metadata()
                .map_err(|error| format!("reidentify exact process mount namespace: {error}"))?;
            if (current_namespace.dev(), current_namespace.ino()) != self.mount_namespace_identity {
                return Err("exact process mount namespace changed".to_owned());
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        Err("exact process-root access is unavailable on this OS".to_owned())
    }

    /// Re-probe only the retained pidfd. This remains valid after the hook has
    /// entered a mount namespace whose procfs does not expose the host PID.
    pub fn require_retained_lifetime(&self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            linux::pidfd_signal(self.pidfd.as_raw_fd(), 0, 0)
                .map_err(|error| format!("probe retained process-root lifetime: {error}"))
        }
        #[cfg(not(target_os = "linux"))]
        Err("exact process-root access is unavailable on this OS".to_owned())
    }

    /// Open a normalized relative directory below the pinned process root.
    /// Every descendant component remains no-follow.
    pub fn open_directory(
        &self,
        relative: &std::path::Path,
    ) -> Result<crate::PinnedDirectory, String> {
        self.require_live()?;
        if relative.is_absolute() || relative.as_os_str().is_empty() {
            return Err("process-root descendant must be a nonempty relative path".to_owned());
        }
        let mut directory = self
            .root
            .try_clone()
            .map_err(|error| format!("retain exact process root: {error}"))?;
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err("process-root descendant path is not canonical".to_owned());
            };
            directory = directory
                .open_child_directory(name)
                .map_err(|error| format!("open process-root descendant: {error}"))?
                .ok_or("process-root descendant directory is absent")?;
        }
        self.require_live()?;
        Ok(directory)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn mount_namespace(&self) -> Result<std::fs::File, String> {
        self.require_live()?;
        self.mount_namespace
            .try_clone()
            .map_err(|error| format!("retain exact process mount namespace: {error}"))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn membership(&self) -> Result<std::fs::File, String> {
        self.require_live()?;
        self.membership
            .try_clone()
            .map_err(|error| format!("retain exact process cgroup membership: {error}"))
    }

    pub(crate) fn identity(&self) -> &ExactProcessIdentity {
        #[cfg(target_os = "linux")]
        {
            &self.identity
        }
        #[cfg(not(target_os = "linux"))]
        unreachable!("no exact process root exists on this OS")
    }
}

#[cfg(target_os = "linux")]
fn open_proc_magic_file(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
    flags: libc::c_int,
    description: &str,
) -> Result<std::fs::File, String> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let descriptor =
        unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags | libc::O_CLOEXEC) };
    if descriptor < 0 {
        return Err(format!(
            "open exact {description}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
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

/// Capture this process's exact birth and process-group incarnation without
/// exposing platform PID/group discovery to protocol or application code.
pub fn capture_current_process_identity() -> Result<ExactProcessIdentity, String> {
    #[cfg(target_os = "linux")]
    {
        let pid = unsafe { libc::getpid() };
        let group = unsafe { libc::getpgrp() };
        if pid <= 1 || group <= 1 {
            return Err("current process has no valid Linux process identity".to_owned());
        }
        capture_exact_process_identity(pid as u32, Some(group as u32))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("current process identity capture is unavailable on this OS".to_owned())
    }
}

/// Check an already captured original current incarnation without constructing
/// another owned identity. Proc reads and refusals are fixed-size; both pidfds
/// fence the numeric observations exactly as in the owned capture path.
pub fn require_current_process_identity_fixed(
    expected: &ExactProcessIdentity,
) -> Result<(), &'static str> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = expected;
        Err("current process identity check requires Linux")
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        fn read_fixed(path: &[u8], bytes: &mut [u8]) -> Result<usize, &'static str> {
            let raw = unsafe {
                libc::open(
                    path.as_ptr().cast(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if raw < 0 {
                return Err("original process input open refused");
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let mut used = 0;
            loop {
                if used == bytes.len() {
                    let mut extra = 0u8;
                    let result =
                        unsafe { libc::read(fd.as_raw_fd(), (&mut extra as *mut u8).cast(), 1) };
                    if result == 0 {
                        return Ok(used);
                    }
                    return Err("original process input exceeds fixed bound");
                }
                let result = unsafe {
                    libc::read(
                        fd.as_raw_fd(),
                        bytes[used..].as_mut_ptr().cast(),
                        bytes.len() - used,
                    )
                };
                if result < 0 {
                    return Err("original process input read refused");
                }
                if result == 0 {
                    return Ok(used);
                }
                used += result as usize;
                // Positive reads consume at least one byte; at most len+1
                // calls, with no EINTR retry or renewed admission interval.
            }
        }
        fn stat(pid: u32) -> Result<(u8, i64, u64), &'static str> {
            let mut path = [0u8; 40];
            path[..6].copy_from_slice(b"/proc/");
            let mut digits = [0u8; 10];
            let mut count = 0;
            let mut value = pid;
            loop {
                digits[count] = b'0' + (value % 10) as u8;
                count += 1;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            for i in 0..count {
                path[6 + i] = digits[count - 1 - i];
            }
            path[6 + count..11 + count].copy_from_slice(b"/stat");
            let mut bytes = [0u8; 4096];
            let size = read_fixed(&path, &mut bytes)?;
            let text = std::str::from_utf8(&bytes[..size])
                .map_err(|_| "original process stat is not UTF-8")?;
            let end = text
                .rfind(')')
                .ok_or("original process stat has no comm boundary")?;
            let mut fields = text[end + 1..].split_ascii_whitespace();
            let state = fields
                .next()
                .and_then(|v| {
                    if v.len() == 1 {
                        Some(v.as_bytes()[0])
                    } else {
                        None
                    }
                })
                .ok_or("original process stat has no state")?;
            fields.next().ok_or("original process stat has no parent")?;
            let group = fields
                .next()
                .ok_or("original process stat has no group")?
                .parse::<i64>()
                .map_err(|_| "original process stat has invalid group")?;
            let birth = fields
                .nth(16)
                .ok_or("original process stat has no birth")?
                .parse::<u64>()
                .map_err(|_| "original process stat has invalid birth")?;
            if birth == 0 {
                return Err("original process stat has zero birth");
            }
            Ok((state, group, birth))
        }
        fn pin(pid: u32) -> Result<OwnedFd, &'static str> {
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
            if raw < 0 {
                return Err("original process pidfd refused");
            }
            Ok(unsafe { OwnedFd::from_raw_fd(raw as i32) })
        }
        fn probe(fd: &OwnedFd) -> Result<(), &'static str> {
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    0i32,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                )
            };
            if result != 0 {
                return Err("original process pidfd probe refused");
            }
            Ok(())
        }
        let pid = unsafe { libc::getpid() };
        let group = unsafe { libc::getpgrp() };
        if pid <= 1
            || group <= 1
            || pid as u32 != expected.target_pid
            || group as u32 != expected.group_leader_pid
        {
            return Err("native admission cannot cross a process incarnation");
        }
        let target_fd = pin(expected.target_pid)?;
        let group_fd = pin(expected.group_leader_pid)?;
        probe(&target_fd)?;
        probe(&group_fd)?;
        let target = stat(expected.target_pid)?;
        let leader = stat(expected.group_leader_pid)?;
        let mut boot = [0u8; 128];
        let size = read_fixed(b"/proc/sys/kernel/random/boot_id\0", &mut boot)?;
        let boot = std::str::from_utf8(&boot[..size])
            .map_err(|_| "original boot identity is not UTF-8")?
            .trim();
        probe(&target_fd)?;
        probe(&group_fd)?;
        if matches!(target.0, b'Z' | b'X')
            || matches!(leader.0, b'Z' | b'X')
            || target.1 != i64::from(expected.group_leader_pid)
            || leader.1 != i64::from(expected.group_leader_pid)
            || target.2 != expected.target_start_time_ticks
            || leader.2 != expected.group_leader_start_time_ticks
            || boot.is_empty()
            || boot != expected.boot_id
        {
            return Err("native admission cannot cross a process incarnation");
        }
        Ok(())
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
    pub(crate) fn validate(&self) -> Result<(), String> {
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

    /// Portable opaque digest for this exact process occurrence. Application
    /// layers may bind evidence to the digest without interpreting kernel
    /// birth coordinates or reproducing their platform-specific semantics.
    pub fn incarnation_digest(&self) -> Result<String, String> {
        let bytes = self.incarnation_digest_bytes().map_err(str::to_owned)?;
        let mut text = String::with_capacity(64);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in bytes {
            text.push(HEX[(byte >> 4) as usize] as char);
            text.push(HEX[(byte & 15) as usize] as char);
        }
        Ok(text)
    }

    /// The same canonical protocol digest as fixed bytes, for an original
    /// admission caller that cannot allocate an intermediate Value or string.
    /// This is identity data, not liveness, resource or execution authority.
    pub fn incarnation_digest_bytes(&self) -> Result<[u8; 32], &'static str> {
        use sha2::Digest;
        if self.boot_id.trim().is_empty()
            || self.target_pid <= 1
            || self.group_leader_pid <= 1
            || self.target_start_time_ticks == 0
            || self.group_leader_start_time_ticks == 0
        {
            return Err("exact process identity is incomplete");
        }
        // Declaration order is the canonical decoded-key order. Borrow every
        // string and preserve integer rendering; no field clone or Value tree.
        #[derive(serde::Serialize)]
        struct Wire<'a> {
            boot_identity: &'a str,
            group_leader_birth: u64,
            group_leader_pid: u32,
            target_birth: u64,
            target_pid: u32,
            version: u32,
        }
        struct HashWriter(sha2::Sha256);
        impl std::io::Write for HashWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // RyeOS canonical strings escape non-ASCII as lowercase UTF-16
        // units. Compact serde already handles ASCII control/quote escapes.
        struct CanonicalStrings;
        impl serde_json::ser::Formatter for CanonicalStrings {
            fn write_string_fragment<W: ?Sized + std::io::Write>(
                &mut self,
                writer: &mut W,
                fragment: &str,
            ) -> std::io::Result<()> {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                for ch in fragment.chars() {
                    if ch.is_ascii() {
                        writer.write_all(&[ch as u8])?;
                    } else {
                        let mut units = [0u16; 2];
                        for unit in ch.encode_utf16(&mut units).iter().copied() {
                            writer.write_all(&[
                                b'\\',
                                b'u',
                                HEX[((unit >> 12) & 15) as usize],
                                HEX[((unit >> 8) & 15) as usize],
                                HEX[((unit >> 4) & 15) as usize],
                                HEX[(unit & 15) as usize],
                            ])?;
                        }
                    }
                }
                Ok(())
            }
        }
        // Exhaustive destructure makes new identity fields require review of
        // this persisted digest instead of silently omitting them.
        let Self {
            boot_id,
            target_pid,
            target_start_time_ticks,
            group_leader_pid,
            group_leader_start_time_ticks,
        } = self;
        let wire = Wire {
            boot_identity: boot_id,
            group_leader_birth: *group_leader_start_time_ticks,
            group_leader_pid: *group_leader_pid,
            target_birth: *target_start_time_ticks,
            target_pid: *target_pid,
            version: 1,
        };
        let mut writer = HashWriter(sha2::Sha256::new());
        let mut serializer = serde_json::Serializer::with_formatter(&mut writer, CanonicalStrings);
        serde::Serialize::serialize(&wire, &mut serializer)
            .map_err(|_| "canonical process identity encoding failed")?;
        Ok(writer.0.finalize().into())
    }

    /// Observe whether this exact process incarnation has ended without ever
    /// treating numeric-PID reuse as liveness. An inspection failure while a
    /// process still occupies the coordinate is indeterminate and therefore
    /// fails closed; only absence or a different captured birth identity is
    /// terminal evidence.
    pub fn has_ended(&self) -> Result<bool, String> {
        self.validate()?;
        #[cfg(target_os = "linux")]
        {
            match capture_exact_process_identity(self.target_pid, Some(self.group_leader_pid)) {
                Ok(current) => Ok(current != *self),
                Err(error) => {
                    if std::path::PathBuf::from(format!("/proc/{}", self.target_pid)).exists() {
                        Err(format!(
                            "cannot classify exact process incarnation: {error}"
                        ))
                    } else {
                        Ok(true)
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err("exact process death observation is unavailable on this OS".to_owned())
        }
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

/// Make the calling process lead its own session and therefore its own
/// process group, detaching it from whatever session and group launched it.
/// Exact current-process identity names the caller's group leader, and group
/// termination signals that whole group, so a process left in a launcher's
/// group would capture a foreign leader and could signal the launcher too.
///
/// A process that already leads its session is unchanged. The kernel refuses
/// a new session to a process that already leads its own group inside another
/// session; that group is already exactly this process's, so it is accepted
/// without detaching. Any other refusal fails closed.
pub fn establish_own_process_session() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let pid = unsafe { libc::getpid() };
        if unsafe { libc::getsid(0) } != pid && unsafe { libc::setsid() } < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EPERM) || unsafe { libc::getpgrp() } != pid {
                return Err(format!("place process in its own session: {error}"));
            }
        }
        if unsafe { libc::getpgrp() } != pid {
            return Err("process did not acquire its exact own process group".to_owned());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("process session placement is unavailable on this OS".to_owned())
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
    #[test]
    fn fixed_process_digest_matches_canonical_protocol() {
        use sha2::Digest;
        for boot in ["01234567-89ab-cdef-0123-456789abcdef", "é🚀\n\t\\\"\u{1b}"] {
            for (pid, ticks) in [(2, 1), (u32::MAX, u64::MAX)] {
                let identity = super::ExactProcessIdentity {
                    boot_id: boot.to_owned(),
                    target_pid: pid,
                    target_start_time_ticks: ticks,
                    group_leader_pid: pid,
                    group_leader_start_time_ticks: ticks,
                };
                let canonical = crate::canonical_json(&serde_json::json!({
                    "version": 1, "boot_identity": boot,
                    "target_pid": pid, "target_birth": ticks,
                    "group_leader_pid": pid, "group_leader_birth": ticks,
                }))
                .unwrap();
                let expected: [u8; 32] = sha2::Sha256::digest(canonical.as_bytes()).into();
                assert_eq!(identity.incarnation_digest_bytes().unwrap(), expected);
                assert_eq!(
                    identity.incarnation_digest().unwrap(),
                    crate::sha256_hex(canonical.as_bytes())
                );
            }
        }
        let invalid = super::ExactProcessIdentity {
            boot_id: " ".into(),
            target_pid: 2,
            target_start_time_ticks: 1,
            group_leader_pid: 2,
            group_leader_start_time_ticks: 1,
        };
        assert!(invalid.incarnation_digest_bytes().is_err());
    }
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
    fn exact_process_death_observation_preserves_birth_identity() {
        use std::os::unix::process::CommandExt as _;

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let identity = capture_exact_process_identity(child.id(), Some(child.id())).unwrap();
        assert!(!identity.has_ended().unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(identity.has_ended().unwrap());
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
    fn exact_process_root_follows_only_the_fenced_proc_magic_link() {
        use std::os::unix::process::CommandExt as _;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let result = (|| -> Result<(), String> {
            let identity = capture_exact_process_identity(child.id(), None)?;
            let root = ExactProcessRoot::open(&identity)?;
            root.require_live()?;
            root.open_directory(std::path::Path::new("tmp"))?;
            assert!(
                root.open_directory(std::path::Path::new("proc/self"))
                    .is_err(),
                "ordinary descendants must still reject symlinks"
            );
            assert!(
                root.open_directory(std::path::Path::new("../tmp")).is_err(),
                "process-root descendants must remain normalized"
            );
            Ok(())
        })();
        let _ = child.kill();
        let _ = child.wait();
        result.unwrap();
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

    #[cfg(target_os = "linux")]
    #[test]
    fn own_session_placement_leaves_the_launcher_group_and_is_idempotent() {
        use std::os::unix::process::CommandExt;
        const CHILD: &str = "LILLUX_TEST_OWN_SESSION_PLACEMENT";
        if let Ok(mode) = std::env::var(CHILD) {
            let pid = std::process::id() as i32;
            let launcher_group = std::env::var("LILLUX_TEST_LAUNCHER_GROUP")
                .unwrap()
                .parse::<i32>()
                .unwrap();
            match mode.as_str() {
                "inherited" => {
                    assert_eq!(unsafe { libc::getpgrp() }, launcher_group);
                    assert_ne!(unsafe { libc::getsid(0) }, pid);
                }
                "group-leader" => {
                    assert_eq!(unsafe { libc::getpgrp() }, pid);
                    assert_ne!(unsafe { libc::getsid(0) }, pid);
                }
                "session-leader" => assert_eq!(unsafe { libc::getsid(0) }, pid),
                other => panic!("unknown placement mode {other}"),
            }
            let session_before = unsafe { libc::getsid(0) };
            for _ in 0..2 {
                establish_own_process_session().unwrap();
                assert_eq!(unsafe { libc::getpgrp() }, pid);
                assert_ne!(unsafe { libc::getpgrp() }, launcher_group);
                if mode == "group-leader" {
                    assert_eq!(unsafe { libc::getsid(0) }, session_before);
                } else {
                    assert_eq!(unsafe { libc::getsid(0) }, pid);
                }
            }
            let identity = capture_current_process_identity().unwrap();
            assert_eq!(identity.group_leader_pid, pid as u32);
            return;
        }
        let caller_group = unsafe { libc::getpgrp() };
        let caller_session = unsafe { libc::getsid(0) };
        for mode in ["inherited", "group-leader", "session-leader"] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "process_control::tests::own_session_placement_leaves_the_launcher_group_and_is_idempotent",
                    "--nocapture",
                ])
                .env_clear()
                .env(CHILD, mode)
                .env("LILLUX_TEST_LAUNCHER_GROUP", caller_group.to_string());
            match mode {
                "group-leader" => {
                    command.process_group(0);
                }
                "session-leader" => unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                },
                _ => {}
            }
            assert!(command.status().unwrap().success(), "{mode}");
            assert_eq!(unsafe { libc::getpgrp() }, caller_group);
            assert_eq!(unsafe { libc::getsid(0) }, caller_session);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod fixed_current_identity_controls {
    #[test]
    fn retained_current_identity_matches_and_rejects_foreign_coordinates() {
        let original =
            super::capture_current_process_identity().expect("capture actual current identity");
        super::require_current_process_identity_fixed(&original).expect("same actual identity");
        let mut foreign = original.clone();
        foreign.target_pid = if original.target_pid == 2 { 3 } else { 2 };
        assert!(super::require_current_process_identity_fixed(&foreign).is_err());
        let mut foreign = original.clone();
        foreign.target_start_time_ticks = original.target_start_time_ticks.wrapping_add(1);
        assert!(super::require_current_process_identity_fixed(&foreign).is_err());
        let mut foreign = original.clone();
        foreign.group_leader_start_time_ticks =
            original.group_leader_start_time_ticks.wrapping_add(1);
        assert!(super::require_current_process_identity_fixed(&foreign).is_err());
        let mut foreign = original;
        foreign.boot_id = "foreign-boot".into();
        assert!(super::require_current_process_identity_fixed(&foreign).is_err());
    }
}
