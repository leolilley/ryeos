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

/// Whether a numeric process group still has members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupPresence {
    /// A member that has not finished exiting, or a finished member this
    /// process still owes a wait for, remains in the group.
    Present,
    /// No member remains except finished members whose parent is another
    /// process; that parent, or init, reaps them.
    Absent,
    /// The pid namespace could not be read or a member's record could not be
    /// parsed, so neither answer is proved. Callers must fail closed.
    Undecidable,
}

/// One process-group member as read from `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GroupMember {
    pid: u32,
    parent_pid: u32,
    state: char,
}

/// Fields of `/proc/<pid>/stat` after the command name, read from raw bytes.
///
/// The name sits between the first `(` and the last `)` and may hold any
/// bytes, including non-UTF-8 ones and parentheses, so only the text after it
/// (state, parent pid, group, and the numeric fields) is interpreted.
pub(crate) fn proc_stat_fields_after_comm(raw: &[u8]) -> Result<Vec<&str>, String> {
    let open = raw
        .iter()
        .position(|byte| *byte == b'(')
        .ok_or("malformed stat comm")?;
    let close = raw
        .iter()
        .rposition(|byte| *byte == b')')
        .filter(|close| *close > open)
        .ok_or("malformed stat comm")?;
    raw[close + 1..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .map(|field| std::str::from_utf8(field).map_err(|_| "non-text stat field".to_string()))
        .collect()
}

/// Parse `/proc/<pid>/stat` bytes into `(state, parent pid, process group)`.
fn parse_proc_stat(raw: &[u8]) -> Result<(char, u32, i64), String> {
    let fields = proc_stat_fields_after_comm(raw)?;
    let mut fields = fields.into_iter();
    let state = fields
        .next()
        .and_then(|value| value.chars().next())
        .ok_or("missing stat state")?;
    let parent_pid = fields
        .next()
        .ok_or("missing stat parent pid")?
        .parse::<u32>()
        .map_err(|error| format!("invalid stat parent pid: {error}"))?;
    let process_group = fields
        .next()
        .ok_or("missing stat process group")?
        .parse::<i64>()
        .map_err(|error| format!("invalid stat process group: {error}"))?;
    Ok((state, parent_pid, process_group))
}

fn is_finished_state(state: char) -> bool {
    matches!(state, 'Z' | 'X')
}

/// Members of `pgid` other than `ignored`. `None` means the namespace or a
/// member record could not be read or parsed. A member that vanishes while
/// being read is gone and is not reported.
#[cfg(target_os = "linux")]
fn scan_group_members(pgid: i64, ignored: Option<u32>) -> Option<Vec<GroupMember>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut members = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if Some(pid) == ignored {
            continue;
        }
        let raw = match std::fs::read(format!("/proc/{pid}/stat")) {
            Ok(raw) => raw,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                continue;
            }
            Err(_) => return None,
        };
        let (state, parent_pid, process_group) = parse_proc_stat(&raw).ok()?;
        if process_group == pgid {
            members.push(GroupMember {
                pid,
                parent_pid,
                state,
            });
        }
    }
    Some(members)
}

/// Finished members this process may reap: parented by the checker, not the
/// group leader, and not a pid the caller still owns.
fn reapable_members(
    members: &[GroupMember],
    pgid: i64,
    checker_pid: u32,
    owned: &[u32],
) -> Vec<u32> {
    members
        .iter()
        .filter(|member| {
            member.state == 'Z'
                && member.parent_pid == checker_pid
                && i64::from(member.pid) != pgid
                && !owned.contains(&member.pid)
        })
        .map(|member| member.pid)
        .collect()
}

fn decide_group_presence(members: &[GroupMember], checker_pid: u32) -> GroupPresence {
    let present = members
        .iter()
        .any(|member| !is_finished_state(member.state) || member.parent_pid == checker_pid);
    if present {
        GroupPresence::Present
    } else {
        GroupPresence::Absent
    }
}

/// Decide whether process group `pgid` still has members.
///
/// 1. Scan `/proc` for members of the group.
/// 2. Reap, by exact pid with `waitpid(pid, WNOHANG)`, every zombie member
///    whose parent is this process, then rescan. Only such a zombie can be an
///    orphan this process adopted. Never `waitpid(-1)`. This process never
///    reaps the group leader (`pid == pgid`), which is the owner's wrapper
///    and the fence on the numeric group, nor any pid in `owned`, which the
///    caller still intends to wait on. A zombie leader or owned zombie parented
///    by this process therefore counts as present.
/// 3. Present when any member is in a state other than zombie or dead, or
///    is a zombie still parented by this process. Absent when only zombies
///    parented elsewhere remain, or no member does. Undecidable when `/proc`
///    or a member's record cannot be read or parsed.
///
/// `retained_leader` is a wrapper the caller keeps unreaped on purpose as the
/// group fence and reaps itself afterwards: it is excluded from the scan
/// entirely, neither reaped nor counted. Callers obtain `owned` from the child
/// handles they hold; a caller that holds none passes an empty slice.
///
/// Every spawn starts a new group; no spawn joins an existing process group
/// (setpgid 0 or setsid). The group leader is therefore the spawner's own
/// direct child, which is why it is never reaped here.
///
/// Invariant: once a group settles absent, its record is retired and nothing
/// signals that pgid again, so a later reuse of the number cannot be hit.
#[cfg(target_os = "linux")]
pub fn group_presence(pgid: i64, retained_leader: Option<u32>, owned: &[u32]) -> GroupPresence {
    if pgid <= 1 {
        return GroupPresence::Undecidable;
    }
    let checker_pid = std::process::id();
    let Some(mut members) = scan_group_members(pgid, retained_leader) else {
        return GroupPresence::Undecidable;
    };
    let reapable = reapable_members(&members, pgid, checker_pid, owned);
    if !reapable.is_empty() {
        for pid in reapable {
            let Ok(pid) = libc::pid_t::try_from(pid) else {
                continue;
            };
            let mut status = 0;
            // SAFETY: exact pid, non-blocking; a pid that is not our child
            // or is already reaped fails harmlessly with ECHILD.
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        }
        match scan_group_members(pgid, retained_leader) {
            Some(rescanned) => members = rescanned,
            None => return GroupPresence::Undecidable,
        }
    }
    decide_group_presence(&members, checker_pid)
}

/// Group presence is only decidable through the Linux pid namespace.
#[cfg(not(target_os = "linux"))]
pub fn group_presence(_pgid: i64, _retained_leader: Option<u32>, _owned: &[u32]) -> GroupPresence {
    GroupPresence::Undecidable
}
mod oci_lifecycle;
// Descendant containment by retained PID-namespace init: termination,
// recovery and settlement without a cgroup, and explicitly no quiescence.
#[cfg(target_os = "linux")]
mod pid_namespace;
#[cfg(target_os = "linux")]
pub use pid_namespace::{
    PidNamespaceIdentity, PinnedPidNamespace, cached_pid_namespace_containment_probe,
    pid_namespace_proof_source, probe_pid_namespace_containment,
};
mod scope;
pub use host_capability::{
    ACCOUNTING, CapabilityEvidence, CapabilityRequirement, CapabilityRequirementSpec, CgroupFacts,
    CgroupLimit, DirectoryStepFacts, EXEC_SEALED_MEMFD, FileClassFacts, HOST_FACTS_VERSION,
    HOST_PROVIDER, HostCapabilities, HostFacts, HostLifetimeBound, ISOLATION_NAMESPACES,
    ImmutableRootFacts, LocalityObservation, MAX_CAPABILITY_ENTRIES, MAX_CAPABILITY_NAME_BYTES,
    MAX_CAPABILITY_TEXT_BYTES, MAX_IMMUTABLE_ROOT_DEPTH, MAX_IMMUTABLE_ROOT_FILES, MemlockFacts,
    PERSISTENCE_DURABLE_STATE, PID_NAMESPACE_PROVIDER, PrivateDirectoryFacts, ProbeOutcome,
    ProcessCredentialFacts, ProofSource, QUIESCENCE_PID_NAMESPACE, RECOVERY_ENCLOSURE_TRUSTED_INIT,
    RECOVERY_PID_NAMESPACE, RESIDENCY_CAPABILITIES, RESIDENCY_LOCKED, RESIDENCY_SWAP_ABSENT,
    RESIDENCY_SWAP_LIMIT, ResidencyProof, RootStorageFacts, SCOPE_LIMITS, STORAGE_DURABLE_ROOT,
    STORAGE_IMMUTABLE_ROOT, TERMINATION_ENCLOSURE_TRUSTED_INIT, TERMINATION_PID_NAMESPACE,
    admit_enclosure_inits, admit_immutable_root_proof, ascending_refusal, current_host_generation,
    decide_durable_root, decide_durable_state, decide_immutable_root, decide_private_directory,
    floor_entry_refusal, host_kernel_is_gvisor, immutable_root_requirement, is_scope_capability,
    is_weaker_grade, kernel_version_is_gvisor, met_by_strongest, satisfies,
};
pub use oci_lifecycle::{
    OciHookState, OciLifecycleGeneration, OciLifecycleIntent, OciPreparationPhase,
    OciPreparationProgress,
};
pub use scope::{
    ControllerAccount, ProbeBarrier, ProcessHostLifetime, ProcessScope, ProcessScopeAllocation,
    ProcessScopeCapability, ProcessScopeConfiguration, ProcessScopeLaunchError,
    ProcessScopeProvider, ProcessScopeRecovery, ProcessScopeResourceLimits, QuiescedProcessScope,
    ResourceLimitedProcessScope, require_administrator,
};

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

/// What the `group_leader_*` coordinates of an identity mean.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBasis {
    /// A real, pinned process-group leader. The default; absent from stored
    /// identities, so every existing row and wire form is unchanged.
    #[default]
    ProcessGroup,
    /// Membership in a retained containment is the authority and no group was
    /// pinned: the coordinates repeat the process's own birth and name no
    /// group. Nothing may signal, freeze or reopen a group from them.
    Contained,
}

impl GroupBasis {
    pub fn is_process_group(&self) -> bool {
        matches!(self, Self::ProcessGroup)
    }
}

/// Durable coordinates required to recover one exact process incarnation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactProcessIdentity {
    pub boot_id: String,
    pub target_pid: u32,
    pub target_start_time_ticks: u64,
    pub group_leader_pid: u32,
    pub group_leader_start_time_ticks: u64,
    /// See [`GroupBasis`].
    #[serde(default, skip_serializing_if = "GroupBasis::is_process_group")]
    pub group_basis: GroupBasis,
    /// The pid namespace (`pid:[<inode>]`, the `/proc/self/ns/pid` link of the
    /// capturing process) in which the numeric pids and birth ticks above
    /// were observed. A pid number and its birth ticks mean something only
    /// inside one pid namespace; another container may use the same number
    /// for a different process. It is required: a stored identity without it
    /// fails to decode.
    pub pid_namespace: String,
}

/// Why [`ExactProcessIdentity::observe_end`] could not decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEndError {
    /// The recorded incarnation was observed in another pid namespace than
    /// the observer's. Its numeric pid and birth evidence are not comparable
    /// here, so they decide neither "ended" nor "alive". Only a caller that
    /// holds the node's exclusive state lock may resolve this, through
    /// [`ExactProcessIdentity::has_ended_holding_exclusive_lock`].
    ForeignPidNamespace { recorded: String, observer: String },
    /// Any other failure to classify; always fail closed.
    Unclassified(String),
}

impl std::fmt::Display for ProcessEndError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ForeignPidNamespace { recorded, observer } => write!(
                f,
                "recorded process incarnation was observed in pid namespace {recorded} but this observer is in {observer}; its pid and birth evidence are not comparable"
            ),
            Self::Unclassified(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for ProcessEndError {}

/// The calling process's pid namespace (`pid:[<inode>]`), the namespace in
/// which every numeric pid this process observes through procfs is meaningful.
pub fn current_pid_namespace() -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        linux::read_self_pid_namespace()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("pid namespace observation is unavailable on this OS".to_owned())
    }
}

fn valid_pid_namespace(value: &str) -> bool {
    value
        .strip_prefix("pid:[")
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|inode| !inode.is_empty() && inode.bytes().all(|b| b.is_ascii_digit()))
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
        identity.require_process_group_basis()?;
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
        fn require_pid_namespace(expected: &str) -> Result<(), &'static str> {
            // /proc must be this process's own pid namespace view.
            let mut own = [0u8; 16];
            let own_size = unsafe {
                libc::readlink(c"/proc/self".as_ptr(), own.as_mut_ptr().cast(), own.len())
            };
            let pid = unsafe { libc::getpid() } as u32;
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
            digits[..count].reverse();
            if own_size <= 0 || own[..own_size as usize] != digits[..count] {
                return Err("native admission cannot trust this process's procfs");
            }
            let mut link = [0u8; 64];
            let size = unsafe {
                libc::readlink(
                    c"/proc/self/ns/pid".as_ptr(),
                    link.as_mut_ptr().cast(),
                    link.len(),
                )
            };
            if size <= 0 || size as usize == link.len() {
                return Err("original pid namespace read refused");
            }
            if &link[..size as usize] != expected.as_bytes() {
                return Err("native admission cannot cross a process incarnation");
            }
            Ok(())
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
        require_pid_namespace(&expected.pid_namespace)?;
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

/// Identity of a process whose authority is membership in a retained
/// containment, not its process group. It requires no live group leader (an
/// orphaned-group member or a connector that called `setsid` has none to
/// pin), so the group coordinates are the process's own birth and are not
/// evidence of a group.
#[cfg(target_os = "linux")]
pub(crate) fn capture_contained_process_identity_from_pidfd(
    target_pid: u32,
    target_pidfd: std::os::fd::BorrowedFd<'_>,
) -> Result<ExactProcessIdentity, String> {
    linux::capture_contained_from_pidfd(target_pid, target_pidfd)
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
    /// Group signalling, freezing and group-rooted access need a real pinned
    /// leader. An identity captured by containment membership has none.
    pub(crate) fn require_process_group_basis(&self) -> Result<(), String> {
        if self.group_basis != GroupBasis::ProcessGroup {
            return Err(
                "identity was captured by containment membership and names no process group"
                    .to_owned(),
            );
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.boot_id.trim().is_empty()
            || self.target_pid <= 1
            || self.group_leader_pid <= 1
            || self.target_start_time_ticks == 0
            || self.group_leader_start_time_ticks == 0
            || !valid_pid_namespace(&self.pid_namespace)
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
            // Reviewed: the persisted version-1 digest is unchanged for every
            // existing identity, and a contained identity's coordinates are
            // its own birth, so the basis does not enter the digest.
            group_basis: _,
            // Reviewed: not in the digest. The persisted version-1 digest is
            // unchanged for every existing identity, and the digest binds
            // evidence to numeric coordinates, not to liveness; namespace
            // comparability is decided by `observe_end`, never by the digest.
            pid_namespace: _,
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
    /// terminal evidence. A recorded incarnation from another pid namespace
    /// is indeterminate too (see [`ProcessEndError::ForeignPidNamespace`]).
    pub fn has_ended(&self) -> Result<bool, String> {
        self.observe_end().map_err(|error| error.to_string())
    }

    /// [`Self::has_ended`] with the distinct typed indeterminate for a
    /// recorded incarnation observed in another pid namespace.
    pub fn observe_end(&self) -> Result<bool, ProcessEndError> {
        self.validate().map_err(ProcessEndError::Unclassified)?;
        #[cfg(target_os = "linux")]
        {
            let boot = linux::read_boot_id().ok();
            let observer = linux::read_self_pid_namespace().ok();
            if let Some(decided) = end_gate(self, boot.as_deref(), observer.as_deref()) {
                return decided;
            }
            match capture_exact_process_identity(self.target_pid, Some(self.group_leader_pid)) {
                Ok(current) => Ok(current != *self),
                Err(error) => self.classify_end_after_capture_error(&error),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ProcessEndError::Unclassified(
                "exact process death observation is unavailable on this OS".to_owned(),
            ))
        }
    }

    /// [`Self::has_ended`] for a caller that holds the node's exclusive state
    /// lock. A recorded incarnation from another pid namespace is then
    /// resolved as ended: the lock is exclusive for its volume, so whichever
    /// previous daemon held it (in this or another container sharing the
    /// volume) has released it by exiting, and that daemon is the incarnation
    /// the custody records name. Every other classification is unchanged and
    /// still fails closed, including an observer whose own `/proc` is not its
    /// pid namespace's (never ended, lock or not). Callers without the lock
    /// must use [`Self::has_ended`].
    ///
    /// The proof is only as strong as the lock: it is a real `flock` on a file
    /// of the data volume, and it excludes every other incarnation only on a
    /// volume where `flock` is real and shared by all of them (a local or
    /// block volume mounted by every container that can run the node). A
    /// shared network volume (NFS) or an object-store mount does not give
    /// that, so such a volume must not rely on this path; it needs a
    /// different proof. No typed volume kind is recorded here to refuse it.
    pub fn has_ended_holding_exclusive_lock(
        &self,
        state_lock: &crate::ExclusiveLockWitness<'_>,
    ) -> Result<bool, String> {
        let _ = state_lock;
        resolve_with_exclusive_lock(self.observe_end())
    }

    /// The boot and pid-namespace gate of [`Self::observe_end`] alone, for a
    /// caller that decides liveness by its own pidfd evidence but must not
    /// compare numeric coordinates across namespaces. `None` means the
    /// coordinate is comparable and the caller's own evidence decides;
    /// `Some(Ok(true))` is a boot that ended the incarnation; a foreign or
    /// unreadable namespace is `Some(Err(..))`, never "ended" and never "alive".
    pub fn end_precheck(&self) -> Option<Result<bool, ProcessEndError>> {
        #[cfg(target_os = "linux")]
        {
            let boot = linux::read_boot_id().ok();
            let observer = linux::read_self_pid_namespace().ok();
            end_gate(self, boot.as_deref(), observer.as_deref())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Some(Err(ProcessEndError::Unclassified(
                "exact process death observation is unavailable on this OS".to_owned(),
            )))
        }
    }

    /// [`Self::end_precheck`] for a caller holding the node's exclusive state
    /// lock: a foreign pid namespace resolves as ended exactly as in
    /// [`Self::has_ended_holding_exclusive_lock`]; everything else is
    /// unchanged and an untrusted observer namespace stays an error.
    pub fn end_precheck_holding_exclusive_lock(
        &self,
        state_lock: &crate::ExclusiveLockWitness<'_>,
    ) -> Option<Result<bool, String>> {
        let _ = state_lock;
        self.end_precheck().map(resolve_with_exclusive_lock)
    }

    /// Classify this recorded incarnation after the pidfd capture of its
    /// coordinate failed; see [`classify_after_capture_error`]. Reads only
    /// world-readable procfs state and sends no signal.
    #[cfg(target_os = "linux")]
    pub(crate) fn classify_end_after_capture_error(
        &self,
        error: &str,
    ) -> Result<bool, ProcessEndError> {
        let boot = linux::read_boot_id().ok();
        let observer = linux::read_self_pid_namespace().ok();
        let thread_group = linux::thread_group_of(self.target_pid);
        let stat = linux::read_process_stat(self.target_pid)
            .ok()
            .map(|stat| ObservedStat {
                start_time_ticks: stat.start_time_ticks,
                process_group: stat.process_group,
                state: stat.state,
            });
        // Only a zombie leader needs its thread list.
        let tasks = stat
            .filter(|stat| stat.state == 'Z')
            .map(|_| linux::task_ids_of(self.target_pid));
        classify_after_capture_error(
            self,
            boot.as_deref(),
            observer.as_deref(),
            thread_group,
            stat,
            tasks,
            error,
        )
    }
}

/// The lock-proof resolution: only the foreign-namespace indeterminate turns
/// into "ended"; everything else is exactly [`ExactProcessIdentity::has_ended`].
fn resolve_with_exclusive_lock(observed: Result<bool, ProcessEndError>) -> Result<bool, String> {
    match observed {
        Err(ProcessEndError::ForeignPidNamespace { .. }) => Ok(true),
        other => other.map_err(|error| error.to_string()),
    }
}

/// What `/proc/<pid>/stat` shows at a recorded coordinate, read without any
/// signal. `None` at the call site means unreadable or absent, which is never
/// evidence either way.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ObservedStat {
    pub(crate) start_time_ticks: u64,
    pub(crate) process_group: i64,
    pub(crate) state: char,
}

/// The first two rules of the end decision, which hold whether or not the
/// pidfd capture of the coordinate succeeds:
///
/// 1. Boot id differs: the recorded process cannot be alive; ended.
/// 2. The observer's pid namespace differs from the recorded one (or cannot
///    be read): pid numbers and birth ticks are not comparable, so they must
///    not decide either way. A different namespace is the typed
///    [`ProcessEndError::ForeignPidNamespace`]; an unreadable one is a plain
///    fail-closed refusal.
///
/// `None` means the coordinate is comparable and the remaining rules apply.
#[cfg(target_os = "linux")]
fn end_gate(
    recorded: &ExactProcessIdentity,
    current_boot: Option<&str>,
    observer_namespace: Option<&str>,
) -> Option<Result<bool, ProcessEndError>> {
    if current_boot.is_some_and(|boot| boot != recorded.boot_id) {
        return Some(Ok(true));
    }
    match observer_namespace {
        None => Some(Err(ProcessEndError::Unclassified(
            "cannot read the observer's pid namespace".to_owned(),
        ))),
        Some(observer) if observer != recorded.pid_namespace => {
            Some(Err(ProcessEndError::ForeignPidNamespace {
                recorded: recorded.pid_namespace.clone(),
                observer: observer.to_owned(),
            }))
        }
        Some(_) => None,
    }
}

/// Decide whether a recorded incarnation has ended when the pidfd capture of
/// its coordinate failed (typically EPERM on a live process of another uid,
/// e.g. a previous container's daemon). Pure so every branch is testable
/// without a foreign-uid process. In order:
///
/// 1. Boot id differs: ended.
/// 2. Observer pid namespace differs or is unreadable (including a mounted
///    procfs that is not this process's own pid namespace view, which cannot
///    be recorded or trusted): indeterminate, see [`end_gate`]; nothing below
///    is consulted. An unreadable or untrusted observer namespace is
///    `Unclassified` and never resolved by the lock proof.
/// 3. No task answers at the coordinate, or it is a non-leader thread (a
///    recorded incarnation is always a thread-group leader): ended.
/// 4. Same boot and namespace and the occupant's birth ticks differ from the
///    record, or (for a real process-group identity) its process group
///    differs: the number was reused; ended.
/// 5. Birth and group match and the occupant is `X` (dead): ended. If it is
///    `Z`, ended only when the leader is the sole entry of its task
///    directory; a zombie leader whose other threads still run is not ended.
///    An unreadable task listing stays fail-closed.
/// 6. Everything else stays fail-closed: a full match whose probe still
///    fails, or an unreadable stat (hidepid, races), is not evidence.
#[cfg(target_os = "linux")]
pub(crate) fn classify_after_capture_error(
    recorded: &ExactProcessIdentity,
    current_boot: Option<&str>,
    observer_namespace: Option<&str>,
    thread_group: Option<Result<u32, String>>,
    stat: Option<ObservedStat>,
    tasks: Option<Result<Vec<u32>, String>>,
    capture_error: &str,
) -> Result<bool, ProcessEndError> {
    if let Some(decided) = end_gate(recorded, current_boot, observer_namespace) {
        return decided;
    }
    match thread_group {
        None => return Ok(true),
        Some(Ok(group)) if group != recorded.target_pid => return Ok(true),
        _ => {}
    }
    if let Some(stat) = stat {
        // A contained identity names no group; its group fields repeat its
        // own birth, so only the birth ticks are comparable.
        let group_differs = recorded.group_basis == GroupBasis::ProcessGroup
            && stat.process_group != i64::from(recorded.group_leader_pid);
        if stat.start_time_ticks != recorded.target_start_time_ticks || group_differs {
            return Ok(true);
        }
        match stat.state {
            'X' => return Ok(true),
            'Z' => match tasks {
                Some(Ok(ids)) if ids.iter().all(|id| *id == recorded.target_pid) => {
                    return Ok(true);
                }
                Some(Ok(_)) => {
                    return Err(ProcessEndError::Unclassified(format!(
                        "cannot classify exact process incarnation: zombie leader still has running threads: {capture_error}"
                    )));
                }
                Some(Err(reason)) => {
                    return Err(ProcessEndError::Unclassified(format!(
                        "cannot classify exact process incarnation: task listing unreadable ({reason}): {capture_error}"
                    )));
                }
                None => {}
            },
            _ => {}
        }
    }
    Err(ProcessEndError::Unclassified(format!(
        "cannot classify exact process incarnation: {capture_error}"
    )))
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
        identity.require_process_group_basis()?;
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
            if let std::collections::btree_map::Entry::Vacant(slot) = retained.entry(pid) {
                let member = match pin_expected(pid, stat.start_time_ticks, stat.process_group) {
                    Ok(member) => member,
                    Err(error) if error.contains("vanished") || error.contains("changed") => {
                        return Ok((false, observed_live));
                    }
                    Err(error) => return Err(error),
                };
                slot.insert(member);
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

    /// The calling process's pid namespace link (`pid:[<inode>]`).
    ///
    /// Refused unless the mounted procfs belongs to this process's own pid
    /// namespace: `/proc/self` must name this process's pid. Otherwise
    /// `/proc/self/ns/pid` would name the wrong namespace and the observer
    /// cannot trust its own `/proc`.
    pub(super) fn read_self_pid_namespace() -> Result<String, String> {
        observer_namespace(
            std::fs::read_link("/proc/self")
                .map(|link| link.to_string_lossy().into_owned())
                .map_err(|error| format!("read /proc/self: {error}")),
            std::process::id(),
            std::fs::read_link("/proc/self/ns/pid")
                .map_err(|error| format!("read Linux pid namespace: {error}"))
                .and_then(|link| {
                    link.into_os_string()
                        .into_string()
                        .map_err(|_| "Linux pid namespace link is not UTF-8".to_owned())
                }),
        )
    }

    /// Pure decision of [`read_self_pid_namespace`], with both procfs reads
    /// injected: `proc_self` is `readlink /proc/self`, `namespace` is
    /// `readlink /proc/self/ns/pid`.
    pub(super) fn observer_namespace(
        proc_self: Result<String, String>,
        pid: u32,
        namespace: Result<String, String>,
    ) -> Result<String, String> {
        let proc_self = proc_self?;
        if proc_self != pid.to_string() {
            return Err(format!(
                "procfs is not this process's pid namespace view (/proc/self is `{proc_self}`, this process is {pid}); its pid namespace cannot be recorded or trusted"
            ));
        }
        let value = namespace?;
        if !crate::process_control::valid_pid_namespace(&value) {
            return Err("Linux pid namespace link has an unexpected form".to_owned());
        }
        Ok(value)
    }

    /// Thread ids listed under `/proc/<pid>/task`; any error is an error.
    pub(super) fn task_ids_of(pid: u32) -> Result<Vec<u32>, String> {
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/{pid}/task"))
            .map_err(|error| format!("read /proc/{pid}/task: {error}"))?
        {
            let entry = entry.map_err(|error| format!("read /proc/{pid}/task entry: {error}"))?;
            let name = entry.file_name();
            let id = name
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
                .ok_or_else(|| format!("/proc/{pid}/task has a non-numeric entry"))?;
            ids.push(id);
        }
        Ok(ids)
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
        let group_leader_pid = expected_group_leader.unwrap_or(target.process_group as u32);
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
        // The namespace in which every number above was read (this process's
        // own procfs view), fenced by the same re-probes as the stat reads.
        let pid_namespace = read_self_pid_namespace()?;
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
            group_basis: crate::GroupBasis::ProcessGroup,
            pid_namespace,
        })
    }

    pub(super) fn capture_contained_from_pidfd(
        target_pid: u32,
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
        let pid_namespace = read_self_pid_namespace()?;
        pidfd_signal(target_pidfd.as_raw_fd(), 0, 0)
            .map_err(|error| format!("reprobe exact target pidfd: {error}"))?;
        Ok(ExactProcessIdentity {
            boot_id: read_boot_id()?,
            target_pid,
            target_start_time_ticks: target.start_time_ticks,
            group_leader_pid: target_pid,
            group_leader_start_time_ticks: target.start_time_ticks,
            group_basis: super::GroupBasis::Contained,
            pid_namespace,
        })
    }

    pub(super) fn read_process_stat(pid: u32) -> std::io::Result<ProcessStat> {
        // A task that exits while its stat file is being read fails the read
        // with ESRCH, which std does not classify as NotFound. Report it as
        // the same absence an already-gone entry gives, so callers that skip
        // vanished members handle both.
        let raw = match std::fs::read(format!("/proc/{pid}/stat")) {
            Ok(raw) => raw,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, error));
            }
            Err(error) => return Err(error),
        };
        let fields = crate::process_control::proc_stat_fields_after_comm(&raw)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
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

    /// The thread group a `/proc/<n>` coordinate belongs to: `None` when no
    /// task answers there, else its `Tgid` (equal to `n` only for a process).
    pub(super) fn thread_group_of(pid: u32) -> Option<Result<u32, String>> {
        let status = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Ok(status) => status,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                return None;
            }
            Err(error) => return Some(Err(format!("read /proc/{pid}/status: {error}"))),
        };
        Some(
            status
                .lines()
                .find_map(|line| line.strip_prefix("Tgid:"))
                .ok_or_else(|| format!("/proc/{pid}/status has no Tgid"))
                .and_then(|value| {
                    value
                        .trim()
                        .parse::<u32>()
                        .map_err(|error| format!("parse /proc/{pid}/status Tgid: {error}"))
                }),
        )
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

    /// The PID a pidfd names in this process's PID namespace, read from its
    /// fdinfo. `-1` means the task has exited. This is the descriptor's own
    /// testimony, unlike a numeric PID read from /proc.
    pub(super) fn pidfd_pid(pidfd: i32) -> Result<i64, String> {
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{pidfd}"))
            .map_err(|error| format!("read pidfd fdinfo: {error}"))?;
        info.lines()
            .find_map(|line| line.strip_prefix("Pid:"))
            .ok_or_else(|| "pidfd fdinfo has no Pid".to_owned())?
            .trim()
            .parse::<i64>()
            .map_err(|error| format!("parse pidfd fdinfo Pid: {error}"))
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

#[cfg(all(test, target_os = "linux"))]
mod contained_identity_tests {
    use std::io::Read as _;
    use std::os::fd::AsFd as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    #[test]
    fn contained_identity_needs_no_live_group_leader() {
        // The group leader exits at once, leaving `sleep` an orphaned-group
        // member: nothing remains to pin as its group.
        let mut leader = Command::new("sh")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut text = String::new();
        leader
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        leader.wait().unwrap();
        let pid: u32 = text.trim().parse().unwrap();
        let pidfd = super::linux::open_pidfd(pid).unwrap();
        let group_required =
            super::capture_exact_process_identity_from_pidfd(pid, None, pidfd.as_fd());
        let contained = super::capture_contained_process_identity_from_pidfd(pid, pidfd.as_fd());
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        assert!(
            group_required.is_err(),
            "the group requirement cannot be met"
        );
        let identity = contained.expect("membership, not the group, is the authority");
        assert_eq!(identity.target_pid, pid);
        assert_eq!(identity.group_leader_pid, pid);
        identity.incarnation_digest().unwrap();
        // The marker is type-level: it is stored only when contained (so
        // every existing identity serializes unchanged) and no group
        // operation accepts it.
        assert_eq!(identity.group_basis, super::GroupBasis::Contained);
        assert_eq!(
            serde_json::to_value(&identity).unwrap()["group_basis"],
            "contained"
        );
        let mut ordinary = identity.clone();
        ordinary.group_basis = super::GroupBasis::ProcessGroup;
        assert!(
            serde_json::to_value(&ordinary)
                .unwrap()
                .get("group_basis")
                .is_none()
        );
        let error = super::quiesce_exact_process_group(&identity, std::time::Duration::ZERO)
            .expect_err("a contained identity names no group to freeze");
        assert!(error.contains("names no process group"), "{error}");
        let error = super::ExactProcessRoot::open(&identity).err().unwrap();
        assert!(error.contains("names no process group"), "{error}");
    }
}

#[cfg(test)]
mod tests {
    /// A reaped child's pid answers neither `/proc/<pid>/stat` nor
    /// `/proc/<pid>/status`; both reads report absence, never a procfs error.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_pid_is_absent_from_procfs_reads() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let error = super::linux::read_process_stat(pid).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(super::linux::thread_group_of(pid).is_none());
    }

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
                    group_basis: crate::GroupBasis::ProcessGroup,
                    pid_namespace: "pid:[4026532001]".into(),
                };
                // The namespace is not part of the persisted digest.
                let other_namespace = super::ExactProcessIdentity {
                    pid_namespace: "pid:[4026532999]".into(),
                    ..identity.clone()
                };
                assert_eq!(
                    other_namespace.incarnation_digest_bytes(),
                    identity.incarnation_digest_bytes()
                );
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
            group_basis: crate::GroupBasis::ProcessGroup,
            pid_namespace: "pid:[4026532001]".into(),
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
            group_basis: crate::GroupBasis::ProcessGroup,
            pid_namespace: String::new(),
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
    mod capture_error_classification {
        use super::*;

        const EPERM: &str = "open exact process 7 pidfd: Operation not permitted (os error 1)";
        const NS: &str = "pid:[4026532001]";
        const OTHER_NS: &str = "pid:[4026532999]";

        fn recorded() -> ExactProcessIdentity {
            ExactProcessIdentity {
                boot_id: "boot-a".into(),
                target_pid: 7,
                target_start_time_ticks: 100,
                group_leader_pid: 7,
                group_leader_start_time_ticks: 100,
                group_basis: crate::GroupBasis::ProcessGroup,
                pid_namespace: NS.into(),
            }
        }

        fn stat(ticks: u64, group: i64, state: char) -> Option<ObservedStat> {
            Some(ObservedStat {
                start_time_ticks: ticks,
                process_group: group,
                state,
            })
        }

        fn classify_in(
            boot: Option<&str>,
            observer: Option<&str>,
            thread_group: Option<Result<u32, String>>,
            stat: Option<ObservedStat>,
            tasks: Option<Result<Vec<u32>, String>>,
        ) -> Result<bool, ProcessEndError> {
            classify_after_capture_error(
                &recorded(),
                boot,
                observer,
                thread_group,
                stat,
                tasks,
                EPERM,
            )
        }

        /// Same boot, same namespace.
        fn classify(
            thread_group: Option<Result<u32, String>>,
            stat: Option<ObservedStat>,
        ) -> Result<bool, ProcessEndError> {
            classify_in(Some("boot-a"), Some(NS), thread_group, stat, None)
        }

        fn unclassified(result: Result<bool, ProcessEndError>) -> String {
            match result {
                Err(ProcessEndError::Unclassified(reason)) => reason,
                other => panic!("expected Unclassified, got {other:?}"),
            }
        }

        #[test]
        fn boot_id_difference_is_ended_even_when_stat_matches() {
            assert_eq!(
                classify_in(
                    Some("boot-b"),
                    Some(NS),
                    Some(Ok(7)),
                    stat(100, 7, 'S'),
                    None
                ),
                Ok(true)
            );
        }

        #[test]
        fn boot_id_difference_decides_before_the_namespace_rule() {
            assert_eq!(
                classify_in(
                    Some("boot-b"),
                    Some(OTHER_NS),
                    Some(Ok(7)),
                    stat(100, 7, 'S'),
                    None
                ),
                Ok(true)
            );
        }

        #[test]
        fn another_namespace_with_matching_ticks_is_foreign_not_decided_by_ticks() {
            assert_eq!(
                classify_in(
                    Some("boot-a"),
                    Some(OTHER_NS),
                    Some(Ok(7)),
                    stat(100, 7, 'S'),
                    None
                ),
                Err(ProcessEndError::ForeignPidNamespace {
                    recorded: NS.into(),
                    observer: OTHER_NS.into(),
                })
            );
        }

        #[test]
        fn another_namespace_is_foreign_whatever_the_occupant_shows() {
            let foreign = Err(ProcessEndError::ForeignPidNamespace {
                recorded: NS.into(),
                observer: OTHER_NS.into(),
            });
            // Absent, non-leader, reused birth, reused group and zombie
            // occupants are all numbers of another namespace: none decides.
            for (group, stat_view) in [
                (None, None),
                (Some(Ok(3)), None),
                (Some(Ok(7)), stat(101, 7, 'S')),
                (Some(Ok(7)), stat(100, 9, 'S')),
                (Some(Ok(7)), stat(100, 7, 'Z')),
                (Some(Ok(7)), stat(100, 7, 'X')),
            ] {
                assert_eq!(
                    classify_in(Some("boot-a"), Some(OTHER_NS), group, stat_view, None),
                    foreign
                );
            }
        }

        #[test]
        fn foreign_namespace_resolves_to_ended_only_with_the_lock_proof() {
            let foreign = ProcessEndError::ForeignPidNamespace {
                recorded: NS.into(),
                observer: OTHER_NS.into(),
            };
            assert_eq!(resolve_with_exclusive_lock(Err(foreign.clone())), Ok(true));
            // Without proof the same observation is an error, never "ended".
            let error = foreign.to_string();
            assert!(error.contains(NS) && error.contains(OTHER_NS), "{error}");
            // The proof changes nothing else.
            assert_eq!(resolve_with_exclusive_lock(Ok(false)), Ok(false));
            assert_eq!(resolve_with_exclusive_lock(Ok(true)), Ok(true));
            let refused = resolve_with_exclusive_lock(Err(ProcessEndError::Unclassified(
                "cannot classify exact process incarnation".into(),
            )));
            assert!(refused.unwrap_err().contains("cannot classify"));
        }

        #[test]
        fn procfs_of_another_namespace_is_unrecordable_and_never_ended() {
            let ns = || Ok(NS.to_owned());
            assert_eq!(
                linux::observer_namespace(Ok("42".into()), 42, ns()),
                Ok(NS.to_owned())
            );
            // /proc mounted from another pid namespace: /proc/self names
            // another number, so /proc/self/ns/pid names the wrong namespace.
            for proc_self in [
                Ok("7".to_owned()),
                Ok("self".to_owned()),
                Err("denied".into()),
            ] {
                let refused = linux::observer_namespace(proc_self, 42, ns());
                assert!(refused.is_err(), "{refused:?}");
            }
            assert!(linux::observer_namespace(Ok("42".into()), 42, Err("x".into())).is_err());
            // At compare the refused read is `None`: Unclassified, and the
            // lock proof does not resolve it.
            let result = classify_in(Some("boot-a"), None, Some(Ok(7)), stat(100, 7, 'S'), None);
            let reason = unclassified(result.clone());
            assert!(reason.contains("pid namespace"), "{reason}");
            assert!(resolve_with_exclusive_lock(result).is_err());
        }

        #[test]
        fn this_process_procfs_is_trusted() {
            assert!(current_pid_namespace().is_ok());
        }

        #[test]
        fn unreadable_observer_namespace_fails_closed_not_foreign() {
            let reason = unclassified(classify_in(
                Some("boot-a"),
                None,
                Some(Ok(7)),
                stat(5, 7, 'S'),
                None,
            ));
            assert!(reason.contains("pid namespace"), "{reason}");
        }

        #[test]
        fn same_boot_different_birth_is_ended() {
            // The EPERM collision: a later container's leader of another uid.
            assert_eq!(classify(Some(Ok(7)), stat(101, 7, 'S')), Ok(true));
        }

        #[test]
        fn same_boot_different_group_is_ended() {
            assert_eq!(classify(Some(Ok(7)), stat(100, 9, 'S')), Ok(true));
        }

        #[test]
        fn contained_identity_ignores_the_group_field() {
            let mut contained = recorded();
            contained.group_basis = crate::GroupBasis::Contained;
            let result = classify_after_capture_error(
                &contained,
                Some("boot-a"),
                Some(NS),
                Some(Ok(7)),
                stat(100, 9, 'S'),
                None,
                EPERM,
            );
            assert!(result.is_err());
        }

        #[test]
        fn full_match_with_failed_probe_stays_an_error() {
            let error = unclassified(classify(Some(Ok(7)), stat(100, 7, 'S')));
            assert!(error.contains("cannot classify exact process incarnation"));
            assert!(error.contains(EPERM));
        }

        #[test]
        fn matching_dead_incarnation_is_ended() {
            assert_eq!(classify(Some(Ok(7)), stat(100, 7, 'X')), Ok(true));
        }

        #[test]
        fn zombie_leader_with_no_other_thread_is_ended() {
            for tasks in [vec![], vec![7]] {
                assert_eq!(
                    classify_in(
                        Some("boot-a"),
                        Some(NS),
                        Some(Ok(7)),
                        stat(100, 7, 'Z'),
                        Some(Ok(tasks))
                    ),
                    Ok(true)
                );
            }
        }

        #[test]
        fn zombie_leader_with_running_threads_is_not_ended() {
            let reason = unclassified(classify_in(
                Some("boot-a"),
                Some(NS),
                Some(Ok(7)),
                stat(100, 7, 'Z'),
                Some(Ok(vec![7, 9])),
            ));
            assert!(reason.contains("running threads"), "{reason}");
        }

        #[test]
        fn zombie_leader_with_unreadable_task_listing_fails_closed() {
            let reason = unclassified(classify_in(
                Some("boot-a"),
                Some(NS),
                Some(Ok(7)),
                stat(100, 7, 'Z'),
                Some(Err("Permission denied".into())),
            ));
            assert!(reason.contains("task listing unreadable"), "{reason}");
            // A missing listing is the same refusal.
            unclassified(classify_in(
                Some("boot-a"),
                Some(NS),
                Some(Ok(7)),
                stat(100, 7, 'Z'),
                None,
            ));
        }

        #[test]
        fn unreadable_stat_is_not_evidence() {
            unclassified(classify(Some(Ok(7)), None));
            unclassified(classify_in(None, Some(NS), Some(Ok(7)), None, None));
            unclassified(classify(Some(Err("denied".into())), None));
        }

        #[test]
        fn absent_coordinate_and_non_leader_thread_are_ended() {
            assert_eq!(classify(None, None), Ok(true));
            assert_eq!(classify(Some(Ok(3)), None), Ok(true));
        }

        #[test]
        fn unreadable_boot_does_not_block_a_birth_mismatch() {
            assert_eq!(
                classify_in(None, Some(NS), Some(Ok(7)), stat(5, 7, 'S'), None),
                Ok(true)
            );
        }

        #[test]
        fn ok_path_is_unchanged() {
            let own = capture_exact_process_identity(std::process::id(), None).unwrap();
            assert!(!own.has_ended().unwrap());
            let mut reborn = own.clone();
            reborn.target_start_time_ticks += 1;
            assert!(reborn.has_ended().unwrap());
        }

        #[test]
        fn capture_records_the_capturers_pid_namespace() {
            let own = capture_exact_process_identity(std::process::id(), None).unwrap();
            assert_eq!(own.pid_namespace, current_pid_namespace().unwrap());
            assert_eq!(
                capture_current_process_identity().unwrap().pid_namespace,
                own.pid_namespace
            );
        }

        #[test]
        fn a_live_identity_from_another_namespace_is_foreign_and_lock_resolved() {
            let mut own = capture_exact_process_identity(std::process::id(), None).unwrap();
            own.pid_namespace = OTHER_NS.into();
            // Matching boot, pid and birth, but not comparable: neither alive
            // nor ended without the proof.
            match own.observe_end() {
                Err(ProcessEndError::ForeignPidNamespace { recorded, observer }) => {
                    assert_eq!(recorded, OTHER_NS);
                    assert_eq!(observer, current_pid_namespace().unwrap());
                }
                other => panic!("expected ForeignPidNamespace, got {other:?}"),
            }
            assert!(own.has_ended().is_err());
            let directory = std::env::temp_dir().join(format!(
                "lillux-pidns-lock-{}-{}",
                std::process::id(),
                own.target_start_time_ticks
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let lock =
                crate::ExactExclusiveFileLock::acquire(&directory.join("operator.lock")).unwrap();
            let resolved = own.has_ended_holding_exclusive_lock(&lock.witness().unwrap());
            drop(lock);
            let _ = std::fs::remove_dir_all(&directory);
            assert_eq!(resolved, Ok(true));
        }

        #[test]
        fn identity_round_trips_with_the_namespace_and_requires_it() {
            let identity = recorded();
            let json = serde_json::to_value(&identity).unwrap();
            assert_eq!(json["pid_namespace"], NS);
            assert_eq!(
                serde_json::from_value::<ExactProcessIdentity>(json.clone()).unwrap(),
                identity
            );
            let mut legacy = json;
            legacy.as_object_mut().unwrap().remove("pid_namespace");
            let error = serde_json::from_value::<ExactProcessIdentity>(legacy).unwrap_err();
            assert!(
                error.to_string().contains("missing field `pid_namespace`"),
                "{error}"
            );
        }

        #[test]
        fn identity_with_a_malformed_namespace_is_incomplete() {
            for bad in ["", "pid:[]", "pid:[12x]", "net:[4026532001]", "4026532001"] {
                let mut identity = recorded();
                identity.pid_namespace = bad.into();
                assert!(identity.validate().is_err(), "{bad}");
            }
        }

        #[test]
        fn task_listing_of_a_live_process_names_its_threads() {
            let ids = linux::task_ids_of(std::process::id()).unwrap();
            assert!(ids.contains(&std::process::id()));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_coordinate_held_by_another_process_thread_is_an_ended_incarnation() {
        let own = capture_exact_process_identity(std::process::id(), None).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            sender.send(unsafe { libc::gettid() } as u32).unwrap();
            held.recv().unwrap();
        });
        let tid = receiver.recv().unwrap();
        assert!(std::path::Path::new(&format!("/proc/{tid}")).exists());
        // The incarnation recorded at that number is gone; the number now
        // names a thread of this process, which is no process at all.
        let recorded = ExactProcessIdentity {
            target_pid: tid,
            ..own.clone()
        };
        assert!(recorded.has_ended().unwrap());
        assert!(!own.has_ended().unwrap());
        release.send(()).unwrap();
        thread.join().unwrap();
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

#[cfg(test)]
mod group_spawn_guard;

#[cfg(all(test, target_os = "linux"))]
mod group_presence_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn member(pid: u32, parent_pid: u32, state: char) -> GroupMember {
        GroupMember {
            pid,
            parent_pid,
            state,
        }
    }

    /// Fork a child that moves into its own group and runs `body`.
    fn fork_group_member(body: fn()) -> libc::pid_t {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe { libc::setpgid(0, 0) };
            body();
            unsafe { libc::_exit(0) };
        }
        // Both sides make the group: a signal sent right after the fork (a
        // SIGSTOP, say) can stop the child before it runs its own `setpgid`,
        // and a child stopped outside the group is never found by a scan of
        // that group. Under load that race is wide.
        unsafe { libc::setpgid(pid, pid) };
        pid
    }

    fn pause_forever() {
        loop {
            unsafe { libc::pause() };
        }
    }

    fn wait_for_state(pgid: libc::pid_t, pid: libc::pid_t, state: char) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let members = scan_group_members(i64::from(pgid), None).expect("scan");
            if members
                .iter()
                .any(|m| m.pid == pid as u32 && m.state == state)
            {
                return;
            }
            assert!(Instant::now() < deadline, "pid {pid} never reached {state}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn reap(pid: libc::pid_t) -> libc::pid_t {
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) }
    }

    #[test]
    fn zombie_parented_elsewhere_is_absent() {
        // The intermediate child makes a grandchild that leads its own group
        // and exits at once. The intermediate never reaps it, so the zombie
        // stays parented by the intermediate, not by this process.
        let (reader, writer) = {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            (fds[0], fds[1])
        };
        let helper = unsafe { libc::fork() };
        assert!(helper >= 0);
        if helper == 0 {
            let grandchild = unsafe { libc::fork() };
            if grandchild == 0 {
                unsafe {
                    libc::setpgid(0, 0);
                    libc::_exit(0);
                }
            }
            let bytes = grandchild.to_ne_bytes();
            unsafe { libc::write(writer, bytes.as_ptr().cast(), bytes.len()) };
            pause_forever();
        }
        unsafe { libc::close(writer) };
        let mut bytes = [0_u8; 4];
        assert_eq!(
            unsafe { libc::read(reader, bytes.as_mut_ptr().cast(), 4) },
            4
        );
        unsafe { libc::close(reader) };
        let grandchild = libc::pid_t::from_ne_bytes(bytes);
        wait_for_state(grandchild, grandchild, 'Z');
        assert_eq!(
            group_presence(i64::from(grandchild), None, &[]),
            GroupPresence::Absent
        );
        unsafe { libc::kill(helper, libc::SIGKILL) };
        reap(helper);
    }

    #[test]
    fn leader_zombie_child_is_present_and_stays_waitable() {
        let child = fork_group_member(|| {});
        wait_for_state(child, child, 'Z');
        assert_eq!(
            group_presence(i64::from(child), None, &[]),
            GroupPresence::Present
        );
        assert_eq!(reap(child), child, "owner's wait must still succeed");
    }

    /// Run in a forked subreaper `S`. A group leader `L` forks a member `M`
    /// that inherits `L`'s group; once `L` dies and is reaped, `M` is adopted
    /// by `S` and exits, leaving a non-leader zombie parented by `S`. Returns
    /// 0 when the presence check behaves as expected.
    fn subreaper_scenario(owned: bool) -> i32 {
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) };
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return 1;
        }
        let (read_end, write_end) = (fds[0], fds[1]);
        let leader = unsafe { libc::fork() };
        if leader < 0 {
            return 1;
        }
        if leader == 0 {
            unsafe { libc::setpgid(0, 0) };
            let member_pid = unsafe { libc::fork() };
            if member_pid == 0 {
                unsafe { libc::close(write_end) };
                let mut byte = 0_u8;
                while unsafe { libc::read(read_end, (&mut byte as *mut u8).cast(), 1) } > 0 {}
                unsafe { libc::_exit(0) };
            }
            unsafe { libc::_exit(0) };
        }
        let mut status = 0;
        unsafe { libc::waitpid(leader, &mut status, 0) };
        unsafe { libc::close(write_end) };
        let me = std::process::id();
        let deadline = Instant::now() + Duration::from_secs(5);
        let member_pid = loop {
            let members = scan_group_members(i64::from(leader), None).unwrap_or_default();
            if let [only] = members.as_slice()
                && only.state == 'Z'
                && only.parent_pid == me
            {
                break only.pid;
            }
            if Instant::now() >= deadline {
                return 2;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let skip: Vec<u32> = if owned { vec![member_pid] } else { Vec::new() };
        let presence = group_presence(i64::from(leader), None, &skip);
        let reaped = reap(member_pid as libc::pid_t);
        match (owned, presence, reaped) {
            (true, GroupPresence::Present, pid) if pid == member_pid as libc::pid_t => 0,
            (true, _, _) => 3,
            (false, GroupPresence::Absent, -1) => 0,
            (false, _, _) => 4,
        }
    }

    fn run_subreaper_scenario(owned: bool) {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let code = subreaper_scenario(owned);
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0, "scenario failure code");
    }

    #[test]
    fn owned_zombie_is_not_reaped_and_counts_present() {
        run_subreaper_scenario(true);
    }

    #[test]
    fn adopted_non_leader_zombie_is_reaped_then_absent() {
        run_subreaper_scenario(false);
    }

    fn reap_blocking(pid: libc::pid_t) {
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }

    #[test]
    fn stopped_member_is_present() {
        let child = fork_group_member(pause_forever);
        assert_eq!(unsafe { libc::kill(child, libc::SIGSTOP) }, 0);
        wait_for_state(child, child, 'T');
        assert_eq!(
            group_presence(i64::from(child), None, &[]),
            GroupPresence::Present
        );
        unsafe { libc::kill(child, libc::SIGKILL) };
        reap_blocking(child);
    }

    #[test]
    fn retained_leader_is_neither_counted_nor_reaped() {
        let child = fork_group_member(|| {});
        wait_for_state(child, child, 'Z');
        assert_eq!(
            group_presence(i64::from(child), Some(child as u32), &[]),
            GroupPresence::Absent
        );
        assert_eq!(reap(child), child);
    }

    #[test]
    fn garbled_stat_is_unparseable() {
        assert!(parse_proc_stat(b"garbage").is_err());
        assert!(parse_proc_stat(b"1 (x) Z").is_err());
        assert!(parse_proc_stat(b"1 (x) Z notanumber 5").is_err());
        assert_eq!(parse_proc_stat(b"1 (a b) Z 7 9 9"), Ok(('Z', 7, 9)));
        // Parentheses and non-UTF-8 bytes inside the command name.
        assert_eq!(parse_proc_stat(b"1 (a) (b)) S 7 9 9"), Ok(('S', 7, 9)));
        assert_eq!(parse_proc_stat(b"1 (\xff\xfe) R 3 4 4"), Ok(('R', 3, 4)));
    }

    #[test]
    fn non_utf8_command_name_does_not_make_the_scan_undecidable() {
        let child = fork_group_member(|| {
            unsafe { libc::prctl(libc::PR_SET_NAME, c"\xff\xfe".as_ptr()) };
            pause_forever();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let raw = std::fs::read(format!("/proc/{child}/stat")).expect("read stat");
            if raw.windows(2).any(|pair| pair == b"\xff\xfe") {
                break;
            }
            assert!(Instant::now() < deadline, "comm was never set");
            std::thread::sleep(Duration::from_millis(5));
        }
        wait_for_state(child, child, 'S');
        assert_eq!(
            group_presence(i64::from(child), None, &[]),
            GroupPresence::Present
        );
        unsafe { libc::kill(child, libc::SIGKILL) };
        reap_blocking(child);
    }

    #[test]
    fn unusable_group_numbers_are_undecidable() {
        for pgid in [i64::MIN, -5, 0, 1] {
            assert_eq!(group_presence(pgid, None, &[]), GroupPresence::Undecidable);
        }
    }

    #[test]
    fn decision_follows_state_and_parent() {
        let me = 100;
        assert_eq!(decide_group_presence(&[], me), GroupPresence::Absent);
        for state in ['R', 'S', 'D', 'T', 't', 'I'] {
            assert_eq!(
                decide_group_presence(&[member(5, 1, state)], me),
                GroupPresence::Present
            );
        }
        assert_eq!(
            decide_group_presence(&[member(5, 1, 'Z'), member(6, 1, 'X')], me),
            GroupPresence::Absent
        );
        assert_eq!(
            decide_group_presence(&[member(5, me, 'Z')], me),
            GroupPresence::Present
        );
    }

    #[test]
    fn reapable_excludes_leader_owned_and_foreign_parents() {
        let me = 100;
        let members = [
            member(10, me, 'Z'),
            member(11, me, 'Z'),
            member(12, me, 'Z'),
            member(13, 1, 'Z'),
            member(14, me, 'S'),
        ];
        assert_eq!(reapable_members(&members, 10, me, &[11]), vec![12]);
    }
}

// proof only, never merged: selects the process-scope suites at a gate
