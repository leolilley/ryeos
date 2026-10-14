//! Platform-neutral ownership of one execution's process scope.
//!
//! Applications retain recovery evidence and enforce their existing admission,
//! attachment and workspace fences. Only Lillux interprets platform configuration
//! or kernel coordinates. Adding an OS must not add cgroup/job-object branches
//! to an application, generic subprocess request, or durable workspace service.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const SCOPE_CONFIGURATION_VERSION: u32 = 3;
const SCOPE_RECOVERY_VERSION: u32 = 5;
const SCOPE_ALLOCATION_VERSION: u32 = 3;
const HOST_LIFETIME_VERSION: u32 = 2;
/// Root owns native supervisor control directories. The selected controller's
/// primary group gets traversal only, so it can reach its exact `0600` FIFO
/// without listing or changing the supervisor namespace.
const DELEGATED_CONTROL_DIRECTORY_MODE: libc::mode_t = 0o710;
/// Root-owned host testimony may be read by its selected controller, but only
/// the administrator may alter the namespace or its records.
const DELEGATED_READONLY_DIRECTORY_MODE: libc::mode_t = 0o750;

/// Administrator-selected host account for a controller, not a RyeOS signing
/// identity. Native account coordinates and credential-drop interpretation
/// stay in Lillux; applications retain this value without matching on its OS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ControllerAccount(AccountBackend);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "implementation", rename_all = "snake_case", deny_unknown_fields)]
enum AccountBackend {
    Unix { uid: u32, gid: u32 },
}

impl ControllerAccount {
    /// Capture the current unprivileged account for an administrator-requested
    /// host association. This observes native identity only; it does not grant
    /// a worker any account-selection capability.
    pub fn current() -> Result<Self, String> {
        #[cfg(unix)]
        {
            let account = Self::unix(unsafe { libc::geteuid() }, unsafe { libc::getegid() });
            account.validate()?;
            Ok(account)
        }
        #[cfg(not(unix))]
        Err("current controller account is unavailable on this OS".to_owned())
    }

    /// Apply the selected identity only in the child, before user code. Reuse
    /// this for maintenance observations as well as scope-controller launch;
    /// the privileged parent must never temporarily change its own credentials.
    pub(crate) fn configure_command(
        &self,
        command: &mut std::process::Command,
    ) -> Result<(), String> {
        self.validate()?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt as _;
            let AccountBackend::Unix { uid, gid } = self.0;
            unsafe {
                command.pre_exec(move || {
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setresgid(gid, gid, gid) != 0
                        || libc::setresuid(uid, uid, uid) != 0
                        || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = command;
            Err("controller credential transition is unavailable on this OS".to_owned())
        }
    }

    /// Administrator provisioning of one exact private intent directory.
    /// The caller must hold its protected installation namespace. This does
    /// not grant ownership of a service definition or follow an ambient path.
    pub fn grant_private_directory(
        &self,
        directory: &crate::PinnedDirectory,
    ) -> anyhow::Result<()> {
        self.validate().map_err(anyhow::Error::msg)?;
        #[cfg(unix)]
        {
            let file = directory.try_clone_descriptor()?;
            self.grant_private_descriptor(&file, true)?;
            self.require_directory_owner(directory)
        }
        #[cfg(not(unix))]
        anyhow::bail!("host directory grants are unavailable on this OS")
    }

    /// Administrator provisioning of one already-open, single-link intent file.
    pub fn grant_private_file(&self, file: &crate::PinnedRegularFile) -> anyhow::Result<()> {
        self.validate().map_err(anyhow::Error::msg)?;
        #[cfg(unix)]
        {
            let descriptor = file.try_clone_descriptor()?;
            self.grant_private_descriptor(&descriptor, false)?;
            let AccountBackend::Unix { uid, .. } = self.0;
            file.require_owner(uid)
        }
        #[cfg(not(unix))]
        anyhow::bail!("host file grants are unavailable on this OS")
    }

    /// Transfer one exact, single-link read-only delivery file to the admitted
    /// account without changing its content-bearing mode. Unlike mutable intent
    /// grants this accepts only private read/read-execute modes, not writable
    /// files or trees. The caller owns admission and parent-directory custody.
    pub fn grant_private_readonly_file(
        &self,
        file: &crate::PinnedRegularFile,
        expected_mode: u32,
    ) -> anyhow::Result<()> {
        self.validate().map_err(anyhow::Error::msg)?;
        if !matches!(expected_mode, 0o400 | 0o500) {
            anyhow::bail!("private read-only grant requires exact delivery mode");
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::fs::MetadataExt as _;
            let descriptor = file.try_clone_descriptor()?;
            let before = descriptor.metadata()?;
            let AccountBackend::Unix { uid, gid } = self.0;
            if unsafe { libc::geteuid() } != 0
                || (before.uid() != 0 && before.uid() != uid)
                || !before.is_file()
                || before.nlink() != 1
                || before.mode() & 0o7777 != expected_mode
            {
                anyhow::bail!(
                    "read-only delivery grant requires administrator authority and exact safe file"
                );
            }
            if unsafe { libc::fchown(descriptor.as_raw_fd(), uid, gid) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let after = descriptor.metadata()?;
            if after.uid() != uid
                || after.gid() != gid
                || after.mode() & 0o7777 != expected_mode
                || after.dev() != before.dev()
                || after.ino() != before.ino()
                || after.nlink() != 1
                || after.len() != before.len()
            {
                anyhow::bail!("read-only delivery grant changed retained file identity or mode");
            }
            descriptor.sync_all()?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            anyhow::bail!("read-only delivery grants are unavailable on this OS")
        }
    }

    /// Give the selected account read/traversal access to an exact
    /// administrator-owned host-state directory without granting mutation.
    ///
    /// This is for public association and recovery testimony that the account
    /// must corroborate during an ordinary lifecycle operation. It is not a
    /// place for credentials or private operator data: root retains ownership
    /// and writes, while the selected primary group gets `r-x` only.
    pub fn grant_readonly_host_directory(
        &self,
        directory: &crate::PinnedDirectory,
    ) -> anyhow::Result<()> {
        self.validate().map_err(anyhow::Error::msg)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::fs::MetadataExt as _;

            directory.require_owner(0)?;
            let descriptor = directory.try_clone_descriptor()?;
            let metadata = descriptor.metadata()?;
            let AccountBackend::Unix { gid, .. } = self.0;
            if unsafe { libc::geteuid() } != 0
                || metadata.mode() & libc::S_IFMT != libc::S_IFDIR
                || metadata.mode() & 0o022 != 0
                || (metadata.gid() != 0 && metadata.gid() != gid)
            {
                anyhow::bail!(
                    "read-only host directory grant requires administrator authority and a safe root directory"
                );
            }
            if unsafe { libc::fchown(descriptor.as_raw_fd(), 0, gid) } != 0
                || unsafe {
                    libc::fchmod(descriptor.as_raw_fd(), DELEGATED_READONLY_DIRECTORY_MODE)
                } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = directory;
            anyhow::bail!("read-only host directory grants are unavailable on this OS")
        }
    }

    #[cfg(unix)]
    fn grant_private_descriptor(
        &self,
        file: &std::fs::File,
        directory: bool,
    ) -> anyhow::Result<()> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::MetadataExt as _;
        let AccountBackend::Unix { uid, gid } = self.0;
        let before = file.metadata()?;
        if unsafe { libc::geteuid() } != 0
            || (before.uid() != 0 && before.uid() != uid)
            || before.mode() & 0o022 != 0
            || (directory && !before.is_dir())
            || (!directory && (!before.is_file() || before.nlink() != 1))
        {
            anyhow::bail!(
                "private host grant requires administrator authority and an exact safe target"
            );
        }
        let mode = if directory { 0o700 } else { 0o600 };
        if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0
            || unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        file.sync_all()?;
        Ok(())
    }

    /// Give the selected account access to one existing native control FIFO.
    ///
    /// The containing directory remains administrator-owned, but gets only
    /// group traversal (`0710 root:<controller-gid>`). Native supervisors
    /// commonly create their control FIFO beneath a root-only directory; a
    /// `0600` FIFO alone is unreachable through that directory. Traversal
    /// exposes neither directory listing nor namespace mutation, while the
    /// FIFO itself remains owned and readable/writable only by the exact
    /// selected account. Never open a FIFO blocking, follow a symlink, or
    /// grant write access to the namespace.
    pub fn grant_private_control_fifo(
        &self,
        directory: &crate::PinnedDirectory,
        name: &std::ffi::OsStr,
    ) -> anyhow::Result<()> {
        self.validate().map_err(anyhow::Error::msg)?;
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd as _, FromRawFd as _};
            use std::os::unix::{ffi::OsStrExt as _, fs::MetadataExt as _};
            directory.require_owner(0)?;
            let bytes = name.as_bytes();
            if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
                anyhow::bail!("control FIFO must be one exact child name");
            }
            let name = std::ffi::CString::new(bytes)?;
            let parent = directory.try_clone_descriptor()?;
            let parent_metadata = parent.metadata()?;
            let AccountBackend::Unix { uid, gid } = self.0;
            if parent_metadata.uid() != 0
                || parent_metadata.mode() & libc::S_IFMT != libc::S_IFDIR
                || parent_metadata.mode() & 0o022 != 0
                || (parent_metadata.gid() != 0 && parent_metadata.gid() != gid)
            {
                anyhow::bail!(
                    "host control grant requires a safe administrator-owned control directory"
                );
            }
            // A named FIFO must be traversable by its one selected controller,
            // but the controller must never list or mutate the supervisor's
            // namespace. This native access-control translation belongs in
            // Lillux; callers only supply an already admitted account.
            if unsafe { libc::fchown(parent.as_raw_fd(), 0, gid) } != 0
                || unsafe { libc::fchmod(parent.as_raw_fd(), DELEGATED_CONTROL_DIRECTORY_MODE) }
                    != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            let before = file.metadata()?;
            if unsafe { libc::geteuid() } != 0
                || before.mode() & libc::S_IFMT != libc::S_IFIFO
                || before.nlink() != 1
                || (before.uid() != 0 && before.uid() != uid)
            {
                anyhow::bail!(
                    "host control grant requires an exact FIFO and administrator authority"
                );
            }
            if unsafe { libc::fchmod(fd, 0o600) } != 0 || unsafe { libc::fchown(fd, uid, gid) } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            // FIFOs are IPC objects, not durable regular files: no fsync.
            Ok(())
        }
        #[cfg(not(unix))]
        anyhow::bail!("native control FIFO grants are unavailable on this OS")
    }

    /// Used by native host provisioning. Deserialization alone grants no
    /// credential-drop authority; exec_controller validates again before use.
    pub fn unix(uid: u32, gid: u32) -> Self {
        Self(AccountBackend::Unix { uid, gid })
    }

    /// Exact numeric account coordinates for a native adapter that must name
    /// the account inside administrator-owned host configuration it renders.
    pub(crate) fn unix_ids(&self) -> Result<(u32, u32), String> {
        self.validate()?;
        let AccountBackend::Unix { uid, gid } = self.0;
        Ok((uid, gid))
    }

    pub fn validate(&self) -> Result<(), String> {
        match self.0 {
            AccountBackend::Unix { uid, gid }
                if uid != 0 && uid != u32::MAX && gid != 0 && gid != u32::MAX =>
            {
                Ok(())
            }
            _ => Err("controller requires an explicit non-root account".to_owned()),
        }
    }

    /// Corroborate the account after controller exec. Account selection alone
    /// is not evidence that credential drop actually happened.
    pub fn require_current_process(&self) -> anyhow::Result<()> {
        self.validate().map_err(anyhow::Error::msg)?;
        #[cfg(unix)]
        {
            let AccountBackend::Unix { uid, gid } = self.0;
            let (mut real_uid, mut effective_uid, mut saved_uid) = (u32::MAX, u32::MAX, u32::MAX);
            let (mut real_gid, mut effective_gid, mut saved_gid) = (u32::MAX, u32::MAX, u32::MAX);
            if unsafe { libc::getresuid(&mut real_uid, &mut effective_uid, &mut saved_uid) } != 0
                || unsafe { libc::getresgid(&mut real_gid, &mut effective_gid, &mut saved_gid) }
                    != 0
                || [real_uid, effective_uid, saved_uid]
                    .into_iter()
                    .any(|observed| observed != uid)
                || [real_gid, effective_gid, saved_gid]
                    .into_iter()
                    .any(|observed| observed != gid)
            {
                anyhow::bail!("controller process does not run as its selected non-root account");
            }
            let supplementary = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
            if supplementary != 0 {
                if supplementary < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                anyhow::bail!("controller process retained supplementary groups");
            }
            #[cfg(target_os = "linux")]
            if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
                anyhow::bail!("controller process did not retain the no-new-privileges transition");
            }
            Ok(())
        }
        #[cfg(not(unix))]
        anyhow::bail!("controller account observation is unavailable on this OS")
    }

    pub fn require_directory_owner(
        &self,
        directory: &crate::PinnedDirectory,
    ) -> anyhow::Result<()> {
        self.validate().map_err(anyhow::Error::msg)?;
        #[cfg(unix)]
        {
            let AccountBackend::Unix { uid, .. } = self.0;
            directory.require_owner(uid)
        }
        #[cfg(not(unix))]
        {
            let _ = directory;
            anyhow::bail!("controller account ownership is unavailable on this OS")
        }
    }
}

/// Require the administrator identity for a host-maintenance entrypoint.
///
/// This is deliberately an OS boundary rather than a RyeOS policy decision:
/// callers use it only before creating or replacing administrator-owned host
/// configuration. It is not an execution capability and must never be
/// threaded into worker requests, node policy, or durable execution state.
pub fn require_administrator() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::geteuid() } != 0 {
            anyhow::bail!("host maintenance requires administrator authority")
        }
        Ok(())
    }
    #[cfg(not(unix))]
    anyhow::bail!("host maintenance authority is unavailable on this OS")
}

/// Coarse host-lifetime witness, never process-control or launch authority.
/// A durable owner can retain this independently of its execution-row schema
/// to prove that even an unobserved pre-attachment process cannot still exist.
///
/// A lifetime is one kernel boot, and, when the node is the whole of a
/// container, that container's enclosure cgroup. The enclosure is recorded
/// only when the node runs alone in a private cgroup namespace rooted at its
/// own cgroup and a private pid namespace, and its exit is proven to end
/// that namespace: the node is pid 1, or its parent is a pid 1 that node
/// policy trusts to exit with its only child. Otherwise only the boot is
/// recorded and the enclosure generation stays undecided. Callers decide
/// lifetimes only while holding the node's state lock, which proves the
/// recording node has exited; with both, a different enclosure on the same
/// boot is a container that has ended. Absence of a process within the same
/// boot and enclosure is deliberately NOT claimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessHostLifetime {
    version: u32,
    backend: HostLifetimeBackend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "implementation", rename_all = "snake_case", deny_unknown_fields)]
enum HostLifetimeBackend {
    LinuxBoot { boot_id: String, cgroup_enclosure: Option<u64> },
}

/// Current boot and, when the node is the whole of a container whose
/// enclosure ends with it, the inode of that enclosure cgroup. `None` means
/// the boot is the only generation the recording node can vouch for.
#[cfg(target_os = "linux")]
fn capture_host_generation() -> Result<(String, Option<u64>), String> {
    let boot_id = current_boot_id()?;
    Ok((boot_id, super::host_capability::read_witnessed_enclosure()?))
}

#[cfg(target_os = "linux")]
fn current_boot_id() -> Result<String, String> {
    let boot_id = super::linux::read_boot_id()?;
    if !valid_boot_id(&boot_id) {
        return Err("current host lifetime is unavailable".to_owned());
    }
    Ok(boot_id)
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostGenerationChange {
    Same,
    BootEnded,
    EnclosureChanged,
}

/// Compare a recorded boot and enclosure with the current ones. The
/// recorder vouched that its exit ends the recorded enclosure, so the current
/// side needs only the enclosure it can see. A recording without an
/// enclosure is decided by boot alone: on the same boot it is never taken as
/// ended. If a recorded enclosure is no longer visible within one boot, the
/// generation cannot be decided and the caller must not settle anything
/// from it.
#[cfg(target_os = "linux")]
fn host_generation_change(
    boot_id: &str,
    cgroup_enclosure: Option<u64>,
) -> Result<HostGenerationChange, String> {
    let current_boot = current_boot_id()?;
    if current_boot != boot_id {
        return Ok(HostGenerationChange::BootEnded);
    }
    let Some(recorded) = cgroup_enclosure else {
        return Ok(HostGenerationChange::Same);
    };
    compare_enclosure(recorded, super::host_capability::read_cgroup_enclosure()?)
}

#[cfg(target_os = "linux")]
fn compare_enclosure(recorded: u64, current: Option<u64>) -> Result<HostGenerationChange, String> {
    match current {
        Some(current) if current != recorded => Ok(HostGenerationChange::EnclosureChanged),
        Some(_) => Ok(HostGenerationChange::Same),
        None => Err(
            "the recorded cgroup enclosure is no longer visible within one boot; the host \
             generation cannot be decided"
                .to_owned(),
        ),
    }
}

/// Lifetime-witness decision: a different boot, or a different container
/// enclosure under the conditions documented on `ProcessHostLifetime`.
#[cfg(target_os = "linux")]
fn host_generation_ended(boot_id: &str, cgroup_enclosure: Option<u64>) -> Result<bool, String> {
    Ok(host_generation_change(boot_id, cgroup_enclosure)? != HostGenerationChange::Same)
}

/// Scope decision. A different enclosure alone never ends a scope: if the
/// provider still reopens the exact recorded parent, the scope is observed
/// directly; only a parent replaced under the same path proves the old
/// enclosure, and every scope beneath it, is gone. An unreopenable parent
/// leaves the generation undecided.
#[cfg(target_os = "linux")]
fn scope_generation_ended(
    boot_id: &str,
    cgroup_enclosure: Option<u64>,
    configuration: &ProcessScopeConfiguration,
    recorded_parent: &crate::PinnedDirectoryIdentity,
) -> Result<bool, String> {
    match host_generation_change(boot_id, cgroup_enclosure)? {
        HostGenerationChange::Same => Ok(false),
        HostGenerationChange::BootEnded => Ok(true),
        HostGenerationChange::EnclosureChanged => {
            let provider = ProcessScopeProvider::open(configuration).map_err(|error| {
                format!(
                    "same boot with a different enclosure, and the recorded scope parent cannot \
                     be reopened ({error}); the scope generation cannot be decided"
                )
            })?;
            let ProviderBackend::LinuxCgroupV2(parent) = &provider.backend;
            Ok(parent.identity()? != *recorded_parent)
        }
    }
}

impl ProcessHostLifetime {
    pub fn capture_current() -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            let (boot_id, cgroup_enclosure) = capture_host_generation()?;
            let value = Self {
                version: HOST_LIFETIME_VERSION,
                backend: HostLifetimeBackend::LinuxBoot { boot_id, cgroup_enclosure },
            };
            value.validate()?;
            Ok(value)
        }
        #[cfg(not(target_os = "linux"))]
        Err("host lifetime capture is unavailable on this OS".to_owned())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != HOST_LIFETIME_VERSION {
            return Err("process host-lifetime witness contract is not current".to_owned());
        }
        match &self.backend {
            HostLifetimeBackend::LinuxBoot { boot_id, cgroup_enclosure: _ } if valid_boot_id(boot_id) => Ok(()),
            _ => Err("process host-lifetime witness is invalid".to_owned()),
        }
    }

    pub fn has_ended(&self) -> Result<bool, String> {
        self.validate()?;
        match &self.backend {
            #[cfg(target_os = "linux")]
            HostLifetimeBackend::LinuxBoot { boot_id, cgroup_enclosure } => {
                host_generation_ended(boot_id, *cgroup_enclosure)
            }
            #[cfg(not(target_os = "linux"))]
            _ => Err("host lifetime observation is unavailable on this OS".to_owned()),
        }
    }

    /// Join an exact process occurrence to this retained host lifetime.
    /// This checks birth identity only; it proves neither liveness nor cleanup.
    pub fn contains_process(&self, process: &super::ExactProcessIdentity) -> Result<bool, String> {
        self.validate()?;
        process.validate()?;
        match &self.backend {
            // Birth identity carries the boot only; a process of an ended
            // enclosure on this boot is excluded by has_ended, not here.
            HostLifetimeBackend::LinuxBoot { boot_id, cgroup_enclosure: _ } => Ok(process.boot_id == *boot_id),
        }
    }
}

/// Guarantees of the selected scope mechanism. Launch admission must also
/// establish that workloads cannot escape the scope (for example by reaching
/// a writable host control interface). Group stopping is not an implementation
/// of scope quiescence, and no unavailable guarantee has a weaker fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessScopeCapability {
    Quiescence,
    Termination,
    Recovery,
}

impl ProcessScopeCapability {
    pub const ALL: [Self; 3] = [Self::Quiescence, Self::Termination, Self::Recovery];

    /// The guarantees one successful lifecycle probe qualifies, given how its
    /// kernel barrier fared. Placement, kill and an observed empty scope
    /// qualify termination and recovery; quiescence needs the barrier to
    /// have been requested and held.
    pub fn qualified_by_probe(barrier: &ProbeBarrier) -> BTreeSet<Self> {
        let mut qualified: BTreeSet<Self> = [Self::Termination, Self::Recovery].into();
        if matches!(barrier, ProbeBarrier::Held) {
            qualified.insert(Self::Quiescence);
        }
        qualified
    }

    /// Open capability name this per-scope guarantee is published under.
    pub const fn capability_name(self) -> &'static str {
        match self {
            Self::Quiescence => "quiescence:scope",
            Self::Termination => "termination:scope",
            Self::Recovery => "recovery:scope",
        }
    }

    pub fn from_capability_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|capability| capability.capability_name() == name)
    }
}

/// How the kernel barrier fared in one lifecycle probe. A probe that does
/// not request the barrier never writes the freezer, so a host whose
/// freezer is refused still qualifies termination and recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeBarrier {
    /// The probe was asked not to exercise the barrier.
    NotRequested,
    /// The barrier froze the probe child before it was killed.
    Held,
    /// The barrier was requested and refused, for the reason given; the
    /// probe child was still killed and the scope observed empty.
    Refused(String),
}

/// Kernel-enforced ceilings for every task in one exact execution scope.
/// These are scope-wide limits, unlike per-process rlimits or account-wide
/// process limits. A backend without equivalent enforcement must refuse them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessScopeResourceLimits {
    pub maximum_memory_bytes: u64,
    pub maximum_processes: u32,
}

impl ProcessScopeResourceLimits {
    pub fn validate(self) -> Result<(), String> {
        if self.maximum_memory_bytes == 0 || self.maximum_processes == 0 {
            return Err("process scope resource limits must be positive".to_owned());
        }
        Ok(())
    }
}

/// Node-authorized stable backend selection. Ephemeral resource identities are
/// captured by the opened provider and journaled allocation, not authored into
/// a policy which must survive reboot. Its platform-specific shape belongs
/// to Lillux, not to an application's policy compiler. The owner must authorize
/// this complete value before opening it; deserialization grants no authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessScopeConfiguration {
    version: u32,
    backend: BackendConfiguration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "implementation", rename_all = "snake_case", deny_unknown_fields)]
enum BackendConfiguration {
    LinuxCgroupV2 { parent: std::path::PathBuf },
}

/// Prepared namespace allocation, retained by the caller's existing launch
/// journal BEFORE resource creation. It grants no process control or replayed
/// launch. A bound allocation must be controlled by its exact recovery record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessScopeAllocation {
    version: u32,
    configuration: ProcessScopeConfiguration,
    control_timeout: Duration,
    backend: BackendAllocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "implementation", rename_all = "snake_case", deny_unknown_fields)]
enum BackendAllocation {
    LinuxCgroupV2 {
        boot_id: String,
        cgroup_enclosure: Option<u64>,
        parent: crate::PinnedDirectoryIdentity,
        name: String,
    },
}

/// Immutable recovery evidence, not a transferable signal authority. Callers
/// bind it to their existing node/placement/attachment records. The backend
/// refuses a different configuration, boot, or exact resource incarnation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessScopeRecovery {
    version: u32,
    // Retain the original admitted node-local generation. Cleanup after a
    // policy change must not look up a replacement provider or require RyeOS
    // to decode platform paths. This grants control only, never new launch.
    configuration: ProcessScopeConfiguration,
    // Selected by the admitting owner, never an OS default. Retaining the
    // ceiling with this exact scope prevents restart or a generic subprocess
    // cleanup path from substituting a longer, unrelated shutdown budget.
    control_timeout: Duration,
    backend: BackendRecovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "implementation", rename_all = "snake_case", deny_unknown_fields)]
enum BackendRecovery {
    // Platform identity remains inside Lillux. The representation is available
    // on every OS so a foreign record can be retained without granting control.
    LinuxCgroupV2 {
        boot_id: String,
        cgroup_enclosure: Option<u64>,
        parent: crate::PinnedDirectoryIdentity,
        directory: crate::PinnedDirectoryIdentity,
        name: String,
    },
}

/// Opened, node-local provider of exact process scopes. Opening never installs
/// host software, grants privilege, creates delegation, or selects a fallback.
#[derive(Debug)]
pub struct ProcessScopeProvider {
    configuration: ProcessScopeConfiguration,
    configuration_digest: String,
    backend: ProviderBackend,
}

#[derive(Debug)]
enum ProviderBackend {
    #[cfg(target_os = "linux")]
    LinuxCgroupV2(super::cgroup::DelegatedCgroup),
}

#[derive(Debug)]
pub struct ProcessScope {
    recovery: ProcessScopeRecovery,
    backend: ScopeBackend,
    // Recovery can reacquire control, never permission to start another writer
    // in a resource whose prior execution may already have been settled.
    launch_available: bool,
}

/// SAME original provider/plan, retained before any creation or bind check.
/// The native frame may contain an unpinned created directory or partial FDs;
/// none of those states is positive local settlement or a new launch right.
#[cfg(target_os = "linux")]
pub(crate) struct SelectedScopePreparation {
    provider: std::sync::Arc<ProcessScopeProvider>,
    allocation: ProcessScopeAllocation,
    limits: Option<ProcessScopeResourceLimits>,
    original_service: crate::time::MonotonicDeadline,
    original_cleanup: crate::time::MonotonicDeadline,
    native: Option<std::sync::Arc<super::cgroup::SelectedCgroupPreparation>>,
    started: bool,
}

#[cfg(target_os = "linux")]
impl SelectedScopePreparation {
    pub(crate) fn retained(
        provider: std::sync::Arc<ProcessScopeProvider>,
        allocation: ProcessScopeAllocation,
        limits: Option<ProcessScopeResourceLimits>,
        original_service: crate::time::MonotonicDeadline,
        original_cleanup: crate::time::MonotonicDeadline,
    ) -> Self {
        Self {
            provider,
            allocation,
            limits,
            original_service,
            original_cleanup,
            native: None,
            started: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn same_provider_and_plan_for_component_test(
        &self,
        provider: &std::sync::Arc<ProcessScopeProvider>,
        allocation: &ProcessScopeAllocation,
    ) -> bool {
        std::sync::Arc::ptr_eq(&self.provider, provider) && &self.allocation == allocation
    }

    pub(crate) fn allocate_into(
        &mut self,
        destination: &mut Option<ProcessScope>,
    ) -> Result<(), String> {
        if self.started || self.native.is_some() || destination.is_some() {
            return Err("selected scope allocation cannot replace original owners".into());
        }
        self.started = true;
        if self.original_cleanup.is_before(&self.original_service) || self.original_service.has_elapsed() {
            return Err("selected allocation original pair unavailable".into());
        }
        self.allocation.validate()?;
        if self.allocation.configuration != self.provider.configuration || self.allocation.host_lifetime_ended()? {
            return Err("selected allocation provider or host generation changed".into());
        }
        let ProviderBackend::LinuxCgroupV2(parent) = &self.provider.backend;
        let BackendAllocation::LinuxCgroupV2 { boot_id, cgroup_enclosure, parent: expected_parent, name } = &self.allocation.backend;
        if parent.identity()? != *expected_parent {
            return Err("selected allocation parent incarnation changed".into());
        }
        let bound = self.original_service.min(crate::time::MonotonicDeadline::after(self.allocation.control_timeout));
        // Allocate the stable wrapper BEFORE acquiring parent/child/control
        // descriptors. Each owner stays in its same frame field thereafter.
        self.native = Some(std::sync::Arc::new(
            super::cgroup::SelectedCgroupPreparation::planned_with_limits(name, *expected_parent, bound, self.limits)?,
        ));
        let native = self.native.as_mut().ok_or("selected native allocation frame absent")?;
        let frame = std::sync::Arc::get_mut(native).ok_or("selected native frame unexpectedly shared")?;
        let allocation_outcome = parent.create_into_selected_slot(frame);
        let scope = match frame.complete() {
            Some(scope) => scope,
            None => return match allocation_outcome {
                Err(error) => Err(error),
                Ok(()) => Err("selected complete native scope absent".into()),
            },
        };
        // The SAME complete controls also remain on the preparation frame.
        // Construct metadata before adopting; post-bind/compiler checks cannot
        // run until actual scope is already in the destination storage.
        let launch_available = scope.launch_ready();
        let identity = scope.identity()?;
        let recovery = ProcessScopeRecovery {
            version: SCOPE_RECOVERY_VERSION,
            configuration: self.provider.configuration.clone(),
            control_timeout: self.allocation.control_timeout,
            backend: BackendRecovery::LinuxCgroupV2 {
                boot_id: boot_id.clone(),
                cgroup_enclosure: *cgroup_enclosure,
                parent: identity.parent,
                directory: identity.directory,
                name: identity.name.clone(),
            },
        };
        *destination = Some(ProcessScope {
            recovery,
            backend: ScopeBackend::LinuxCgroupV2Selected(std::sync::Arc::clone(native)),
            launch_available,
        });
        // Even late completion installs the SAME scope before propagating the
        // refusal, permitting original no-child observation without recreation.
        allocation_outcome?;
        if self.original_service.has_elapsed() || self.allocation.host_lifetime_ended()? {
            return Err("selected allocated scope generation/expiry changed; scope retained".into());
        }
        Ok(())
    }
}

/// A one-shot launch handle whose exact scope-wide memory and task ceilings
/// were installed and read back before any workload could enter the scope.
#[derive(Debug)]
pub struct ResourceLimitedProcessScope(ProcessScope);

/// Even a failure before target attachment retains the exact allocated scope
/// evidence. Callers must not lose its cleanup obligation with a spawn error.
#[derive(Debug)]
pub struct ProcessScopeLaunchError {
    pub recovery: ProcessScopeRecovery,
    pub result: crate::SubprocessResult,
}

#[derive(Debug)]
enum ScopeBackend {
    #[cfg(target_os = "linux")]
    LinuxCgroupV2(std::sync::Arc<super::cgroup::ProcessCgroup>),
    #[cfg(target_os = "linux")]
    LinuxCgroupV2Selected(std::sync::Arc<super::cgroup::SelectedCgroupPreparation>),
}

/// A completed kernel barrier. Exclusive workspace owners must consume it by
/// termination on cancellation; ordinary capture guards may resume on drop.
#[derive(Debug)]
pub struct QuiescedProcessScope {
    control_timeout: Duration,
    backend: QuiescedBackend,
}

#[derive(Debug)]
enum QuiescedBackend {
    #[cfg(target_os = "linux")]
    LinuxCgroupV2(super::cgroup::FrozenCgroup),
}

impl ProcessScopeConfiguration {
    /// Enter the exact OCI init mount namespace after preparation. Intended
    /// only for the short-lived hook process, which exits after publishing the
    /// protected binding and therefore never needs to recover its host view.
    pub fn enter_oci_mount_namespace(
        state: &super::OciHookState,
        intent: &super::OciLifecycleIntent,
        process_root: &super::ExactProcessRoot,
    ) -> Result<(), String> {
        state.validate_prestart()?;
        intent.require_live()?;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            if process_root.identity() != intent.init_process() {
                return Err("OCI process-root authority differs from lifecycle intent".to_owned());
            }
            let membership = process_root.membership()?;
            let namespace = process_root.mount_namespace()?;
            intent.require_live()?;
            super::cgroup::require_oci_membership(
                intent.init_process().target_pid,
                intent.lifecycle_path(),
            )?;
            if unsafe { libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNS) } != 0 {
                return Err(format!(
                    "enter OCI init mount namespace: {}",
                    std::io::Error::last_os_error()
                ));
            }
            super::cgroup::require_oci_membership_file(&membership, intent.lifecycle_path())?;
            process_root.require_retained_lifetime()?;
            // The namespace and proc-membership descriptors pin the exact
            // objects selected between exact-process checks. The post-setns
            // check uses the retained proc descriptor rather than assuming
            // the container procfs exposes the host PID.
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        Err("OCI mount namespace entry is unavailable on this OS".to_owned())
    }

    /// OCI prestart hook operation. Lillux derives C from the exact init PID,
    /// creates R directly beneath it, and installs the R-rooted mount in the
    /// container namespace before returning an opaque provider configuration.
    pub fn prepare_oci_hook(
        state: &super::OciHookState,
        account: &ControllerAccount,
        intent: &super::OciLifecycleIntent,
        process_root: &super::ExactProcessRoot,
        node_resources: ProcessScopeResourceLimits,
        mut observer: impl FnMut(&super::OciPreparationProgress) -> Result<(), String>,
    ) -> Result<(Self, super::OciLifecycleGeneration), String> {
        state.validate_prestart()?;
        super::require_administrator().map_err(|error| error.to_string())?;
        #[cfg(target_os = "linux")]
        {
            account.validate()?;
            intent.validate()?;
            if intent.container_id() != state.id {
                return Err("OCI state differs from the durable lifecycle intent".to_owned());
            }
            if process_root.identity() != intent.init_process() {
                return Err("OCI process-root authority differs from lifecycle intent".to_owned());
            }
            let AccountBackend::Unix { uid, gid } = account.0;
            let prepared = super::cgroup::prepare_oci_controller_root(intent, uid, gid, node_resources, &mut observer)?;
            let lifecycle = (|| {
                let host_lifetime = intent.host_lifetime().clone();
                let generation_payload = serde_json::json!({
                    "container_id": &state.id,
                    "host_lifetime": &host_lifetime,
                    "init_process": &prepared.init_process,
                    "lifecycle_scope": &prepared.lifecycle_scope,
                    "controller_scope": &prepared.controller_scope,
                    "init_scope": &prepared.init_scope,
                    "node_resources": &prepared.node_resources,
                    "nonce": intent.nonce(),
                });
                let generation = format!(
                    "sha256:{}",
                    crate::sha256_hex(
                        crate::canonical_json(&generation_payload)
                            .map_err(|error| error.to_string())?
                            .as_bytes()
                    )
                );
                super::OciLifecycleGeneration::new(
                    state.id.clone(),
                    host_lifetime,
                    prepared.init_process.clone(),
                    prepared.lifecycle_scope,
                    prepared.controller_scope,
                    prepared.init_scope,
                    prepared.node_resources,
                    prepared.lifecycle_path.clone(),
                    prepared.controller_path.clone(),
                    generation,
                )
            })();
            let lifecycle = match lifecycle {
                Ok(lifecycle) => lifecycle,
                Err(error) => {
                    return Err(format!("{error}; original OCI preparation retained for ended-lifetime recovery"));
                }
            };
            let configuration = Self {
                version: SCOPE_CONFIGURATION_VERSION,
                backend: BackendConfiguration::LinuxCgroupV2 {
                    parent: "/sys/fs/cgroup".into(),
                },
            };
            if let Err(error) = configuration.validate() {
                return Err(format!("{error}; original OCI preparation retained for ended-lifetime recovery"));
            }
            if let Err(error) = super::cgroup::install_oci_controller_mount(&prepared, process_root)
            {
                return Err(format!("{error}; original OCI preparation retained for ended-lifetime recovery"));
            }
            Ok((configuration, lifecycle))
        }
        #[cfg(not(target_os = "linux"))]
        Err("OCI lifecycle preparation is unavailable on this OS".to_owned())
    }

    pub fn require_oci_generation_as_controller(
        &self,
        account: &ControllerAccount,
        lifecycle: &super::OciLifecycleGeneration,
    ) -> Result<(), String> {
        account
            .require_current_process()
            .map_err(|error| error.to_string())?;
        lifecycle.validate()?;
        let provider = ProcessScopeProvider::open(self)?;
        match &provider.backend {
            #[cfg(target_os = "linux")]
            ProviderBackend::LinuxCgroupV2(parent)
                if parent.identity()? == lifecycle.controller_scope() =>
            {
                Ok(())
            }
            #[cfg(target_os = "linux")]
            ProviderBackend::LinuxCgroupV2(_) => {
                Err("OCI controller-root generation has been replaced".to_owned())
            }
            #[cfg(not(target_os = "linux"))]
            _ => Err("OCI lifecycle validation is unavailable on this OS".to_owned()),
        }
    }

    /// Root-side validation observes the selected non-root account without
    /// entering the normal controller opener as that account.
    pub fn require_oci_generation_as_administrator(
        &self,
        account: &ControllerAccount,
        lifecycle: &super::OciLifecycleGeneration,
    ) -> Result<(), String> {
        self.validate()?;
        account.validate()?;
        lifecycle.validate()?;
        super::require_administrator().map_err(|error| error.to_string())?;
        #[cfg(target_os = "linux")]
        {
            let BackendConfiguration::LinuxCgroupV2 { parent } = &self.backend;
            if parent != std::path::Path::new("/sys/fs/cgroup") {
                return Err(
                    "OCI controller delegation has a noncanonical container path".to_owned(),
                );
            }
            let AccountBackend::Unix { uid, gid } = account.0;
            super::cgroup::validate_administrator_oci_delegation(
                parent,
                uid,
                gid,
                lifecycle.controller_scope(),
            )?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        Err("OCI lifecycle validation is unavailable on this OS".to_owned())
    }

    /// Provision one administrator-owned host-service delegation and compile
    /// the current platform's opaque scope contract. Applications supply only
    /// their already-determined native service label; they never choose a
    /// cgroup path, backend, or OS-specific delegation mechanism.
    pub fn provision_host_delegation(service_label: &str) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            let parent = super::cgroup::provision_host_delegation(service_label)?;
            let configuration = Self {
                version: SCOPE_CONFIGURATION_VERSION,
                backend: BackendConfiguration::LinuxCgroupV2 { parent },
            };
            configuration.validate()?;
            Ok(configuration)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = service_label;
            Err("the current host has no supported process-scope backend".to_owned())
        }
    }

    /// Host-maintenance observation, never a worker recovery or cleanup grant.
    /// The caller must retain its launch exclusion and must not infer this
    /// result from a controller PID or erase individual recovery obligations.
    pub fn require_controller_tree_empty(&self, account: &ControllerAccount) -> Result<(), String> {
        self.validate()?;
        account.validate()?;
        #[cfg(target_os = "linux")]
        {
            let BackendConfiguration::LinuxCgroupV2 { parent } = &self.backend;
            let AccountBackend::Unix { uid, .. } = account.0;
            super::cgroup::require_controller_tree_empty(parent, uid)
        }
        #[cfg(not(target_os = "linux"))]
        Err("controller process-tree observation is unavailable on this OS".to_owned())
    }

    /// Controller-side proof that the current process was placed by the host
    /// supervisor into this delegation's administrator-owned controller leaf.
    /// Association alone is not placement: a process started directly on an
    /// associated app root is refused, naming its cgroup and the expected leaf.
    pub fn require_current_process_in_controller(
        &self,
        account: &ControllerAccount,
    ) -> Result<(), String> {
        self.validate()?;
        account.validate()?;
        #[cfg(target_os = "linux")]
        {
            let BackendConfiguration::LinuxCgroupV2 { parent } = &self.backend;
            let AccountBackend::Unix { uid, .. } = account.0;
            super::cgroup::require_current_process_in_controller(parent, uid)
        }
        #[cfg(not(target_os = "linux"))]
        Err("controller placement observation is unavailable on this OS".to_owned())
    }

    /// Explicit host-supervisor entry, not worker execution. Replaces this
    /// administrator process with an unprivileged controller after exact
    /// placement. It installs no service and changes no application policy.
    pub fn exec_controller(
        &self,
        account: &ControllerAccount,
        node_resources: ProcessScopeResourceLimits,
        executable: &crate::PinnedRegularFile,
        arguments: &[String],
        cwd: &crate::PinnedDirectory,
        environment: &[(String, String)],
    ) -> Result<std::convert::Infallible, String> {
        self.exec_controller_inner(account, node_resources, executable, arguments, cwd, environment, None, None)
    }

    /// External-supervisor entry using one exact administrator-owned launch
    /// document. This has the same placement and credential-drop semantics as
    /// a native host service; only the protected transport differs.
    pub fn exec_controller_with_inherited_document(
        &self,
        account: &ControllerAccount,
        node_resources: ProcessScopeResourceLimits,
        executable: &crate::PinnedRegularFile,
        arguments: &[String],
        cwd: &crate::PinnedDirectory,
        environment: &[(String, String)],
        document: crate::InheritedReadonlyDocument,
        descriptor_env_name: &str,
    ) -> Result<std::convert::Infallible, String> {
        self.exec_controller_inner(
            account,
            node_resources,
            executable,
            arguments,
            cwd,
            environment,
            Some((document, descriptor_env_name)),
            None,
        )
    }

    /// External-supervisor entry for an exact OCI delegation prepared before
    /// container start. This is deliberately distinct from native delegation
    /// creation and is authorized by the retained lifecycle generation.
    pub fn exec_prepared_oci_controller_with_inherited_document(
        &self,
        account: &ControllerAccount,
        node_resources: ProcessScopeResourceLimits,
        lifecycle: &super::OciLifecycleGeneration,
        executable: &crate::PinnedRegularFile,
        arguments: &[String],
        cwd: &crate::PinnedDirectory,
        environment: &[(String, String)],
        document: crate::InheritedReadonlyDocument,
        descriptor_env_name: &str,
    ) -> Result<std::convert::Infallible, String> {
        self.exec_controller_inner(
            account,
            node_resources,
            executable,
            arguments,
            cwd,
            environment,
            Some((document, descriptor_env_name)),
            Some(lifecycle),
        )
    }

    fn exec_controller_inner(
        &self,
        account: &ControllerAccount,
        node_resources: ProcessScopeResourceLimits,
        executable: &crate::PinnedRegularFile,
        arguments: &[String],
        cwd: &crate::PinnedDirectory,
        environment: &[(String, String)],
        inherited_document: Option<(crate::InheritedReadonlyDocument, &str)>,
        oci_lifecycle: Option<&super::OciLifecycleGeneration>,
    ) -> Result<std::convert::Infallible, String> {
        self.validate()?;
        account.validate()?;
        node_resources.validate()?;
        if !executable.path().is_absolute() || !cwd.path().is_absolute() {
            return Err("controller executable and cwd must be explicit absolute paths".to_owned());
        }
        executable
            .require_executable()
            .map_err(|error| error.to_string())?;
        let mut names = BTreeSet::new();
        for (name, value) in environment {
            if name.is_empty()
                || name.contains(['=', '\0'])
                || value.contains('\0')
                || !names.insert(name)
            {
                return Err("controller environment must have unique nonempty exact names and NUL-free values".to_owned());
            }
        }
        if let Some((_, descriptor_env_name)) = inherited_document.as_ref()
            && names
                .iter()
                .any(|name| name.as_str() == *descriptor_env_name)
        {
            return Err(
                "controller environment duplicates inherited document transport".to_owned(),
            );
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt as _;
            // Retain the exact selected directory across provisioning and
            // credential drop. Reopening cwd.path() here would discard the
            // caller's host association check if its parent were renamed.
            let BackendConfiguration::LinuxCgroupV2 { parent } = &self.backend;
            let AccountBackend::Unix { uid, gid } = account.0;
            // Derive transport from the verified file, never reopen its original
            // pathname after provisioning or credential drop. Use the existing
            // inherited-authority owner rather than another raw-FD protocol.
            let image = executable
                .inherited_descriptor_authority()
                .map_err(|error| error.to_string())?;
            let mut command = std::process::Command::new(image.path());
            crate::configure_inherited_descriptor_authorities(
                &mut command,
                std::slice::from_ref(&image),
            )?;
            let bootstrap = match oci_lifecycle {
                Some(lifecycle) => {
                    self.require_oci_generation_as_administrator(account, lifecycle)?;
                    super::cgroup::adopt_prepared_oci_controller(
                        parent,
                        uid,
                        gid,
                        lifecycle.controller_scope(),
                    )?
                }
                None => super::cgroup::provision_controller(parent, uid, gid)?,
            };
            // Enclosing node ceilings dominate controller and worker scopes,
            // including authorized intra-node placement changes. Install them
            // before controller placement, privilege drop and daemon exec.
            bootstrap.install_resource_limits(node_resources)?;
            // Per-process ceilings survive credential drop and exec. Raise them
            // while still administrator: the daemon locks its resident region
            // at startup, and host supervisors commonly inherit a small
            // RLIMIT_MEMLOCK. A prepared OCI controller keeps its runtime's
            // configured ceilings.
            if oci_lifecycle.is_none() {
                super::cgroup::install_controller_rlimits(node_resources)?;
            }
            command
                .args(arguments)
                .env_clear()
                .envs(environment.iter().map(|(key, value)| (key, value)));
            if let Some((document, descriptor_env_name)) = inherited_document {
                document.bind_to_command(&mut command, descriptor_env_name)?;
            }
            bootstrap.configure_command(&mut command)?;
            cwd.configure_command_cwd(&mut command)
                .map_err(|error| error.to_string())?;
            // A host supervisor commonly runs this entry inside its own
            // administrator-owned session and process group. The controller
            // must lead its own: its exact identity names its group leader,
            // and group termination must never reach the supervisor. Exec
            // replaces this process, so the session placement carries over.
            super::establish_own_process_session()?;
            Err(format!(
                "exec unprivileged scope controller: {}",
                command.exec()
            ))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (account, arguments, inherited_document, oci_lifecycle);
            Err("scope controller provisioning is unavailable on this OS".to_owned())
        }
    }

    /// Pure validation only. It does not inspect or mutate the current host.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != SCOPE_CONFIGURATION_VERSION {
            return Err("process scope configuration contract is not current".to_owned());
        }
        match &self.backend {
            BackendConfiguration::LinuxCgroupV2 { parent, .. } => {
                if !parent.is_absolute()
                    || parent.components().any(|part| {
                        matches!(
                            part,
                            std::path::Component::ParentDir | std::path::Component::CurDir
                        )
                    })
                    || parent.parent().is_none()
                {
                    return Err(
                        "process scope backend requires an exact absolute delegation path"
                            .to_owned(),
                    );
                }
            }
        }
        Ok(())
    }

    fn digest(&self) -> Result<String, String> {
        self.validate()?;
        let value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        Ok(crate::sha256_hex(
            crate::canonical_json(&value)
                .map_err(|error| error.to_string())?
                .as_bytes(),
        ))
    }
}

impl ProcessScopeProvider {
    pub fn open(configuration: &ProcessScopeConfiguration) -> Result<Self, String> {
        let configuration_digest = configuration.digest()?;
        match &configuration.backend {
            BackendConfiguration::LinuxCgroupV2 { parent } => {
                #[cfg(target_os = "linux")]
                {
                    Ok(Self {
                        configuration: configuration.clone(),
                        configuration_digest,
                        backend: ProviderBackend::LinuxCgroupV2(
                            super::cgroup::DelegatedCgroup::open(parent)?,
                        ),
                    })
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = (configuration_digest, parent);
                    Err("process scope backend is unavailable on this OS".to_owned())
                }
            }
        }
    }

    /// Implemented semantics, NOT an attestation that this caller can launch
    /// into the configured delegation. Opening kernel files cannot prove the
    /// source/common-ancestor migration permissions. Admission must qualify an
    /// exact reserved scope, and real launch still performs its own placement.
    pub fn implemented_capabilities(&self) -> BTreeSet<ProcessScopeCapability> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            ProviderBackend::LinuxCgroupV2(_) => [
                ProcessScopeCapability::Quiescence,
                ProcessScopeCapability::Termination,
                ProcessScopeCapability::Recovery,
            ]
            .into(),
            #[cfg(not(target_os = "linux"))]
            _ => unreachable!("no process scope provider is implemented on this OS"),
        }
    }

    /// Qualify this opened generation using one disposable kernel-only probe.
    /// The caller controls the deadline; reservation, probe and retirement
    /// share it. Placement, kill and an observed empty scope are always
    /// probed; the kernel barrier only when `wanted` names quiescence, and
    /// its refusal withholds quiescence alone. A failed placement, kill or
    /// retirement qualifies nothing.
    pub fn qualify(
        &self,
        timeout: Duration,
        wanted: &BTreeSet<ProcessScopeCapability>,
    ) -> Result<BTreeSet<ProcessScopeCapability>, String> {
        let started = std::time::Instant::now();
        let allocation = format!(
            "inspection-{}",
            crate::sha256_hex(&crate::crypto::generate_random_bytes::<32>())
        );
        let mut probe = self.reserve(&allocation, timeout)?;
        let recovery = probe.recovery().clone();
        let barrier_wanted = wanted.contains(&ProcessScopeCapability::Quiescence);
        let qualification =
            probe.probe_lifecycle(timeout.saturating_sub(started.elapsed()), barrier_wanted);
        let retirement = self.retire(&recovery, timeout.saturating_sub(started.elapsed()));
        match (qualification, retirement) {
            (Ok(barrier), Ok(())) => Ok(self
                .implemented_capabilities()
                .intersection(&ProcessScopeCapability::qualified_by_probe(&barrier))
                .copied()
                .collect()),
            (qualification, retirement) => Err(format!(
                "process scope qualification failed: {qualification:?}; retirement: {retirement:?}; retained recovery: {recovery:?}"
            )),
        }
    }

    /// Internal disposable qualification shortcut. Durable execution owners
    /// must use plan_allocation -> journal -> allocate -> bind -> held spawn;
    /// do not expose a create-before-journal shortcut to application code.
    fn reserve(&self, allocation: &str, timeout: Duration) -> Result<ProcessScope, String> {
        let planned = self.plan_allocation(allocation, timeout)?;
        self.allocate(&planned)
    }

    /// Preparation after provider admission without creating a resource.
    /// The caller's worker identity must uniquely own this allocation name.
    pub fn plan_allocation(
        &self,
        allocation: &str,
        timeout: Duration,
    ) -> Result<ProcessScopeAllocation, String> {
        validate_control_timeout(timeout)?;
        validate_allocation_name(allocation)?;
        match &self.backend {
            #[cfg(target_os = "linux")]
            ProviderBackend::LinuxCgroupV2(parent) => {
                let (boot_id, cgroup_enclosure) = capture_host_generation()?;
                Ok(ProcessScopeAllocation {
                    version: SCOPE_ALLOCATION_VERSION,
                    configuration: self.configuration.clone(),
                    control_timeout: timeout,
                    backend: BackendAllocation::LinuxCgroupV2 {
                        boot_id,
                        cgroup_enclosure,
                        parent: parent.identity()?,
                        name: allocation.to_owned(),
                    },
                })
            }
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scopes are unavailable on this OS".to_owned()),
        }
    }

    /// Call only after committing the allocation to the existing one-shot
    /// launch journal. Never replay creation from a recovered planned record;
    /// recovery may discard an unused slot but cannot create another writer.
    pub fn allocate(&self, allocation: &ProcessScopeAllocation) -> Result<ProcessScope, String> {
        allocation.validate()?;
        if allocation.configuration != self.configuration || allocation.host_lifetime_ended()? {
            return Err("process scope allocation provider or host lifetime changed".to_owned());
        }
        match &self.backend {
            #[cfg(target_os = "linux")]
            ProviderBackend::LinuxCgroupV2(parent) => {
                let BackendAllocation::LinuxCgroupV2 {
                    boot_id,
                    cgroup_enclosure,
                    parent: expected_parent,
                    name,
                } = &allocation.backend;
                if parent.identity()? != *expected_parent {
                    return Err("process scope allocation provider incarnation changed".to_owned());
                }
                let scope = parent.create(name, allocation.control_timeout)?;
                let identity = scope.identity();
                Ok(ProcessScope {
                    recovery: ProcessScopeRecovery {
                        version: SCOPE_RECOVERY_VERSION,
                        configuration: self.configuration.clone(),
                        control_timeout: allocation.control_timeout,
                        backend: BackendRecovery::LinuxCgroupV2 {
                            boot_id: boot_id.clone(),
                            cgroup_enclosure: *cgroup_enclosure,
                            parent: identity.parent,
                            directory: identity.directory,
                            name: identity.name.clone(),
                        },
                    },
                    backend: ScopeBackend::LinuxCgroupV2(scope),
                    launch_available: true,
                })
            }
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = allocation;
                Err("process scopes are unavailable on this OS".to_owned())
            }
        }
    }

    /// Selected lexical allocation mechanics only; this does not issue a root
    /// loan or authenticate App/Executor admission. Retain SAME Arc provider
    /// and plan in private storage before the first native creation operation.
    #[cfg(target_os = "linux")]
    pub fn allocate_into_selected_slot(
        self: std::sync::Arc<Self>,
        allocation: ProcessScopeAllocation,
        destination: &mut crate::exec::SelectedScopedBorrow<'_>,
        original_service: crate::time::MonotonicDeadline,
        original_cleanup: crate::time::MonotonicDeadline,
    ) -> Result<(), String> {
        destination.allocate_scope_into(self, allocation, None, original_service, original_cleanup)
    }

    /// Owner-preserving resource variant: limits are installed/read back in
    /// the original retained allocation frame, never a consuming scope API.
    #[cfg(target_os = "linux")]
    pub fn allocate_resource_into_selected_slot(
        self: std::sync::Arc<Self>,
        allocation: ProcessScopeAllocation,
        limits: ProcessScopeResourceLimits,
        destination: &mut crate::exec::SelectedScopedBorrow<'_>,
        original_service: crate::time::MonotonicDeadline,
        original_cleanup: crate::time::MonotonicDeadline,
    ) -> Result<(), String> {
        destination.allocate_scope_into(self, allocation, Some(limits), original_service, original_cleanup)
    }

    /// The caller must already hold its existing placement/recovery fence.
    /// Evidence is local to the selected provider and boot; remote handoff
    /// reserves a fresh scope rather than transferring this authority.
    pub fn recover(
        &self,
        recovery: &ProcessScopeRecovery,
        timeout: Duration,
    ) -> Result<ProcessScope, String> {
        self.validate_recovery(recovery)?;
        let timeout = timeout.min(recovery.control_timeout);
        match (&self.backend, &recovery.backend) {
            #[cfg(target_os = "linux")]
            (
                ProviderBackend::LinuxCgroupV2(parent),
                BackendRecovery::LinuxCgroupV2 {
                    boot_id,
                    cgroup_enclosure,
                    parent: owner,
                    directory,
                    name,
                },
            ) => {
                // validate_recovery above already proved this provider holds
                // the exact recorded parent, so only a different boot ends it.
                if host_generation_change(boot_id, *cgroup_enclosure)? == HostGenerationChange::BootEnded {
                    return Err("process scope recovery belongs to another boot".to_owned());
                }
                let scope = parent.reopen(
                    &super::cgroup::CgroupIdentity {
                        parent: *owner,
                        directory: *directory,
                        name: name.clone(),
                    },
                    timeout,
                )?;
                Ok(ProcessScope {
                    recovery: recovery.clone(),
                    backend: ScopeBackend::LinuxCgroupV2(scope),
                    launch_available: false,
                })
            }
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
                Err("process scope recovery is unavailable on this OS".to_owned())
            }
        }
    }

    /// Call only after durable attachment/workspace settlement excludes any
    /// later launch into this resource. Resource removal is not settlement.
    pub fn retire(&self, recovery: &ProcessScopeRecovery, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(recovery.control_timeout);
        let started = std::time::Instant::now();
        let scope = self.recover(recovery, timeout)?;
        match (&self.backend, &scope.backend) {
            #[cfg(target_os = "linux")]
            (ProviderBackend::LinuxCgroupV2(parent), ScopeBackend::LinuxCgroupV2(scope)) => {
                parent.retire_empty(scope.identity(), timeout.saturating_sub(started.elapsed()))
            }
            #[cfg(target_os = "linux")]
            (ProviderBackend::LinuxCgroupV2(_), ScopeBackend::LinuxCgroupV2Selected(_)) => {
                Err("selected frame grants no ordinary retirement".into())
            }
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope retirement is unavailable on this OS".to_owned()),
        }
    }

    fn validate_recovery(&self, recovery: &ProcessScopeRecovery) -> Result<(), String> {
        recovery.validate()?;
        if recovery.configuration.digest()? != self.configuration_digest {
            return Err(
                "process scope recovery contract or configured provider changed".to_owned(),
            );
        }
        #[cfg(target_os = "linux")]
        {
            let ProviderBackend::LinuxCgroupV2(parent) = &self.backend;
            let BackendRecovery::LinuxCgroupV2 {
                parent: expected, ..
            } = &recovery.backend;
            if parent.identity()? != *expected {
                return Err("retained scope names another provider incarnation".to_owned());
            }
        }
        Ok(())
    }

    /// Bind compilation to the captured provider, not merely the stable
    /// configuration path. A replacement at that path is a different authority.
    pub fn validate_scope(&self, scope: &ProcessScope) -> Result<(), String> {
        self.validate_recovery(scope.recovery())?;
        if scope.recovery().host_lifetime_ended()? {
            return Err("retained scope belongs to an ended host lifetime".to_owned());
        }
        Ok(())
    }
}

impl ProcessScopeAllocation {
    pub fn host_lifetime(&self) -> Result<ProcessHostLifetime, String> {
        self.validate()?;
        match &self.backend {
            BackendAllocation::LinuxCgroupV2 { boot_id, cgroup_enclosure, parent: _, name: _ } => Ok(ProcessHostLifetime {
                version: HOST_LIFETIME_VERSION,
                backend: HostLifetimeBackend::LinuxBoot {
                    boot_id: boot_id.clone(),
                    cgroup_enclosure: *cgroup_enclosure,
                },
            }),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.version != SCOPE_ALLOCATION_VERSION {
            return Err("process scope allocation contract is not current".to_owned());
        }
        self.configuration.validate()?;
        validate_control_timeout(self.control_timeout)?;
        match &self.backend {
            BackendAllocation::LinuxCgroupV2 { boot_id, name, .. } => {
                if !valid_boot_id(boot_id) {
                    return Err("process scope allocation has no exact host lifetime".to_owned());
                }
                validate_allocation_name(name)
            }
        }
    }

    /// The existing journal must prove the allocation NEVER became bound for
    /// launch and fence its prior creator. This is namespace-slot cleanup, not
    /// live process recovery: absence is fine; a populated slot is refused,
    /// never killed. Once bound, use exact ProcessScopeRecovery instead.
    pub fn discard_unlaunched(&self) -> Result<(), String> {
        if self.host_lifetime_ended()? {
            return Ok(());
        }
        let provider = ProcessScopeProvider::open(&self.configuration)?;
        match (&provider.backend, &self.backend) {
            #[cfg(target_os = "linux")]
            (
                ProviderBackend::LinuxCgroupV2(parent),
                BackendAllocation::LinuxCgroupV2 {
                    name,
                    parent: expected_parent,
                    ..
                },
            ) => {
                if parent.identity()? != *expected_parent {
                    return Err("unbound allocation parent incarnation changed".to_owned());
                }
                parent.discard_unlaunched_slot(name)
            }
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope allocation cleanup is unavailable on this OS".to_owned()),
        }
    }

    fn host_lifetime_ended(&self) -> Result<bool, String> {
        self.validate()?;
        match &self.backend {
            #[cfg(target_os = "linux")]
            BackendAllocation::LinuxCgroupV2 { boot_id, cgroup_enclosure, parent, name: _ } => {
                scope_generation_ended(boot_id, *cgroup_enclosure, &self.configuration, parent)
            }
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope lifetime observation is unavailable on this OS".to_owned()),
        }
    }
}

impl ProcessScopeRecovery {
    /// Exact binding of a completed allocation. The concrete resource identity
    /// is new evidence, but provider, lifetime, slot and budget cannot change.
    pub fn matches_allocation(&self, allocation: &ProcessScopeAllocation) -> bool {
        if self.validate().is_err()
            || allocation.validate().is_err()
            || self.configuration != allocation.configuration
            || self.control_timeout != allocation.control_timeout
        {
            return false;
        }
        match (&self.backend, &allocation.backend) {
            (
                BackendRecovery::LinuxCgroupV2 {
                    boot_id,
                    cgroup_enclosure,
                    parent,
                    name,
                    directory: _,
                },
                BackendAllocation::LinuxCgroupV2 {
                    boot_id: expected_boot,
                    cgroup_enclosure: expected_enclosure,
                    parent: expected_parent,
                    name: expected_name,
                },
            ) => {
                boot_id == expected_boot
                    && cgroup_enclosure == expected_enclosure
                    && parent == expected_parent
                    && name == expected_name
            }
        }
    }
    /// Original admitted per-operation ceiling; not the worker lifetime or a
    /// cooperative cancellation grace. Callers may impose shorter deadlines.
    pub fn control_timeout(&self) -> Duration {
        self.control_timeout
    }

    /// Complete an existing durable retirement intent after every launch and
    /// workspace owner has settled. Never use this to prove process death.
    /// Replaying a committed removal may observe absence; replacement and a
    /// populated resource remain errors. No active control operation gains
    /// this interpretation of a missing scope.
    pub fn retire_after_settlement(&self) -> Result<(), String> {
        if self.host_lifetime_ended()? {
            return Ok(());
        }
        let provider = ProcessScopeProvider::open(&self.configuration)?;
        provider.validate_recovery(self)?;
        match (&provider.backend, &self.backend) {
            #[cfg(target_os = "linux")]
            (
                ProviderBackend::LinuxCgroupV2(parent),
                BackendRecovery::LinuxCgroupV2 {
                    parent: owner,
                    directory,
                    name,
                    ..
                },
            ) => parent.retire_settled(&super::cgroup::CgroupIdentity {
                parent: *owner,
                directory: *directory,
                name: name.clone(),
            }),
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope retirement is unavailable on this OS".to_owned()),
        }
    }

    /// Check consistency with the process attachment before granting control.
    /// This is not live membership evidence; held launch owns that proof.
    pub fn validate_for_process(
        &self,
        process: &super::ExactProcessIdentity,
    ) -> Result<(), String> {
        self.validate()?;
        process.validate()?;
        match &self.backend {
            BackendRecovery::LinuxCgroupV2 { boot_id, .. } if *boot_id == process.boot_id => Ok(()),
            _ => Err("process attachment and scope name different host lifetimes".to_owned()),
        }
    }

    pub fn quiesce(&self, timeout: Duration) -> Result<QuiescedProcessScope, String> {
        let timeout = timeout.min(self.control_timeout);
        let started = std::time::Instant::now();
        self.recover(timeout)?
            .quiesce(timeout.saturating_sub(started.elapsed()))
    }

    /// Caller must own the existing durable barrier's recovery fence.
    pub fn recover_quiesced(&self, timeout: Duration) -> Result<QuiescedProcessScope, String> {
        let timeout = timeout.min(self.control_timeout);
        let started = std::time::Instant::now();
        self.recover(timeout)?
            .recover_quiesced(timeout.saturating_sub(started.elapsed()))
    }
    /// Observe the whole retained scope, not only the original target or its
    /// group. Errors remain indeterminate; callers must not turn them into
    /// absence or an empty-scope proof.
    pub fn is_empty(&self, timeout: Duration) -> Result<bool, String> {
        if self.host_lifetime_ended()? {
            return Ok(true);
        }
        self.recover(timeout)?.is_empty()
    }
    /// Validate retained evidence without opening host resources or granting
    /// authority. The application must bind the complete record to its exact
    /// existing execution/attachment owner before invoking control operations.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != SCOPE_RECOVERY_VERSION {
            return Err("process scope recovery contract is not current".to_owned());
        }
        self.configuration.validate()?;
        validate_control_timeout(self.control_timeout)?;
        match (&self.configuration.backend, &self.backend) {
            (
                BackendConfiguration::LinuxCgroupV2 { .. },
                BackendRecovery::LinuxCgroupV2 { boot_id, name, .. },
            ) => {
                if !valid_boot_id(boot_id) {
                    return Err(
                        "process scope recovery contradicts its retained provider".to_owned()
                    );
                }
                validate_allocation_name(name)
            }
        }
    }

    /// Reacquire ONLY the control authority named by an already-authorized
    /// durable attachment. Callers hold their existing launch/placement fence.
    /// This opens the original pinned generation, not current policy or a
    /// same-named replacement, and cannot be used to launch another process.
    pub fn recover(&self, timeout: Duration) -> Result<ProcessScope, String> {
        self.validate()?;
        ProcessScopeProvider::open(&self.configuration)?.recover(self, timeout)
    }

    /// Prove that the retained execution scope is empty. An ended host boot is
    /// a kernel lifetime proof, not a missing-path fallback. On the same boot,
    /// absent/replaced resources and unavailable control remain hard errors.
    pub fn wait_empty(&self, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.control_timeout);
        if self.host_lifetime_ended()? {
            return Ok(());
        }
        let started = std::time::Instant::now();
        self.recover(timeout)?
            .wait_empty(timeout.saturating_sub(started.elapsed()))
    }

    /// Settle this retained scope without consulting newly selected node
    /// policy. Policy replacement cannot transfer cleanup to another resource.
    pub fn terminate_and_wait(&self, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.control_timeout);
        if self.host_lifetime_ended()? {
            return Ok(());
        }
        let started = std::time::Instant::now();
        self.recover(timeout)?
            .terminate_and_wait(timeout.saturating_sub(started.elapsed()))
    }

    fn host_lifetime_ended(&self) -> Result<bool, String> {
        self.validate()?;
        match &self.backend {
            #[cfg(target_os = "linux")]
            BackendRecovery::LinuxCgroupV2 { boot_id, cgroup_enclosure, parent, directory: _, name: _ } => {
                scope_generation_ended(boot_id, *cgroup_enclosure, &self.configuration, parent)
            }
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope lifetime observation is unavailable on this OS".to_owned()),
        }
    }
}

fn validate_control_timeout(timeout: Duration) -> Result<(), String> {
    if timeout.is_zero() || std::time::Instant::now().checked_add(timeout).is_none() {
        return Err("process scope requires a positive representable control budget".to_owned());
    }
    Ok(())
}

pub(super) fn validate_allocation_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 160
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        || matches!(name, "." | "..")
    {
        return Err("process scope allocation is not a bounded direct child".to_owned());
    }
    Ok(())
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

impl ProcessScope {
    /// Component preflight only. This does not authenticate an upper admission.
    pub(crate) fn require_selected_launch_right(&self) -> Result<(), String> {
        if self.launch_available {
            Ok(())
        } else {
            Err("selected scope launch right already consumed".into())
        }
    }

    pub(crate) fn consume_selected_launch_right(&mut self) -> Result<(), String> {
        self.require_selected_launch_right()?;
        self.launch_available = false;
        Ok(())
    }

    fn selected_control_bound(
        &self,
        original: crate::time::MonotonicDeadline,
    ) -> Result<crate::time::MonotonicDeadline, String> {
        if original.has_elapsed() || self.recovery.control_timeout.is_zero() {
            return Err("selected scope control bound unavailable".into());
        }
        Ok(original.min(crate::time::MonotonicDeadline::after(
            self.recovery.control_timeout,
        )))
    }

    pub(crate) fn require_held_member_until(
        &self,
        pid: u32,
        original: crate::time::MonotonicDeadline,
    ) -> Result<(), String> {
        let bound = self.selected_control_bound(original)?;
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.require_member_until(pid, bound),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(scope) => scope.require_member_until(pid, bound),
            #[cfg(not(target_os = "linux"))]
            _ => Err("selected scope membership unavailable".into()),
        }
    }

    pub(crate) fn request_selected_termination_until(
        &self,
        original: crate::time::MonotonicDeadline,
    ) -> Result<(), String> {
        let bound = self.selected_control_bound(original)?;
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.request_termination_until(bound),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(scope) => scope.request_termination_until(bound),
            #[cfg(not(target_os = "linux"))]
            _ => Err("selected scope termination unavailable".into()),
        }
    }

    /// Install and read back scope-wide limits while this exact reservation is
    /// still empty. A failure grants no weaker launch path; the owner must
    /// retire the unlaunched reservation through its durable journal.
    pub fn into_resource_limited(
        mut self,
        limits: ProcessScopeResourceLimits,
    ) -> Result<ResourceLimitedProcessScope, (ProcessScopeRecovery, String)> {
        if let Err(error) = limits.validate() {
            self.launch_available = false;
            return Err((self.recovery.clone(), error));
        }
        if !self.launch_available {
            return Err((
                self.recovery.clone(),
                "resource limits require an unused process scope".to_owned(),
            ));
        }
        let result = match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.install_resource_limits(limits),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(_) => Err("selected resource-limit adapter not issued".into()),
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope resource limits are unavailable on this OS".to_owned()),
        };
        match result {
            Ok(()) => Ok(ResourceLimitedProcessScope(self)),
            Err(error) => {
                // A partial kernel write or failed readback cannot return an
                // unrestricted launch handle. Journal-owned recovery still
                // permits exact cleanup of this unused scope.
                self.launch_available = false;
                Err((self.recovery.clone(), error))
            }
        }
    }

    pub(crate) fn occupancy_watchdog_kill_descriptor(&self) -> Result<std::fs::File, String> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.clone_kill_descriptor(),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(_) => Err("selected watchdog adapter not issued".into()),
            #[cfg(not(target_os = "linux"))]
            _ => Err("process-scope lifetime enforcement is unavailable".to_owned()),
        }
    }

    pub(crate) fn require_held_member(&self, pid: u32, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.recovery.control_timeout);
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.require_member(pid, timeout),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(_) => Err("selected membership requires original endpoint".into()),
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope membership is unavailable on this OS".to_owned()),
        }
    }

    /// Retire a reservation which has never authorized process contact. This
    /// consumes its launch right; recovered/control-only scopes cannot invoke
    /// this shortcut. Contacted scopes require the owner's durable settlement.
    pub fn retire_unlaunched(mut self, timeout: Duration) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if matches!(&self.backend, ScopeBackend::LinuxCgroupV2Selected(_)) {
            return Err("selected frame cannot use consuming unlaunched retirement".into());
        }
        if !self.launch_available {
            return Err("process scope has already authorized contact".to_owned());
        }
        self.launch_available = false;
        ProcessScopeProvider::open(&self.recovery.configuration)?.retire(&self.recovery, timeout)
    }

    pub fn is_empty(&self) -> Result<bool, String> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.is_empty(),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(scope) => scope.is_empty(),
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope observation is unavailable on this OS".to_owned()),
        }
    }
    /// Exercise placement and kill in a disposable reservation, and the
    /// kernel barrier too when `barrier` asks for it, without executing a
    /// workload or depending on a host executable. This consumes the
    /// reservation's launch right even on failure. The caller retains its
    /// recovery coordinate and must settle/retire the reservation. A probe
    /// does not authorize a later workload or prove its confinement.
    pub fn probe_lifecycle(&mut self, timeout: Duration, barrier: bool) -> Result<ProbeBarrier, String> {
        let timeout = timeout.min(self.recovery.control_timeout);
        if !self.launch_available {
            return Err("process scope probe requires an unused reservation".to_owned());
        }
        self.launch_available = false;
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.probe_lifecycle(timeout, barrier),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(_) => Err("selected allocation grants no probe lifecycle".into()),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = (timeout, barrier);
                Err("process scope probing is unavailable on this OS".to_owned())
            }
        }
    }

    /// Share control with the held-launch owner, never the one-shot right to
    /// launch. This stays private to Lillux so failure cleanup retains the same
    /// exact resource even when the spawn worker consumes its launch handle.
    pub(crate) fn control_authority(&self) -> Self {
        Self {
            recovery: self.recovery.clone(),
            launch_available: false,
            backend: match &self.backend {
                #[cfg(target_os = "linux")]
                ScopeBackend::LinuxCgroupV2(scope) => {
                    ScopeBackend::LinuxCgroupV2(std::sync::Arc::clone(scope))
                }
                #[cfg(target_os = "linux")]
                ScopeBackend::LinuxCgroupV2Selected(scope) => {
                    ScopeBackend::LinuxCgroupV2Selected(std::sync::Arc::clone(scope))
                }
                #[cfg(not(target_os = "linux"))]
                _ => unreachable!("no process scope is implemented on this OS"),
            },
        }
    }

    /// Enter this exact scope through the existing held-launch lifecycle. No
    /// target code may run before the caller records scope recovery evidence
    /// together with its ordinary process attachment and releases that owner.
    pub fn spawn_awaiting_attachment(
        self,
        request: crate::SubprocessRequest,
    ) -> Result<crate::ProcessAwaitingAttachment, ProcessScopeLaunchError> {
        let recovery = self.recovery.clone();
        if !self.launch_available {
            return Err(ProcessScopeLaunchError {
                recovery,
                result: crate::exec::spawn_failure(
                    std::time::Instant::now(),
                    "recovered process scope grants control, not a new launch",
                ),
            });
        }
        #[cfg(target_os = "linux")]
        {
            crate::exec::lib_spawn_awaiting_attachment_in_scope(request, Some(self))
                .map_err(|result| ProcessScopeLaunchError { recovery, result })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (self, request);
            Err(ProcessScopeLaunchError {
                recovery,
                result: crate::exec::spawn_failure(
                    std::time::Instant::now(),
                    "held process scope launch is unavailable on this OS",
                ),
            })
        }
    }
    pub fn recovery(&self) -> &ProcessScopeRecovery {
        &self.recovery
    }

    // Wiring stays inside Lillux's existing subprocess lifecycle. Do not make
    // RyeOS configure raw Command hooks or inspect the backend variant.
    pub(crate) fn configure_command(
        &self,
        command: &mut std::process::Command,
    ) -> Result<(), String> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.configure_command(command),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(_) => Err("selected command adapter not issued".into()),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = command;
                Err("process scopes are unavailable on this OS".to_owned())
            }
        }
    }

    pub fn quiesce(&self, timeout: Duration) -> Result<QuiescedProcessScope, String> {
        let timeout = timeout.min(self.recovery.control_timeout);
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => Ok(QuiescedProcessScope {
                control_timeout: self.recovery.control_timeout,
                backend: QuiescedBackend::LinuxCgroupV2(scope.freeze(timeout)?),
            }),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(_) => Err("selected allocation grants no generic freeze".into()),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
                Err("process scope quiescence is unavailable on this OS".to_owned())
            }
        }
    }

    pub fn terminate_and_wait(&self, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.recovery.control_timeout);
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.terminate_and_wait(timeout),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(_) => Err("selected cleanup requires original endpoint".into()),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
                Err("process scope termination is unavailable on this OS".to_owned())
            }
        }
    }

    /// Recover the barrier named by an existing durable operation, only after
    /// the caller has excluded its previous controller with that operation's
    /// recovery fence. Scope identity alone does not grant this permission.
    /// Missing/incomplete kernel evidence refuses without resuming the scope
    /// or silently creating a new barrier.
    pub fn recover_quiesced(&self, timeout: Duration) -> Result<QuiescedProcessScope, String> {
        let timeout = timeout.min(self.recovery.control_timeout);
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => Ok(QuiescedProcessScope {
                control_timeout: self.recovery.control_timeout,
                backend: QuiescedBackend::LinuxCgroupV2(scope.recover_freeze(timeout)?),
            }),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(_) => Err("selected allocation grants no recovered barrier".into()),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
                Err("process scope barrier recovery is unavailable on this OS".to_owned())
            }
        }
    }

    pub fn wait_empty(&self, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.recovery.control_timeout);
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.wait_empty(timeout),
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2Selected(_) => Err("selected scope permits retained observation only".into()),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
                Err("process scope observation is unavailable on this OS".to_owned())
            }
        }
    }
}

impl ResourceLimitedProcessScope {
    pub fn recovery(&self) -> &ProcessScopeRecovery {
        self.0.recovery()
    }

    /// Hand the already-limited exact scope to an isolation adapter. This
    /// does not create a fresh launch right or remove kernel limits; callers
    /// must still use the held attachment/release protocol.
    pub fn into_process_scope(self) -> ProcessScope {
        self.0
    }

    pub fn spawn_awaiting_attachment(
        self,
        request: crate::SubprocessRequest,
    ) -> Result<crate::ProcessAwaitingAttachment, ProcessScopeLaunchError> {
        self.0.spawn_awaiting_attachment(request)
    }

    pub fn retire_unlaunched(self, timeout: Duration) -> Result<(), String> {
        self.0.retire_unlaunched(timeout)
    }
}

impl QuiescedProcessScope {
    /// Release a journaled workspace barrier, terminating the same scope if
    /// resume cannot be proved. An error always leaves durable settlement to
    /// the caller; it is never permission to discard the recovery coordinate.
    pub fn resume_or_terminate(self, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.control_timeout);
        match self.backend {
            #[cfg(target_os = "linux")]
            QuiescedBackend::LinuxCgroupV2(guard) => guard.resume_or_terminate(timeout),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
                Err("process scope resume is unavailable on this OS".to_owned())
            }
        }
    }

    pub fn resume(self, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.control_timeout);
        match self.backend {
            #[cfg(target_os = "linux")]
            QuiescedBackend::LinuxCgroupV2(guard) => guard.resume(timeout),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
                Err("process scope resume is unavailable on this OS".to_owned())
            }
        }
    }

    pub fn terminate(self, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.control_timeout);
        match self.backend {
            #[cfg(target_os = "linux")]
            QuiescedBackend::LinuxCgroupV2(guard) => guard.terminate(timeout),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
                Err("process scope termination is unavailable on this OS".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_qualifies_quiescence_only_when_its_barrier_held() {
        let always: BTreeSet<ProcessScopeCapability> =
            [ProcessScopeCapability::Termination, ProcessScopeCapability::Recovery].into();
        assert_eq!(ProcessScopeCapability::qualified_by_probe(&ProbeBarrier::NotRequested), always);
        assert_eq!(
            ProcessScopeCapability::qualified_by_probe(&ProbeBarrier::Refused("freezer refused".to_owned())),
            always
        );
        assert_eq!(
            ProcessScopeCapability::qualified_by_probe(&ProbeBarrier::Held),
            ProcessScopeCapability::ALL.into()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recorded_enclosure_is_compared_only_with_a_visible_one() {
        assert_eq!(compare_enclosure(4, Some(4)).unwrap(), HostGenerationChange::Same);
        assert_eq!(compare_enclosure(4, Some(5)).unwrap(), HostGenerationChange::EnclosureChanged);
        assert!(compare_enclosure(4, None).unwrap_err().contains("cannot be decided"));
        // A boot-only recording on this boot is never taken as ended.
        let boot = current_boot_id().unwrap();
        assert_eq!(host_generation_change(&boot, None).unwrap(), HostGenerationChange::Same);
    }

    #[test]
    fn scope_resource_limits_reject_unbounded_dimensions() {
        let valid = ProcessScopeResourceLimits {
            maximum_memory_bytes: 1,
            maximum_processes: 1,
        };
        assert!(valid.validate().is_ok());
        assert!(
            ProcessScopeResourceLimits {
                maximum_memory_bytes: 0,
                ..valid
            }
            .validate()
            .is_err()
        );
        assert!(
            ProcessScopeResourceLimits {
                maximum_processes: 0,
                ..valid
            }
            .validate()
            .is_err()
        );
    }

    fn configuration() -> serde_json::Value {
        serde_json::json!({"version": SCOPE_CONFIGURATION_VERSION, "backend": {
            "implementation": "linux_cgroup_v2", "parent": "/explicit/delegation"
        }})
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires separately authorized explicit disposable cgroup delegation; component control only"]
    fn selected_no_child_retains_consumed_actual_scope_until_local_empty() {
        let root =
            std::env::var_os("LILLUX_TEST_CGROUP_PARENT").expect("explicit disposable delegation");
        let config: ProcessScopeConfiguration = serde_json::from_value(serde_json::json!({
            "version": SCOPE_CONFIGURATION_VERSION,
            "backend": {"implementation": "linux_cgroup_v2", "parent": root}
        }))
        .unwrap();
        let provider = ProcessScopeProvider::open(&config).unwrap();
        let scope = provider
            .reserve(
                &format!("selected-nochild-{:032x}", rand::random::<u128>()),
                Duration::from_secs(5),
            )
            .unwrap();
        let recovery = scope.recovery().clone();
        let mut status = crate::exec::supervised_launcher_attachment_status_pipe().unwrap();
        status
            .reader
            .require_applied_launch_receipt("test.selected-scoped/v1")
            .unwrap();
        status.reader.require_mount_preparation_receipt().unwrap();
        let mut request = Some(crate::SubprocessRequest {
            cmd: "/missing-selected-scoped-component-command".into(),
            argv0: None,
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 5.0,
            limits: None,
            inherited_fds: vec![status.writer],
            inherited_fd_mappings: Vec::new(),
            supervised_status: Some(status.reader),
        });
        let mut source_scope = Some(scope);
        // Synthetic compiler commitments select no admitted caller and yield
        // no positive native evidence; creation intentionally fails before exec.
        let mut target = Some(crate::LinuxSandboxAppliedLaunchCommitments {
            executable_sha256: [0; 32],
            argv_sha256: [0; 32],
            environment_sha256: [0; 32],
            cwd_sha256: [0; 32],
        });
        let mut mounts = Some(crate::LinuxSandboxMountPreparationCommitments {
            schema: 1,
            mount_count: 0,
            destination_access_sha256: [0; 32],
        });
        let original = crate::time::MonotonicDeadline::after(Duration::from_secs(5));
        let mut storage = crate::exec::SelectedScopedStorage::for_component_test();
        let mut ledger = storage.borrow_for_component_test();
        ledger
            .prepare_from_compiled_slots(
                &mut request,
                &mut source_scope,
                &mut target,
                &mut mounts,
                original,
                original,
            )
            .unwrap();
        assert!(
            request.is_none() && source_scope.is_none() && target.is_none() && mounts.is_none()
        );
        assert_eq!(
            ledger.create(),
            crate::exec::SelectedScopedObservation::RefusedWithoutChild
        );
        assert!(
            ledger
                .require_recorded_local_scope_settlement(&recovery)
                .is_err()
        );
        assert_eq!(
            ledger.observe_cleanup(),
            crate::exec::SelectedScopedObservation::LocallySettled
        );
        ledger
            .require_recorded_local_scope_settlement(&recovery)
            .unwrap();
        assert_eq!(
            ledger.create(),
            crate::exec::SelectedScopedObservation::Refused
        );
        // Fixture directory retirement only after actual local proof; not App
        // reservation clearing, refunded funding or enclosing root qualification.
        drop(ledger);
        drop(storage);
        provider.retire(&recovery, Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn scope_configuration_is_closed_and_validated_without_host_contact() {
        let original = configuration();
        let parsed: ProcessScopeConfiguration = serde_json::from_value(original.clone()).unwrap();
        parsed.validate().unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), original);
        for path in ["relative", "/", "/explicit/../other"] {
            let mut value = configuration();
            value["backend"]["parent"] = path.into();
            assert!(
                serde_json::from_value::<ProcessScopeConfiguration>(value)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut old = configuration();
        old["version"] = 0.into();
        assert!(
            serde_json::from_value::<ProcessScopeConfiguration>(old)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut unknown = configuration();
        unknown["backend"]["implicit_delegation"] = true.into();
        assert!(serde_json::from_value::<ProcessScopeConfiguration>(unknown).is_err());
        let mut ambiguous = configuration();
        ambiguous["backend"]["identity"] = serde_json::json!({"containing_device":1,"inode":2});
        assert!(
            serde_json::from_value::<ProcessScopeConfiguration>(ambiguous).is_err(),
            "policy must not pretend that a reboot-ephemeral inode is a stable host selection"
        );
    }

    #[test]
    fn controller_bootstrap_rejects_ambient_or_privileged_requests_before_host_mutation() {
        let configuration: ProcessScopeConfiguration =
            serde_json::from_value(configuration()).unwrap();
        let cwd = crate::PinnedDirectory::open(std::path::Path::new("/"))
            .unwrap()
            .unwrap();
        let image_path = std::env::current_exe().unwrap();
        let image_parent = crate::PinnedDirectory::open(image_path.parent().unwrap())
            .unwrap()
            .unwrap();
        let image = image_parent
            .open_pinned_regular(image_path.file_name().unwrap(), false)
            .unwrap()
            .unwrap();
        assert!(
            configuration
                .exec_controller(
                    &ControllerAccount::unix(1000, 1000),
                    ProcessScopeResourceLimits { maximum_memory_bytes: 268435456, maximum_processes: 64 },
                    &image,
                    &[],
                    &cwd,
                    &[
                        ("X".to_owned(), "a".to_owned()),
                        ("X".to_owned(), "b".to_owned())
                    ]
                )
                .unwrap_err()
                .contains("unique")
        );
        #[cfg(target_os = "linux")]
        assert!(
            configuration
                .exec_controller(&ControllerAccount::unix(0, 0), ProcessScopeResourceLimits { maximum_memory_bytes: 268435456, maximum_processes: 64 }, &image, &[], &cwd, &[])
                .unwrap_err()
                .contains("non-root account")
        );
    }

    #[cfg(unix)]
    #[test]
    fn controller_account_observation_does_not_accept_privilege_or_another_account() {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let selected = ControllerAccount::unix(uid, gid);
        if uid == 0 || gid == 0 {
            assert!(selected.require_current_process().is_err());
        }
        assert!(
            ControllerAccount::unix(0, 0)
                .require_current_process()
                .is_err()
        );
        let other_uid = if uid == 1 { 2 } else { 1 };
        assert!(
            ControllerAccount::unix(other_uid, gid.max(1))
                .require_current_process()
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires administrator credential transition in a disposable child process"]
    fn controller_transition_clears_all_credentials_and_sets_no_new_privileges() {
        const CHILD: &str = "LILLUX_TEST_CONTROLLER_CREDENTIAL_TRANSITION";
        let selected = ControllerAccount::unix(65_534, 65_534);
        if std::env::var_os(CHILD).is_some() {
            selected.require_current_process().unwrap();
            return;
        }
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "process_control::scope::tests::controller_transition_clears_all_credentials_and_sets_no_new_privileges",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, "1");
        selected.configure_command(&mut command).unwrap();
        assert!(command.status().unwrap().success());
    }

    #[test]
    fn delegated_native_control_directory_is_traversable_but_not_mutable_or_listable() {
        assert_eq!(DELEGATED_CONTROL_DIRECTORY_MODE & 0o700, 0o700);
        assert_eq!(DELEGATED_CONTROL_DIRECTORY_MODE & 0o070, 0o010);
        assert_eq!(DELEGATED_CONTROL_DIRECTORY_MODE & 0o007, 0);
    }

    #[test]
    fn delegated_host_state_directory_is_readable_but_not_mutable() {
        assert_eq!(DELEGATED_READONLY_DIRECTORY_MODE & 0o700, 0o700);
        assert_eq!(DELEGATED_READONLY_DIRECTORY_MODE & 0o070, 0o050);
        assert_eq!(DELEGATED_READONLY_DIRECTORY_MODE & 0o007, 0);
    }

    #[test]
    fn scope_recovery_round_trips_without_exposing_control_or_reinterpreting_fields() {
        let value = serde_json::json!({"version": SCOPE_RECOVERY_VERSION, "configuration": configuration(),
            "control_timeout": {"secs": 1, "nanos": 0}, "backend": {
            "implementation": "linux_cgroup_v2", "boot_id": "00000000-0000-4000-8000-000000000000",
            "cgroup_enclosure": 7, "parent": {"containing_device": 1, "inode": 2},
            "directory": {"containing_device": 1, "inode": 3}, "name": "existing-allocation"
        }});
        let recovery: ProcessScopeRecovery = serde_json::from_value(value.clone()).unwrap();
        recovery.validate().unwrap();
        assert_eq!(serde_json::to_value(&recovery).unwrap(), value);
        let mut planned = value.clone();
        planned["version"] = SCOPE_ALLOCATION_VERSION.into();
        planned["backend"]
            .as_object_mut()
            .unwrap()
            .remove("directory");
        let allocation: ProcessScopeAllocation = serde_json::from_value(planned.clone()).unwrap();
        allocation.validate().unwrap();
        assert!(recovery.matches_allocation(&allocation));
        for (field, replacement) in [
            ("name", serde_json::json!("different-slot")),
            ("boot_id", serde_json::json!("00000000-0000-4000-8000-000000000001")),
            ("cgroup_enclosure", serde_json::json!(8)),
        ] {
            let mut changed = planned.clone();
            changed["backend"][field] = replacement;
            assert!(!recovery.matches_allocation(&serde_json::from_value(changed).unwrap()));
        }
        let mut changed = planned.clone();
        changed["control_timeout"]["secs"] = 2.into();
        assert!(!recovery.matches_allocation(&serde_json::from_value(changed).unwrap()));
        planned.as_object_mut().unwrap().remove("control_timeout");
        assert!(serde_json::from_value::<ProcessScopeAllocation>(planned).is_err());
        for (field, replacement) in [
            ("name", "../other"),
            ("boot_id", "not-a-boot"),
            ("boot_id", "00000000-0000-4000-8000-00000000000G"),
        ] {
            let mut invalid = value.clone();
            invalid["backend"][field] = replacement.into();
            assert!(
                serde_json::from_value::<ProcessScopeRecovery>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut changed_parent = value.clone();
        changed_parent["backend"]["parent"]["inode"] = 100.into();
        let changed_parent: ProcessScopeRecovery = serde_json::from_value(changed_parent).unwrap();
        let mut original_plan = value.clone();
        original_plan["version"] = SCOPE_ALLOCATION_VERSION.into();
        original_plan["backend"]
            .as_object_mut()
            .unwrap()
            .remove("directory");
        assert!(
            !changed_parent.matches_allocation(&serde_json::from_value(original_plan).unwrap())
        );
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove("control_timeout");
        assert!(serde_json::from_value::<ProcessScopeRecovery>(missing).is_err());
        let mut zero = value.clone();
        zero["control_timeout"]["secs"] = 0.into();
        assert!(
            serde_json::from_value::<ProcessScopeRecovery>(zero)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut predecessor = value;
        predecessor["version"] = (SCOPE_RECOVERY_VERSION - 1).into();
        assert!(
            serde_json::from_value::<ProcessScopeRecovery>(predecessor)
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires explicitly delegated disposable host scope; no node or installed-state mutation"]
    fn scope_native_held_launch_cleanup_and_failed_launch_retain_exact_evidence() {
        if super::super::cgroup::tests::run_in_disposable_delegation(
            "process_control::scope::tests::scope_native_held_launch_cleanup_and_failed_launch_retain_exact_evidence",
        ) {
            return;
        }
        let root = std::env::var_os("LILLUX_TEST_CGROUP_PARENT")
            .expect("set explicit disposable delegation");
        let root = std::path::Path::new(&root);
        let configuration: ProcessScopeConfiguration = serde_json::from_value(serde_json::json!({
            "version": SCOPE_CONFIGURATION_VERSION,
            "backend": {"implementation": "linux_cgroup_v2", "parent": root}
        }))
        .unwrap();
        let provider = ProcessScopeProvider::open(&configuration).unwrap();
        // Crash either before mkdir or after mkdir/before journal binding.
        // Neither phase may be retried into process launch during recovery.
        let allocation = provider
            .plan_allocation(
                &format!("unbound-{:032x}", rand::random::<u128>()),
                Duration::from_secs(5),
            )
            .unwrap();
        allocation.discard_unlaunched().unwrap();
        let unbound = provider.allocate(&allocation).unwrap();
        let unbound_recovery = unbound.recovery().clone();
        assert!(unbound_recovery.matches_allocation(&allocation));
        drop(unbound); // simulated creator loss: no process has contacted it
        allocation.discard_unlaunched().unwrap();
        allocation.discard_unlaunched().unwrap();
        assert!(
            provider
                .recover(&unbound_recovery, Duration::from_secs(5))
                .is_err()
        );
        let mut probe = provider
            .reserve(
                &format!("probe-{:032x}", rand::random::<u128>()),
                Duration::from_secs(5),
            )
            .unwrap();
        let probe_recovery = probe.recovery().clone();
        assert_eq!(probe.probe_lifecycle(Duration::from_secs(5), true).unwrap(), ProbeBarrier::Held);
        assert!(
            probe.probe_lifecycle(Duration::from_secs(5), true).is_err(),
            "probe launch authority must be one-shot"
        );
        provider
            .recover(&probe_recovery, Duration::from_secs(5))
            .unwrap()
            .wait_empty(Duration::ZERO)
            .unwrap();
        provider
            .retire(&probe_recovery, Duration::from_secs(5))
            .unwrap();
        // Qualification exercises the barrier only for a floor that wants
        // quiescence; without it, termination and recovery still qualify.
        let termination_only: BTreeSet<ProcessScopeCapability> = [ProcessScopeCapability::Termination].into();
        assert_eq!(
            provider.qualify(Duration::from_secs(5), &termination_only).unwrap(),
            [ProcessScopeCapability::Termination, ProcessScopeCapability::Recovery].into()
        );
        assert_eq!(
            provider.qualify(Duration::from_secs(5), &ProcessScopeCapability::ALL.into()).unwrap(),
            ProcessScopeCapability::ALL.into()
        );
        let fixture = tempfile::tempdir().unwrap();
        let marker = fixture.path().join("ran");
        // Ordinary host programs construct the test workload only. They are
        // not production runtime dependencies or implied executable search.
        let request = || crate::SubprocessRequest {
            cmd: "/bin/sh".to_owned(),
            argv0: None,
            args: vec![
                "-c".to_owned(),
                "setsid /bin/sleep 60 & printf ran > \"$1\"".to_owned(),
                "fixture".to_owned(),
                marker.to_string_lossy().into_owned(),
            ],
            cwd: None,
            envs: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
            stdin_data: None,
            timeout: 5.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        let scope_allocation = provider
            .plan_allocation(
                &format!("scope-{:032x}", rand::random::<u128>()),
                Duration::from_secs(5),
            )
            .unwrap();
        let scope = provider.allocate(&scope_allocation).unwrap();
        let recovery = scope.recovery().clone();
        let pending = scope.spawn_awaiting_attachment(request()).unwrap();
        assert_eq!(pending.scope_recovery(), Some(&recovery));
        assert!(!marker.exists(), "workload ran before attachment");
        assert!(
            scope_allocation.discard_unlaunched().is_err(),
            "unbound cleanup cannot remove or signal a populated scope"
        );
        let unrelated = provider
            .reserve(
                &format!("unlaunched-{:032x}", rand::random::<u128>()),
                Duration::from_secs(5),
            )
            .unwrap();
        let unrelated_recovery = unrelated.recovery().clone();
        assert!(
            unrelated
                .require_held_member(pending.pid(), Duration::from_secs(5))
                .is_err()
        );
        unrelated.retire_unlaunched(Duration::from_secs(5)).unwrap();
        assert!(
            provider
                .recover(&unrelated_recovery, Duration::from_secs(5))
                .is_err()
        );
        let recovered = provider.recover(&recovery, Duration::from_secs(5)).unwrap();
        let refused = recovered
            .spawn_awaiting_attachment(request())
            .err()
            .unwrap();
        assert_eq!(refused.recovery, recovery);
        assert!(refused.result.stderr.contains("not a new launch"));
        let running = pending.release_after_attachment().unwrap();
        assert_eq!(running.scope_recovery(), Some(&recovery));
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(&marker).unwrap(), b"ran");
        let running = match running.wait_for_natural_exit(Duration::from_millis(100)) {
            Ok(_) => panic!("live scoped descendant was misreported as natural exit"),
            Err(running) => running,
        };
        assert!(
            !recovery.is_empty(Duration::from_secs(1)).unwrap(),
            "natural wait must retain the live descendant for explicit cleanup"
        );
        let completed = running.wait();
        assert!(completed.success, "{}", completed.stderr);
        assert_eq!(std::fs::read(&marker).unwrap(), b"ran");
        provider
            .recover(&recovery, Duration::from_secs(5))
            .unwrap()
            .wait_empty(Duration::ZERO)
            .unwrap();
        provider.retire(&recovery, Duration::from_secs(5)).unwrap();
        assert!(provider.recover(&recovery, Duration::from_secs(5)).is_err());

        // A distinct scope with no surviving descendants may complete
        // naturally. The wrapper remains unreaped until kernel emptiness is
        // observed, then the existing owner performs ordinary settlement.
        let scope = provider
            .reserve(
                &format!("natural-{:032x}", rand::random::<u128>()),
                Duration::from_secs(5),
            )
            .unwrap();
        let natural_recovery = scope.recovery().clone();
        let mut exits = request();
        exits.args = vec!["-c".to_owned(), "exit 0".to_owned()];
        let running = scope
            .spawn_awaiting_attachment(exits)
            .unwrap()
            .release_after_attachment()
            .unwrap();
        let natural = match running.wait_for_natural_exit(Duration::from_secs(5)) {
            Ok(result) => result,
            Err(running) => {
                running.abort();
                panic!("empty scope did not complete naturally");
            }
        };
        assert!(natural.success, "{}", natural.stderr);
        natural_recovery.wait_empty(Duration::ZERO).unwrap();
        provider
            .retire(&natural_recovery, Duration::from_secs(5))
            .unwrap();

        // Only replay of an already-authorized disposal treats missing as
        // removed. It must not change passive liveness/recovery semantics.
        recovery.retire_after_settlement().unwrap();
        recovery.retire_after_settlement().unwrap();
        assert!(recovery.is_empty(Duration::from_secs(5)).is_err());
        let BackendRecovery::LinuxCgroupV2 { name, .. } = &recovery.backend;
        let replacement = provider.reserve(name, Duration::from_secs(5)).unwrap();
        assert!(
            recovery.retire_after_settlement().is_err(),
            "retirement replay must not remove a new resource incarnation"
        );
        replacement
            .retire_unlaunched(Duration::from_secs(5))
            .unwrap();

        // Inject premature kernel-resource removal AFTER the exact target
        // exits but BEFORE its lifecycle owner settles. This is forbidden in
        // production; the fixture proves that natural target exit cannot
        // mask missing scope proof at the readiness cleanup boundary.
        let scope = provider
            .reserve(
                &format!("lost-control-{:032x}", rand::random::<u128>()),
                Duration::from_secs(5),
            )
            .unwrap();
        let lost_recovery = scope.recovery().clone();
        let mut exits = request();
        exits.args = vec!["-c".to_owned(), "exit 0".to_owned()];
        let pending = scope.spawn_awaiting_attachment(exits).unwrap();
        let pid = pending.pid();
        let pidfd = pending.pidfd().try_clone_to_owned().unwrap();
        let running = pending.release_after_attachment().unwrap();
        use std::os::fd::AsRawFd;
        super::super::linux::wait_pidfd_exit(
            pidfd.as_raw_fd(),
            std::time::Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        provider
            .retire(&lost_recovery, Duration::from_secs(5))
            .unwrap();
        let running = match running.wait_for_natural_exit(Duration::from_secs(1)) {
            Ok(_) => panic!("natural exit hid unavailable scope cleanup authority"),
            Err(running) => running,
        };
        assert!(running.abort_and_reap_checked().is_err());
        // The injected loss deliberately prevents ordinary cleanup testimony.
        // Only this fixture, after its pinned exit observation and consumed
        // process handle, reaps the exact child it created. No live workload
        // or node process is found or signaled by a numeric-PID scan.
        assert_eq!(
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0) },
            pid as libc::pid_t
        );

        let scope = provider
            .reserve(
                &format!("scope-{:032x}", rand::random::<u128>()),
                Duration::from_secs(5),
            )
            .unwrap();
        let recovery = scope.recovery().clone();
        let mut missing = request();
        missing.cmd = "/no-such-test-program".to_owned();
        // A direct held launch can reach attachment before exec reports ENOENT.
        // Its release failure must still settle the scope and preserve the
        // evidence retained by the ordinary attachment owner.
        match scope.spawn_awaiting_attachment(missing) {
            Ok(pending) => {
                assert_eq!(pending.scope_recovery(), Some(&recovery));
                assert!(pending.release_after_attachment().is_err());
            }
            Err(error) => assert_eq!(error.recovery, recovery),
        }
        provider
            .recover(&recovery, Duration::from_secs(5))
            .unwrap()
            .wait_empty(Duration::ZERO)
            .unwrap();
        let mut wrong = recovery.clone();
        let BackendConfiguration::LinuxCgroupV2 { parent, .. } = &mut wrong.configuration.backend;
        *parent = "/another/delegation".into();
        assert!(provider.recover(&wrong, Duration::from_secs(5)).is_err());
        let BackendRecovery::LinuxCgroupV2 { boot_id, .. } = &mut wrong.backend;
        *boot_id = "00000000-0000-4000-8000-000000000000".to_owned();
        wrong.configuration = recovery.configuration.clone();
        assert!(provider.recover(&wrong, Duration::from_secs(5)).is_err());
        // Control refuses an old boot, but that ended kernel lifetime proves
        // no old writer survived. Neither operation touches a replacement.
        wrong.wait_empty(Duration::from_secs(5)).unwrap();
        recovery.wait_empty(Duration::from_secs(5)).unwrap();
        let reconstructed: ProcessScopeRecovery =
            serde_json::from_value(serde_json::to_value(&recovery).unwrap()).unwrap();
        reconstructed
            .recover(Duration::from_secs(5))
            .unwrap()
            .wait_empty(Duration::ZERO)
            .unwrap();
        provider.retire(&recovery, Duration::from_secs(5)).unwrap();
    }
}
#[cfg(target_os = "linux")]
#[test]
fn host_lifetime_witness_is_strict_and_does_not_claim_same_boot_cleanup() {
    let current = ProcessHostLifetime::capture_current().unwrap();
    let process = super::ExactProcessIdentity {
        boot_id: super::linux::read_boot_id().unwrap(),
        target_pid: 101,
        target_start_time_ticks: 1,
        group_leader_pid: 101,
        group_leader_start_time_ticks: 1,
    };
    assert!(current.contains_process(&process).unwrap());
    let mut stale = process.clone();
    stale.boot_id = "00000000-0000-4000-8000-000000000000".to_owned();
    assert!(!current.contains_process(&stale).unwrap());
    stale.target_start_time_ticks = 0;
    assert!(current.contains_process(&stale).is_err());
    assert!(!current.has_ended().unwrap());
    let value = serde_json::to_value(&current).unwrap();
    let roundtrip: ProcessHostLifetime = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(roundtrip, current);
    let mut wrong = value.clone();
    wrong["version"] = 0.into();
    assert!(
        serde_json::from_value::<ProcessHostLifetime>(wrong)
            .unwrap()
            .has_ended()
            .is_err()
    );
    let mut wrong = value;
    wrong["proof"] = "daemon_exited".into();
    assert!(serde_json::from_value::<ProcessHostLifetime>(wrong).is_err());
    let ended = ProcessHostLifetime {
        version: HOST_LIFETIME_VERSION,
        backend: HostLifetimeBackend::LinuxBoot {
            boot_id: "00000000-0000-4000-8000-000000000000".to_owned(),
            cgroup_enclosure: None,
        },
    };
    assert_ne!(ended, current);
    assert!(ended.has_ended().unwrap());
    // Same boot, different enclosure: a restarted container is a new
    // generation even though the kernel boot did not change.
    let HostLifetimeBackend::LinuxBoot { boot_id, cgroup_enclosure } = &current.backend;
    if let Some(inode) = cgroup_enclosure {
        let restarted = ProcessHostLifetime {
            version: HOST_LIFETIME_VERSION,
            backend: HostLifetimeBackend::LinuxBoot {
                boot_id: boot_id.clone(),
                cgroup_enclosure: Some(inode.wrapping_add(1)),
            },
        };
        assert!(restarted.has_ended().unwrap());
        let unobserved = ProcessHostLifetime {
            version: HOST_LIFETIME_VERSION,
            backend: HostLifetimeBackend::LinuxBoot { boot_id: boot_id.clone(), cgroup_enclosure: None },
        };
        assert!(unobserved.has_ended().is_err());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn readonly_delivery_grant_refuses_writable_modes_without_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = crate::PinnedDirectory::open(temporary.path())
        .unwrap()
        .unwrap();
    let file = directory
        .atomic_create_pinned_regular(
            std::ffi::OsStr::new("input"),
            b"exact",
            0o400,
            crate::secure_fs::TEST_TEMP_PURPOSE,
        )
        .unwrap()
        .unwrap();
    let account = ControllerAccount::unix(65534, 65534);
    for mode in [0o600, 0o700, 0o444, 0o755, 0o4500] {
        assert!(account.grant_private_readonly_file(&file, mode).is_err());
        assert_eq!(file.permission_mode().unwrap(), 0o400);
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "installed administrator qualification: requires root; never grants live project files"]
fn readonly_delivery_grant_preserves_read_and_executable_modes() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let temporary = tempfile::tempdir().unwrap();
    let directory = crate::PinnedDirectory::open(temporary.path())
        .unwrap()
        .unwrap();
    let account = ControllerAccount::unix(65534, 65534);
    for (name, mode) in [("record", 0o400), ("verifier", 0o500)] {
        let file = directory
            .atomic_create_pinned_regular(
                std::ffi::OsStr::new(name),
                b"exact",
                mode,
                crate::secure_fs::TEST_TEMP_PURPOSE,
            )
            .unwrap()
            .unwrap();
        account.grant_private_readonly_file(&file, mode).unwrap();
        file.require_owner(65534).unwrap();
        assert_eq!(file.permission_mode().unwrap(), mode);
        assert_eq!(
            file.read_stable_bounded(&file.observation().unwrap(), 5)
                .unwrap(),
            b"exact"
        );
    }
}
