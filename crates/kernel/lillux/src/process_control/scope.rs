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
const SCOPE_RECOVERY_VERSION: u32 = 4;
const SCOPE_ALLOCATION_VERSION: u32 = 2;
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
            if unsafe { libc::getuid() } != uid
                || unsafe { libc::geteuid() } != uid
                || unsafe { libc::getgid() } != gid
                || unsafe { libc::getegid() } != gid
            {
                anyhow::bail!("controller process does not run as its selected non-root account");
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
/// Same-boot process absence is deliberately NOT claimed by this witness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessHostLifetime {
    version: u32,
    backend: HostLifetimeBackend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "implementation", rename_all = "snake_case", deny_unknown_fields)]
enum HostLifetimeBackend {
    LinuxBoot { boot_id: String },
}

impl ProcessHostLifetime {
    pub fn capture_current() -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            let value = Self {
                version: 1,
                backend: HostLifetimeBackend::LinuxBoot {
                    boot_id: super::linux::read_boot_id()?,
                },
            };
            value.validate()?;
            Ok(value)
        }
        #[cfg(not(target_os = "linux"))]
        Err("host lifetime capture is unavailable on this OS".to_owned())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("process host-lifetime witness contract is not current".to_owned());
        }
        match &self.backend {
            HostLifetimeBackend::LinuxBoot { boot_id } if valid_boot_id(boot_id) => Ok(()),
            _ => Err("process host-lifetime witness is invalid".to_owned()),
        }
    }

    pub fn has_ended(&self) -> Result<bool, String> {
        self.validate()?;
        match &self.backend {
            #[cfg(target_os = "linux")]
            HostLifetimeBackend::LinuxBoot { boot_id } => {
                let current = super::linux::read_boot_id()?;
                if !valid_boot_id(&current) {
                    return Err("current host lifetime is unavailable".to_owned());
                }
                Ok(current != *boot_id)
            }
            #[cfg(not(target_os = "linux"))]
            _ => Err("host lifetime observation is unavailable on this OS".to_owned()),
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

    /// Explicit host-supervisor entry, not worker execution. Replaces this
    /// administrator process with an unprivileged controller after exact
    /// placement. It installs no service and changes no application policy.
    pub fn exec_controller(
        &self,
        account: &ControllerAccount,
        executable: &crate::PinnedRegularFile,
        arguments: &[String],
        cwd: &crate::PinnedDirectory,
        environment: &[(String, String)],
    ) -> Result<std::convert::Infallible, String> {
        self.validate()?;
        account.validate()?;
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
            let bootstrap = super::cgroup::provision_controller(parent, uid, gid)?;
            command
                .args(arguments)
                .env_clear()
                .envs(environment.iter().map(|(key, value)| (key, value)));
            bootstrap.configure_command(&mut command)?;
            cwd.configure_command_cwd(&mut command)
                .map_err(|error| error.to_string())?;
            Err(format!(
                "exec unprivileged scope controller: {}",
                command.exec()
            ))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (account, arguments);
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
    /// share it. Failure never returns an advertised capability set.
    pub fn qualify(&self, timeout: Duration) -> Result<BTreeSet<ProcessScopeCapability>, String> {
        let started = std::time::Instant::now();
        let allocation = format!(
            "inspection-{}",
            crate::sha256_hex(&crate::crypto::generate_random_bytes::<32>())
        );
        let mut probe = self.reserve(&allocation, timeout)?;
        let recovery = probe.recovery().clone();
        let qualification = probe.probe_lifecycle(timeout.saturating_sub(started.elapsed()));
        let retirement = self.retire(&recovery, timeout.saturating_sub(started.elapsed()));
        match (qualification, retirement) {
            (Ok(()), Ok(())) => Ok(self.implemented_capabilities()),
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
            ProviderBackend::LinuxCgroupV2(parent) => Ok(ProcessScopeAllocation {
                version: SCOPE_ALLOCATION_VERSION,
                configuration: self.configuration.clone(),
                control_timeout: timeout,
                backend: BackendAllocation::LinuxCgroupV2 {
                    boot_id: super::linux::read_boot_id()?,
                    parent: parent.identity()?,
                    name: allocation.to_owned(),
                },
            }),
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
                    parent: owner,
                    directory,
                    name,
                },
            ) => {
                if super::linux::read_boot_id()? != *boot_id {
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
            BackendAllocation::LinuxCgroupV2 { boot_id, .. } => Ok(ProcessHostLifetime {
                version: 1,
                backend: HostLifetimeBackend::LinuxBoot {
                    boot_id: boot_id.clone(),
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
            BackendAllocation::LinuxCgroupV2 { boot_id, .. } => {
                let current = super::linux::read_boot_id()?;
                if !valid_boot_id(&current) {
                    return Err("current host lifetime is unavailable".to_owned());
                }
                Ok(current != *boot_id)
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
                    parent,
                    name,
                    ..
                },
                BackendAllocation::LinuxCgroupV2 {
                    boot_id: expected_boot,
                    parent: expected_parent,
                    name: expected_name,
                },
            ) => boot_id == expected_boot && parent == expected_parent && name == expected_name,
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
            BackendRecovery::LinuxCgroupV2 { boot_id, .. } => {
                let current = super::linux::read_boot_id()?;
                if !valid_boot_id(&current) {
                    return Err("kernel boot identity is unavailable or malformed".to_owned());
                }
                Ok(current != *boot_id)
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
    pub(crate) fn require_held_member(&self, pid: u32, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.recovery.control_timeout);
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.require_member(pid, timeout),
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope membership is unavailable on this OS".to_owned()),
        }
    }

    /// Retire a reservation which has never authorized process contact. This
    /// consumes its launch right; recovered/control-only scopes cannot invoke
    /// this shortcut. Contacted scopes require the owner's durable settlement.
    pub fn retire_unlaunched(mut self, timeout: Duration) -> Result<(), String> {
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
            #[cfg(not(target_os = "linux"))]
            _ => Err("process scope observation is unavailable on this OS".to_owned()),
        }
    }
    /// Exercise placement and the kernel barrier in a disposable reservation,
    /// without executing a workload or depending on a host executable. This
    /// consumes the reservation's launch right even on failure. The caller
    /// retains its recovery coordinate and must settle/retire the reservation.
    /// A probe does not authorize a later workload or prove its confinement.
    pub fn probe_lifecycle(&mut self, timeout: Duration) -> Result<(), String> {
        let timeout = timeout.min(self.recovery.control_timeout);
        if !self.launch_available {
            return Err("process scope probe requires an unused reservation".to_owned());
        }
        self.launch_available = false;
        match &self.backend {
            #[cfg(target_os = "linux")]
            ScopeBackend::LinuxCgroupV2(scope) => scope.probe_lifecycle(timeout),
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
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
            #[cfg(not(target_os = "linux"))]
            _ => {
                let _ = timeout;
                Err("process scope observation is unavailable on this OS".to_owned())
            }
        }
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

    fn configuration() -> serde_json::Value {
        serde_json::json!({"version": SCOPE_CONFIGURATION_VERSION, "backend": {
            "implementation": "linux_cgroup_v2", "parent": "/explicit/delegation"
        }})
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
                .exec_controller(&ControllerAccount::unix(0, 0), &image, &[], &cwd, &[])
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
        if uid != 0 && gid != 0 {
            selected.require_current_process().unwrap();
        } else {
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
            "parent": {"containing_device": 1, "inode": 2},
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
        for field in ["name", "boot_id"] {
            let mut changed = planned.clone();
            changed["backend"][field] = if field == "name" {
                "different-slot"
            } else {
                "00000000-0000-4000-8000-000000000001"
            }
            .into();
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
        probe.probe_lifecycle(Duration::from_secs(5)).unwrap();
        assert!(
            probe.probe_lifecycle(Duration::from_secs(5)).is_err(),
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
        version: 1,
        backend: HostLifetimeBackend::LinuxBoot {
            boot_id: "00000000-0000-4000-8000-000000000000".to_owned(),
        },
    };
    assert_ne!(ended, current);
    assert!(ended.has_ended().unwrap());
}
