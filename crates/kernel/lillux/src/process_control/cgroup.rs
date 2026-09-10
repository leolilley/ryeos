//! Explicitly delegated cgroup-v2 lifecycle authority.
//!
//! Higher layers select the parent and retain execution ownership. This module
//! never discovers a host delegation. Ordinary execution never changes host
//! ownership/controllers; only the explicitly privileged supervisor bootstrap
//! below delegates one administrator-selected boundary.
//! Control files are kernel interfaces: never use atomic replacement, truncate,
//! fsync, recursive filesystem removal, or ordinary state-file fallbacks here.

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{PinnedDirectory, PinnedDirectoryIdentity};

const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
const HOST_SERVICE_DELEGATIONS: &str = "lillux-host-services";
// Bounded kernel interface records, not workload output or acquisition policy.
const MAX_CONTROL_RECORD_BYTES: usize = 4096;

/// Root-only selection of the native delegation parent for one installed host
/// service. The path is deliberately owned by Lillux rather than supplied by
/// an application or node policy. The returned child is not created/chowned
/// until the scope controller launches, so provisioning leaves no runnable
/// user-owned cgroup behind.
pub(crate) fn provision_host_delegation(service_label: &str) -> Result<std::path::PathBuf, String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("host delegation provisioning requires administrator authority".to_owned());
    }
    if service_label.is_empty()
        || service_label.len() > 128
        || !service_label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("host service label is not safe for a native delegation".to_owned());
    }
    let root = PinnedDirectory::open(Path::new("/sys/fs/cgroup"))
        .map_err(display)?
        .ok_or("cgroup-v2 root is absent")?;
    let root_fd = root.try_clone_descriptor().map_err(display)?;
    require_cgroup2(&root_fd)?;
    let parent = open_or_create_host_delegation_parent(&root)?;
    parent.require_owner(0).map_err(display)?;
    let metadata = parent
        .try_clone_descriptor()
        .map_err(display)?
        .metadata()
        .map_err(display)?;
    if metadata.mode() & 0o022 != 0 {
        return Err("Lillux host delegation parent has shared write access".to_owned());
    }
    let parent_fd = parent.try_clone_descriptor().map_err(display)?;
    require_cgroup2(&parent_fd)?;
    Ok(parent.path().join(service_label))
}

/// Create or recover the one Lillux-owned cgroup namespace below the exact
/// cgroup-v2 root descriptor. cgroup is a synchronous kernel control
/// filesystem, not durable storage: `fsync(2)` is invalid there. In
/// particular, do not use `PinnedDirectory::open_or_create_child`, whose
/// durability sync is deliberately required for ordinary state directories.
///
/// This remains descriptor-rooted and no-follow. The selected name is a
/// Lillux constant, never an application or workload path.
fn open_or_create_host_delegation_parent(
    root: &PinnedDirectory,
) -> Result<PinnedDirectory, String> {
    let name = OsStr::new(HOST_SERVICE_DELEGATIONS);
    if let Some(existing) = root.open_child_directory(name).map_err(display)? {
        return Ok(existing);
    }
    let root_fd = root.try_clone_descriptor().map_err(display)?;
    let name_c = child_name(HOST_SERVICE_DELEGATIONS)?;
    if unsafe { libc::mkdirat(root_fd.as_raw_fd(), name_c.as_ptr(), 0o755) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(format!("create Lillux host delegation namespace: {error}"));
        }
    }
    root.open_child_directory(name)
        .map_err(display)?
        .ok_or_else(|| "Lillux host delegation namespace disappeared after creation".to_owned())
}

/// Read-only host-maintenance barrier. The host launch gate must exclude new
/// controller launches throughout its use. Parent ownership prevents replacing
/// the selected delegation; the kernel populated bit includes every descendant
/// (including detached writers), not merely the daemon/controller leaf.
pub(crate) fn require_controller_tree_empty(path: &Path, uid: u32) -> Result<(), String> {
    let parent_path = path.parent().ok_or("controller delegation has no parent")?;
    let parent = PinnedDirectory::open_owned_hierarchy(parent_path, 0)
        .map_err(display)?
        .ok_or("host delegation parent is absent")?;
    require_cgroup2(&parent.try_clone_descriptor().map_err(display)?)?;
    let Some(directory) = parent
        .open_child_directory(
            path.file_name()
                .ok_or("controller delegation has no name")?,
        )
        .map_err(display)?
    else {
        // A populated cgroup cannot be removed by its delegated owner. Its
        // parent namespace is administrator-owned and was verified above.
        return Ok(());
    };
    directory.require_owner(uid).map_err(display)?;
    let descriptor = directory.try_clone_descriptor().map_err(display)?;
    require_cgroup2(&descriptor)?;
    require_domain(&descriptor)?;
    let events = open_control(&descriptor, c"cgroup.events", libc::O_RDONLY)?;
    if parse_events(&read_control(&events)?)?.populated {
        return Err("controller process tree still has live members; retained recovery must settle before package replacement".to_owned());
    }
    Ok(())
}

/// Host-supervisor bootstrap only. This is not a worker capability, a setuid
/// helper or a long-lived privileged service. The caller must already be root
/// and explicitly select one child beneath an administrator-owned cgroup.
pub(crate) fn provision_controller(
    path: &Path,
    uid: u32,
    gid: u32,
) -> Result<ControllerBootstrap, String> {
    if unsafe { libc::geteuid() } != 0 || uid == 0 || gid == 0 {
        return Err(
            "controller provisioning requires an administrator and an explicit non-root account"
                .to_owned(),
        );
    }
    let enclosing_path = path.parent().ok_or("controller delegation has no parent")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("controller delegation needs an exact child name")?;
    let name_c = child_name(name)?;
    let enclosing = PinnedDirectory::open(enclosing_path)
        .map_err(display)?
        .ok_or("host cgroup parent is absent")?;
    let enclosing_fd = enclosing.try_clone_descriptor().map_err(display)?;
    require_cgroup2(&enclosing_fd)?;
    let metadata = enclosing_fd.metadata().map_err(display)?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(
            "host cgroup parent must be administrator-owned without shared write".to_owned(),
        );
    }
    if unsafe { libc::mkdirat(enclosing_fd.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0
        && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
    {
        return Err(format!(
            "create selected delegation: {}",
            io::Error::last_os_error()
        ));
    }
    let parent = enclosing
        .open_child_directory(name.as_ref())
        .map_err(display)?
        .ok_or("selected delegation disappeared")?;
    let parent_fd = parent.try_clone_descriptor().map_err(display)?;
    require_domain(&parent_fd)?;
    let metadata = parent_fd.metadata().map_err(display)?;
    if (metadata.uid() != 0 && metadata.uid() != uid) || metadata.mode() & 0o077 != 0 {
        return Err("selected delegation has foreign ownership or shared access".to_owned());
    }
    // Never grant parent freeze/kill authority. Those may affect the node
    // controller itself; only children belong to the unprivileged owner.
    for name in [c"cgroup.freeze", c"cgroup.kill"] {
        let control = open_control(&parent_fd, name, libc::O_PATH)?;
        let metadata = control.metadata().map_err(display)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err("delegation lifecycle controls must remain administrator-owned".to_owned());
        }
    }
    let parent_freeze = open_control(&parent_fd, c"cgroup.freeze", libc::O_RDONLY)?;
    if read_control(&parent_freeze)?.trim() != "0" {
        return Err("selected delegation is frozen; refusing controller launch".to_owned());
    }
    let parent_placement = open_control(&parent_fd, c"cgroup.procs", libc::O_RDWR)?;
    if !read_control(&parent_placement)?.trim().is_empty() {
        return Err("selected delegation directly contains processes; controller must occupy its separate leaf".to_owned());
    }
    if unsafe { libc::mkdirat(parent_fd.as_raw_fd(), c"controller".as_ptr(), 0o700) } != 0
        && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
    {
        return Err(format!(
            "create controller leaf: {}",
            io::Error::last_os_error()
        ));
    }
    let controller = parent
        .open_child_directory("controller".as_ref())
        .map_err(display)?
        .ok_or("controller leaf disappeared")?;
    let controller_fd = controller.try_clone_descriptor().map_err(display)?;
    let metadata = controller_fd.metadata().map_err(display)?;
    if metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        return Err("controller leaf must be administrator-owned and private".to_owned());
    }
    let events = open_control(&controller_fd, c"cgroup.events", libc::O_RDONLY)?;
    let observed = parse_events(&read_control(&events)?)?;
    if observed.populated || observed.frozen {
        return Err(
            "controller leaf is occupied or frozen; settle its existing supervisor first"
                .to_owned(),
        );
    }
    let placement = open_control(&controller_fd, c"cgroup.procs", libc::O_WRONLY)?;
    for fd in [&parent_fd, &parent_placement] {
        if unsafe { libc::fchown(fd.as_raw_fd(), uid, gid) } != 0 {
            return Err(format!(
                "delegate exact controller authority: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(ControllerBootstrap {
        parent,
        placement,
        uid,
        gid,
    })
}

pub(crate) struct ControllerBootstrap {
    parent: PinnedDirectory,
    placement: File,
    uid: u32,
    gid: u32,
}

impl ControllerBootstrap {
    pub(crate) fn configure_command(
        &self,
        command: &mut std::process::Command,
    ) -> Result<(), String> {
        let placement = self.placement.try_clone().map_err(display)?;
        let (uid, gid) = (self.uid, self.gid);
        // Retain the exact parent through preparation; no path reopening in
        // the credential-transition hook. Its control FDs remain CLOEXEC.
        let _ = self.parent.identity().map_err(display)?;
        unsafe {
            command.pre_exec(move || {
                loop {
                    let result = libc::write(placement.as_raw_fd(), b"0".as_ptr().cast(), 1);
                    if result == 1 {
                        break;
                    }
                    let error = io::Error::last_os_error();
                    if result < 0 && error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(if result < 0 {
                        error
                    } else {
                        io::Error::from_raw_os_error(libc::EIO)
                    });
                }
                Ok(())
            });
        }
        super::scope::ControllerAccount::unix(uid, gid).configure_command(command)?;
        Ok(())
    }
}

/// Exact child within an independently authorized, descriptor-pinned parent.
/// The application must also retain boot and placement identity. An inode or
/// a friendly child name alone never authorizes recovery.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CgroupIdentity {
    pub parent: PinnedDirectoryIdentity,
    pub directory: PinnedDirectoryIdentity,
    pub name: String,
}

#[derive(Debug)]
pub struct DelegatedCgroup {
    directory: PinnedDirectory,
}

impl DelegatedCgroup {
    /// Open only the exact node-selected parent. The operator/supervisor must
    /// already have delegated it to this UID; opening never grants delegation.
    pub fn open(path: &Path) -> Result<Self, String> {
        let directory = PinnedDirectory::open(path)
            .map_err(|error| format!("open cgroup delegation: {error}"))?
            .ok_or_else(|| "cgroup delegation is absent".to_owned())?;
        let fd = directory.try_clone_descriptor().map_err(display)?;
        require_cgroup2(&fd)?;
        let metadata = fd.metadata().map_err(display)?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(
                "cgroup delegation must be owned by this UID without group/other write".to_owned(),
            );
        }
        require_domain(&fd)?;
        // The supervisor owns the delegation boundary's lifecycle controls;
        // delegation grants creation/placement BELOW it, not permission to
        // freeze or kill that parent (which may contain our own controller).
        // Inspect interface availability without requesting write authority.
        // Exact child controls are opened and validated during reservation.
        // Root cgroups lack these interfaces and are refused.
        let _freeze = open_control(&fd, c"cgroup.freeze", libc::O_PATH)?;
        let _kill = open_control(&fd, c"cgroup.kill", libc::O_PATH)?;
        let _placement = open_control(&fd, c"cgroup.procs", libc::O_WRONLY)?;
        Ok(Self { directory })
    }

    pub fn identity(&self) -> Result<PinnedDirectoryIdentity, String> {
        self.directory.identity().map_err(display)
    }

    /// Reserve a new exact execution child. Existing children are never
    /// adopted. The caller supplies its existing execution allocation name.
    pub fn create(&self, name: &str, timeout: Duration) -> Result<Arc<ProcessCgroup>, String> {
        let deadline = control_deadline(timeout)?;
        let name_c = child_name(name)?;
        let parent = self.directory.try_clone_descriptor().map_err(display)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
            return Err(format!(
                "reserve execution cgroup: {}",
                io::Error::last_os_error()
            ));
        }
        // A cgroupfs mkdir is the kernel operation, not a persistent directory
        // transaction; PinnedDirectory::create_child deliberately fsyncs and is
        // therefore NOT the correct primitive for this pseudo-filesystem.
        let directory = self
            .directory
            .open_child_directory(name.as_ref())
            .map_err(display)?
            .ok_or_else(|| "new execution cgroup disappeared".to_owned())?;
        let identity = CgroupIdentity {
            parent: self.directory.identity().map_err(display)?,
            directory: directory.identity().map_err(display)?,
            name: name.to_owned(),
        };
        let prepared = (|| {
            let cgroup = ProcessCgroup::from_directory(directory, identity.clone(), deadline)?;
            let events = cgroup.events()?;
            if events.populated || events.frozen {
                return Err("new execution cgroup is populated or frozen".to_owned());
            }
            Ok(Arc::new(cgroup))
        })();
        match prepared {
            Ok(cgroup) => Ok(cgroup),
            Err(error) => {
                // No launch authority has escaped this method. Failed control
                // validation must not leak a fresh empty resource on every
                // admission retry. Remove only the incarnation just created;
                // the kernel refuses removal if an external actor populated
                // it. Do not kill, adopt, or remove a same-named replacement.
                match self.remove_exact_child(&identity) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(format!(
                        "{error}; unlaunched scope retirement failed: {cleanup}; retained allocation: {identity:?}"
                    )),
                }
            }
        }
    }

    pub fn reopen(
        &self,
        identity: &CgroupIdentity,
        timeout: Duration,
    ) -> Result<Arc<ProcessCgroup>, String> {
        let deadline = control_deadline(timeout)?;
        child_name(&identity.name)?;
        if self.directory.identity().map_err(display)? != identity.parent {
            return Err("execution cgroup names another delegation".to_owned());
        }
        let directory = self
            .directory
            .open_child_directory(identity.name.as_ref())
            .map_err(display)?
            .ok_or_else(|| "retained execution cgroup is absent; cleanup is unproved".to_owned())?;
        if directory.identity().map_err(display)? != identity.directory {
            return Err("retained execution cgroup directory was replaced".to_owned());
        }
        Ok(Arc::new(ProcessCgroup::from_directory(
            directory,
            identity.clone(),
            deadline,
        )?))
    }

    /// The higher owner must first settle attachment/workspace authority and
    /// exclude further placement. No Drop automatically destroys replay proof.
    pub fn retire_empty(&self, identity: &CgroupIdentity, timeout: Duration) -> Result<(), String> {
        let retained = self.reopen(identity, timeout)?;
        if retained.events()?.populated {
            return Err("cannot retire a populated execution cgroup".to_owned());
        }
        self.remove_exact_child(identity)
    }

    fn remove_exact_child(&self, identity: &CgroupIdentity) -> Result<(), String> {
        self.remove_child(identity, false)
    }

    /// The higher journal owns this exact unbound allocation slot and has
    /// fenced its creator. Unlike a bound scope, no process launch was ever
    /// authorized. Pin the current empty slot for removal only; do not adopt
    /// its controls, signal it, or recurse into a foreign child hierarchy.
    pub fn discard_unlaunched_slot(&self, name: &str) -> Result<(), String> {
        child_name(name)?;
        let Some(directory) = self
            .directory
            .open_child_directory(name.as_ref())
            .map_err(display)?
        else {
            return Ok(());
        };
        let identity = CgroupIdentity {
            parent: self.directory.identity().map_err(display)?,
            directory: directory.identity().map_err(display)?,
            name: name.to_owned(),
        };
        self.remove_child(&identity, true)
    }

    /// Resume an already durably authorized resource retirement. This is not
    /// observation of a process lifetime. An absent leaf only settles this
    /// removal operation; ordinary recovery continues to reject absence.
    pub fn retire_settled(&self, identity: &CgroupIdentity) -> Result<(), String> {
        self.remove_child(identity, true)
    }

    fn remove_child(
        &self,
        identity: &CgroupIdentity,
        retirement_reserved: bool,
    ) -> Result<(), String> {
        child_name(&identity.name)?;
        if self.directory.identity().map_err(display)? != identity.parent {
            return Err("scope retirement names another delegation".to_owned());
        }
        let current = self
            .directory
            .open_child_directory(identity.name.as_ref())
            .map_err(display)?;
        let Some(current) = current else {
            return if retirement_reserved {
                Ok(())
            } else {
                Err("scope retirement target is absent".to_owned())
            };
        };
        if current.identity().map_err(display)? != identity.directory {
            return Err("scope retirement target was replaced".to_owned());
        }
        let descriptor = current.try_clone_descriptor().map_err(display)?;
        let events = open_control(&descriptor, c"cgroup.events", libc::O_RDONLY)?;
        if parse_events(&read_control(&events)?)?.populated {
            return Err("scope retirement target is still populated".to_owned());
        }
        // This is a kernel leaf removal, not recursive filesystem deletion.
        // A live member or child scope makes rmdir fail. The delegation's
        // trusted controller excludes concurrent replacement; workloads have
        // neither these descriptors nor access to its host control mount.
        let parent = self.directory.try_clone_descriptor().map_err(display)?;
        let name = child_name(&identity.name)?;
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            let error = io::Error::last_os_error();
            if retirement_reserved && error.kind() == io::ErrorKind::NotFound {
                return Ok(());
            }
            return Err(format!("retire exact empty cgroup: {}", error));
        }
        Ok(())
    }
}

/// Retained kernel controls. These descriptors belong to the process owner,
/// never to the workload's admitted mount or inherited-channel collection.
#[derive(Debug)]
pub struct ProcessCgroup {
    identity: CgroupIdentity,
    events: File,
    freeze: File,
    kill: File,
    placement: File,
    members: File,
    freeze_owned: AtomicBool,
}

impl ProcessCgroup {
    /// Prove this controller can place a child in the exact reserved scope,
    /// then freeze and terminate it. The child performs only async-signal-safe
    /// syscalls and never execs a host program or workload. CLONE_PIDFD retains
    /// the child incarnation atomically, including failed placement.
    pub fn probe_lifecycle(self: &Arc<Self>, timeout: Duration) -> Result<(), String> {
        if timeout.is_zero() {
            return Err("process scope probe needs a positive bounded timeout".to_owned());
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "process scope probe deadline overflow".to_owned())?;
        if self.events()?.populated || read_control(&self.freeze)?.trim() != "0" {
            return Err("process scope probe requires an empty unfrozen reservation".to_owned());
        }
        // This child never execs: CLOEXEC is not a descriptor boundary.
        // Reuse the held launcher's barrier, including its registered child
        // closes, rather than introducing a second fork/lock protocol. Keep
        // the bounded probe's report endpoints inside this exclusive window.
        let fork_guard = crate::exec::quiesce_fork_sensitive_descriptors(deadline)?;
        let child_close_fds = fork_guard.fork_child_close_fds(&Default::default())?;
        let mut report = [-1; 2];
        if unsafe { libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(format!(
                "create scope probe report: {}",
                io::Error::last_os_error()
            ));
        }
        let reader = unsafe { File::from_raw_fd(report[0]) };
        let writer = unsafe { File::from_raw_fd(report[1]) };
        let placement = self.placement.as_raw_fd();
        let parent = unsafe { libc::getpid() };
        let mut child_pidfd = -1;
        // With no shared VM or replacement stack this is fork-like. No Rust
        // destructor, allocation, logging or inherited lock runs in the child.
        let pid = unsafe {
            libc::syscall(
                libc::SYS_clone,
                libc::CLONE_PIDFD | libc::SIGCHLD,
                0usize,
                &mut child_pidfd as *mut i32,
                0usize,
                0usize,
            )
        };
        if pid < 0 {
            return Err(format!(
                "create exact scope probe child: {}",
                io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            unsafe {
                for fd in &child_close_fds {
                    libc::close(*fd);
                }
                libc::close(report[0]);
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                    || libc::getppid() != parent
                {
                    libc::_exit(125);
                }
                let error = loop {
                    let written = libc::write(placement, b"0".as_ptr().cast(), 1);
                    if written == 1 {
                        break 0i32;
                    }
                    let error = *libc::__errno_location();
                    if written < 0 && error == libc::EINTR {
                        continue;
                    }
                    break if written < 0 { error } else { libc::EIO };
                };
                let message = error.to_ne_bytes();
                let written = loop {
                    let written = libc::write(report[1], message.as_ptr().cast(), message.len());
                    if written < 0 && *libc::__errno_location() == libc::EINTR {
                        continue;
                    }
                    break written;
                };
                if error != 0 || written != message.len() as isize {
                    libc::_exit(125);
                }
                // Parent report-reader closure is also loss of ownership.
                // Do not leave an indefinitely paused child behind merely
                // because signal submission or a bounded wait failed. There
                // is no workload in this probe and no permission to continue
                // after its exact controller has abandoned the report.
                wait_probe_controller_close(report[1]);
                libc::_exit(0);
            }
        }
        let child_pidfd = unsafe { File::from_raw_fd(child_pidfd) };
        drop(writer);
        let outcome = (|| {
            read_placement_report(&reader, deadline)?;
            let frozen = self.freeze(deadline.saturating_duration_since(Instant::now()))?;
            frozen.terminate(deadline.saturating_duration_since(Instant::now()))
        })();
        drop(reader);
        // Only this disposable non-executing probe owns the leaf. If its kill
        // attempt failed, thawing allows controller loss to exit the probe.
        // This is NOT the workload FrozenCgroup failure rule: a real exclusive
        // workspace barrier must stay frozen until durable recovery settles it.
        let thaw_error = if outcome.is_err() {
            write_control(&self.freeze, b"0").err()
        } else {
            None
        };
        // Failed placement leaves the child outside the scope. Cleanup always
        // uses its atomically retained pidfd, never a numeric-PID substitute.
        let cleanup = (|| {
            let signal_error =
                super::linux::pidfd_signal(child_pidfd.as_raw_fd(), libc::SIGKILL, 0)
                    .err()
                    .filter(|error| error.raw_os_error() != Some(libc::ESRCH));
            // A refused signal does not preclude an already-exited child.
            // Always observe the pinned lifetime and reap when proved; never
            // discard that obligation solely because signal submission failed.
            super::linux::wait_pidfd_exit(child_pidfd.as_raw_fd(), deadline).map_err(|error| {
                format!("{error}; signal error: {signal_error:?}; probe thaw error: {thaw_error:?}")
            })?;
            let mut status = 0;
            loop {
                let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
                if waited == pid as libc::pid_t {
                    break;
                }
                let error = io::Error::last_os_error();
                if waited < 0 && error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(format!("reap exact scope probe: {error}"));
            }
            self.wait_empty(deadline.saturating_duration_since(Instant::now()))
        })();
        match (outcome, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(format!("scope probe cleanup remains unproved: {error}")),
            (Err(error), Err(cleanup)) => Err(format!(
                "{error}; scope probe cleanup remains unproved: {cleanup}"
            )),
        }
    }

    fn from_directory(
        directory: PinnedDirectory,
        identity: CgroupIdentity,
        deadline: Instant,
    ) -> Result<Self, String> {
        let fd = directory.try_clone_descriptor().map_err(display)?;
        require_cgroup2(&fd)?;
        require_domain(&fd)?;
        // An execution owns a leaf; its processes inherit that membership
        // across groups and namespaces. Workloads receive no delegation or
        // writable cgroup mount. Refuse externally introduced subgroups and
        // any leaf containing this controller rather than assuming a fixed
        // host supervisor layout. The caller's migration permission is proved
        // by the exact pre-exec placement operation, not by a path convention.
        let entries = directory.entries_no_follow_bounded(1024).map_err(display)?;
        if entries
            .iter()
            .any(|entry| entry.entry_type == crate::PinnedEntryType::Directory)
        {
            return Err(
                "execution cgroup must be a leaf, without delegated child groups".to_owned(),
            );
        }
        let members = open_control(&fd, c"cgroup.procs", libc::O_RDONLY)?;
        require_controller_excluded(
            |buffer, offset| members.read_at(buffer, offset),
            std::process::id(),
            deadline,
        )?;
        Ok(Self {
            identity,
            events: open_control(&fd, c"cgroup.events", libc::O_RDONLY)?,
            freeze: open_control(&fd, c"cgroup.freeze", libc::O_RDWR)?,
            kill: open_control(&fd, c"cgroup.kill", libc::O_WRONLY)?,
            placement: open_control(&fd, c"cgroup.procs", libc::O_WRONLY)?,
            members,
            freeze_owned: AtomicBool::new(false),
        })
    }

    pub fn identity(&self) -> &CgroupIdentity {
        &self.identity
    }

    /// The held launcher already pins this target's incarnation. Confirm it
    /// belongs to this exact scope before handing attachment authority out.
    pub fn require_member(&self, pid: u32, timeout: Duration) -> Result<(), String> {
        if membership_contains(
            |buffer, offset| self.members.read_at(buffer, offset),
            pid,
            control_deadline(timeout)?,
        )? {
            Ok(())
        } else {
            Err("held target does not belong to its reserved execution scope".to_owned())
        }
    }

    /// Place only the forked command itself, before exec or any workload code.
    /// Writing zero lets the kernel select the exact current task; no numeric
    /// PID race or post-launch move is involved. The descriptor is CLOEXEC.
    pub fn configure_command(&self, command: &mut std::process::Command) -> Result<(), String> {
        let placement = self.placement.try_clone().map_err(display)?;
        unsafe {
            command.pre_exec(move || {
                // Only async-signal-safe operations run after fork. One kernel
                // control write is required; a short write is failure.
                loop {
                    let result = libc::write(placement.as_raw_fd(), b"0".as_ptr().cast(), 1);
                    if result == 1 {
                        return Ok(());
                    }
                    let error = io::Error::last_os_error();
                    if result < 0 && error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(if result < 0 {
                        error
                    } else {
                        io::Error::from_raw_os_error(libc::EIO)
                    });
                }
            });
        }
        Ok(())
    }

    /// Caller must hold the existing execution/contact fence, excluding new
    /// placement and concurrent recovered owners. This guard covers forks,
    /// threads, sessions and nested namespaces, not just the main process.
    pub fn freeze(self: &Arc<Self>, timeout: Duration) -> Result<FrozenCgroup, String> {
        if self
            .freeze_owned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("execution cgroup already has a live freeze owner".to_owned());
        }
        // Check the requested state, not just the eventual frozen event. An
        // in-progress freeze belongs to its prior owner too. No unwind guard
        // may thaw that owner's scope if inspection or acquisition fails.
        let available = read_control(&self.freeze).and_then(|value| match value.trim() {
            "0" => Ok(()),
            "1" => Err(
                "execution cgroup already has a freeze request; recover its existing owner"
                    .to_owned(),
            ),
            _ => Err("invalid kernel cgroup freeze state".to_owned()),
        });
        if let Err(error) = available {
            self.freeze_owned.store(false, Ordering::Release);
            return Err(error);
        }
        let guard = FrozenCgroup {
            cgroup: Arc::clone(self),
            settled: false,
        };
        write_control(&self.freeze, b"1")?;
        self.wait_for(timeout, |events| events.frozen)?;
        Ok(guard)
    }

    pub fn terminate_and_wait(&self, timeout: Duration) -> Result<(), String> {
        write_control(&self.kill, b"1")?;
        self.wait_empty(timeout)
    }

    /// Reacquire an already requested barrier after the execution owner has
    /// fenced out its predecessor. Unlike fresh acquisition, failure here must
    /// never undo the retained request: its workspace may have another writer.
    pub fn recover_freeze(self: &Arc<Self>, timeout: Duration) -> Result<FrozenCgroup, String> {
        if self
            .freeze_owned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("execution cgroup already has a live freeze owner".to_owned());
        }
        let result = read_control(&self.freeze).and_then(|value| {
            if value.trim() != "1" {
                return Err("retained execution barrier has no kernel freeze request".to_owned());
            }
            self.wait_for(timeout, |events| events.frozen)
        });
        if let Err(error) = result {
            self.freeze_owned.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(FrozenCgroup {
            cgroup: Arc::clone(self),
            settled: false,
        })
    }

    /// This is valid only while the existing launch fence excludes new members.
    /// The kernel's populated bit includes all descendant cgroups.
    pub fn wait_empty(&self, timeout: Duration) -> Result<(), String> {
        self.wait_for(timeout, |events| !events.populated)
    }

    pub fn is_empty(&self) -> Result<bool, String> {
        self.events().map(|events| !events.populated)
    }

    fn events(&self) -> Result<CgroupEvents, String> {
        parse_events(&read_control(&self.events)?)
    }

    fn wait_for(
        &self,
        timeout: Duration,
        ready: impl Fn(CgroupEvents) -> bool,
    ) -> Result<(), String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "cgroup observation deadline overflow".to_owned())?;
        loop {
            if ready(self.events()?) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("cgroup state did not settle before deadline".to_owned());
            }
            let mut poll = libc::pollfd {
                fd: self.events.as_raw_fd(),
                events: libc::POLLPRI | libc::POLLERR,
                revents: 0,
            };
            let result = unsafe {
                libc::poll(
                    &mut poll,
                    1,
                    remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(format!("wait for cgroup notification: {error}"));
            }
            if poll.revents & (libc::POLLNVAL | libc::POLLHUP) != 0 {
                return Err("retained cgroup event descriptor became unavailable".to_owned());
            }
        }
    }
}

/// Live kernel barrier. Ordinary capture cancellation thaws its exact scope.
/// Exclusive-workspace owners must explicitly terminate instead on cancellation.
#[derive(Debug)]
pub struct FrozenCgroup {
    cgroup: Arc<ProcessCgroup>,
    settled: bool,
}

impl FrozenCgroup {
    pub fn resume(mut self, timeout: Duration) -> Result<(), String> {
        write_control(&self.cgroup.freeze, b"0")?;
        self.cgroup.wait_for(timeout, |events| !events.frozen)?;
        self.settled = true;
        self.cgroup.freeze_owned.store(false, Ordering::Release);
        Ok(())
    }

    pub fn terminate(mut self, timeout: Duration) -> Result<(), String> {
        // On failure leave the scope frozen for durable recovery. Implicit
        // thaw would let the old writer race its exclusive workspace borrower.
        self.settled = true;
        let result = self.cgroup.terminate_and_wait(timeout);
        self.cgroup.freeze_owned.store(false, Ordering::Release);
        result
    }

    pub fn resume_or_terminate(mut self, timeout: Duration) -> Result<(), String> {
        let started = Instant::now();
        let resumed = write_control(&self.cgroup.freeze, b"0")
            .and_then(|()| self.cgroup.wait_for(timeout, |events| !events.frozen));
        if resumed.is_ok() {
            self.settled = true;
            self.cgroup.freeze_owned.store(false, Ordering::Release);
            return Ok(());
        }
        let resume_error = resumed.unwrap_err();
        // Consume via exact termination, which deliberately suppresses thaw on
        // failure. Neither a lost resume acknowledgement nor Drop settles the
        // caller's durable workspace journal.
        match self.terminate(timeout.saturating_sub(started.elapsed())) {
            Ok(()) => Err(format!(
                "execution scope resume was not proved; scope was terminated: {resume_error}"
            )),
            Err(error) => Err(format!(
                "execution scope resume was not proved ({resume_error}); termination remains unproved ({error})"
            )),
        }
    }
}

impl Drop for FrozenCgroup {
    fn drop(&mut self) {
        if !self.settled {
            // Best-effort unwind only; explicit resume carries observable errors
            // to the higher owner, which must retain unresolved durable fences.
            let _ = write_control(&self.cgroup.freeze, b"0");
            self.cgroup.freeze_owned.store(false, Ordering::Release);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CgroupEvents {
    populated: bool,
    frozen: bool,
}

fn parse_events(raw: &str) -> Result<CgroupEvents, String> {
    let (mut populated, mut frozen) = (None, None);
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let name = fields
            .next()
            .ok_or_else(|| "empty cgroup event field".to_owned())?;
        let value = fields
            .next()
            .ok_or_else(|| "missing cgroup event value".to_owned())?;
        if fields.next().is_some() {
            return Err("malformed cgroup event field".to_owned());
        }
        let slot = match name {
            "populated" => &mut populated,
            "frozen" => &mut frozen,
            _ => continue,
        };
        if slot.is_some() {
            return Err("duplicate cgroup event field".to_owned());
        }
        *slot = Some(match value {
            "0" => false,
            "1" => true,
            _ => return Err("non-boolean cgroup event field".to_owned()),
        });
    }
    Ok(CgroupEvents {
        populated: populated.ok_or_else(|| "cgroup events lacks populated".to_owned())?,
        frozen: frozen.ok_or_else(|| "cgroup events lacks frozen".to_owned())?,
    })
}

fn require_cgroup2(file: &File) -> Result<(), String> {
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(display(io::Error::last_os_error()));
    }
    if stat.f_type != CGROUP2_SUPER_MAGIC {
        return Err("authority is not cgroup v2".to_owned());
    }
    Ok(())
}

fn require_domain(directory: &File) -> Result<(), String> {
    let file = open_control(directory, c"cgroup.type", libc::O_RDONLY)?;
    if read_control(&file)?.trim() != "domain" {
        return Err("execution lifecycle requires a domain cgroup".to_owned());
    }
    Ok(())
}

fn child_name(name: &str) -> Result<CString, String> {
    super::scope::validate_allocation_name(name)?;
    CString::new(name).map_err(display)
}

fn open_control(directory: &File, name: &CStr, access: i32) -> Result<File, String> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            access | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(format!(
            "open {}: {}",
            name.to_string_lossy(),
            io::Error::last_os_error()
        ));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    require_cgroup2(&file)?;
    Ok(file)
}

fn read_control(file: &File) -> Result<String, String> {
    let mut bytes = [0; MAX_CONTROL_RECORD_BYTES + 1];
    let size = file.read_at(&mut bytes, 0).map_err(display)?;
    if size > MAX_CONTROL_RECORD_BYTES {
        return Err("cgroup control record exceeds kernel-interface bound".to_owned());
    }
    String::from_utf8(bytes[..size].to_vec()).map_err(display)
}

fn control_deadline(timeout: Duration) -> Result<Instant, String> {
    if timeout.is_zero() {
        return Err("process scope control needs a positive bounded timeout".to_owned());
    }
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "process scope control deadline overflow".to_owned())
}

/// Membership is a variable-length kernel stream, not a small control record.
/// Do not cap its total bytes or enumerate it into memory: that would impose a
/// second, accidental process limit unrelated to admitted workload resources.
/// A changing membership stream or repeated EINTR is bounded by the caller's
/// lifecycle deadline. This is only controller exclusion; cgroup.events and
/// the kernel freezer/kill operations, never this scan, prove scope settlement.
fn require_controller_excluded(
    read_at: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    controller: u32,
    deadline: Instant,
) -> Result<(), String> {
    if membership_contains(read_at, controller, deadline)? {
        Err("execution cgroup contains the controlling process".to_owned())
    } else {
        Ok(())
    }
}

fn membership_contains(
    mut read_at: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    expected: u32,
    deadline: Instant,
) -> Result<bool, String> {
    let mut buffer = [0; MAX_CONTROL_RECORD_BYTES];
    let mut offset = 0u64;
    let mut pid = 0u32;
    let mut digits = 0u32;
    loop {
        if Instant::now() >= deadline {
            return Err("process scope membership check exceeded its control deadline".to_owned());
        }
        let size = match read_at(&mut buffer, offset) {
            Ok(size) => size,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("read execution scope membership: {error}")),
        };
        if size == 0 {
            return if digits == 0 {
                Ok(false)
            } else {
                Err("incomplete execution scope member record".to_owned())
            };
        }
        for byte in &buffer[..size] {
            match byte {
                b'0'..=b'9' if digits < 10 => {
                    pid = pid
                        .checked_mul(10)
                        .and_then(|pid| pid.checked_add(u32::from(byte - b'0')))
                        .filter(|pid| *pid <= i32::MAX as u32)
                        .ok_or_else(|| "invalid execution scope member PID".to_owned())?;
                    digits += 1;
                }
                b'\n' if digits > 0 => {
                    if pid == expected {
                        return Ok(true);
                    }
                    pid = 0;
                    digits = 0;
                }
                _ => return Err("invalid execution scope member record".to_owned()),
            }
        }
        offset = offset
            .checked_add(size as u64)
            .ok_or_else(|| "execution scope membership offset overflow".to_owned())?;
    }
}

fn read_placement_report(reader: &File, deadline: Instant) -> Result<(), String> {
    let mut bytes = [0u8; 4];
    let mut offset = 0;
    while offset < bytes.len() {
        if Instant::now() >= deadline {
            return Err("scope placement probe exceeded its deadline".to_owned());
        }
        let size = unsafe {
            libc::read(
                reader.as_raw_fd(),
                bytes[offset..].as_mut_ptr().cast(),
                bytes.len() - offset,
            )
        };
        if size > 0 {
            offset += size as usize;
            continue;
        }
        if size == 0 {
            return Err("scope probe exited before placement testimony".to_owned());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(format!("read scope placement testimony: {error}"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("scope placement probe exceeded its deadline".to_owned());
        }
        let mut poll = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let polled = unsafe {
            libc::poll(
                &mut poll,
                1,
                remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
            )
        };
        if polled < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Err(format!(
                "wait for scope placement testimony: {}",
                io::Error::last_os_error()
            ));
        }
        if poll.revents & libc::POLLNVAL != 0 {
            return Err("scope placement report descriptor became invalid".to_owned());
        }
    }
    let error = i32::from_ne_bytes(bytes);
    if error != 0 {
        return Err(format!(
            "execution scope placement refused by host: {}",
            io::Error::from_raw_os_error(error)
        ));
    }
    Ok(())
}

/// Child-side report lifeline. Only async-signal-safe syscalls may run here:
/// the caller is forked from a multithreaded daemon and never execs. No Rust
/// lock, allocation, logging, or destructor belongs in this wait path.
unsafe fn wait_probe_controller_close(fd: i32) {
    let mut control = libc::pollfd {
        fd,
        events: 0,
        revents: 0,
    };
    loop {
        let ready = unsafe { libc::poll(&mut control, 1, -1) };
        if ready > 0 || (ready < 0 && unsafe { *libc::__errno_location() } != libc::EINTR) {
            return;
        }
    }
}

fn write_control(file: &File, bytes: &[u8]) -> Result<(), String> {
    let size = file.write_at(bytes, 0).map_err(display)?;
    if size != bytes.len() {
        return Err("short cgroup control write".to_owned());
    }
    Ok(())
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn membership_scan_is_streamed_and_does_not_create_a_process_count_limit() {
        let members = "12345\n".repeat(10_000);
        let check = |bytes: &[u8], controller, chunk| {
            require_controller_excluded(
                |buffer, offset| {
                    let remaining = &bytes[offset as usize..];
                    let size = remaining.len().min(buffer.len()).min(chunk);
                    buffer[..size].copy_from_slice(&remaining[..size]);
                    Ok(size)
                },
                controller,
                Instant::now() + Duration::from_secs(5),
            )
        };
        for chunk in [1, 7, MAX_CONTROL_RECORD_BYTES] {
            check(members.as_bytes(), 54321, chunk).unwrap();
            let with_controller = format!("{members}54321\n");
            assert!(
                check(with_controller.as_bytes(), 54321, chunk)
                    .unwrap_err()
                    .contains("controlling process")
            );
        }
        for malformed in ["123", "\n", "abc\n", "2147483648\n", "00000000000\n"] {
            assert!(check(malformed.as_bytes(), 54321, 1).is_err());
        }
    }

    #[test]
    fn membership_scan_retains_its_deadline_across_interruptions() {
        let deadline = Instant::now() + Duration::from_millis(1);
        let error = require_controller_excluded(
            |_, _| Err(io::Error::from(io::ErrorKind::Interrupted)),
            54321,
            deadline,
        )
        .unwrap_err();
        assert!(error.contains("control deadline"), "{error}");
        let error = require_controller_excluded(
            |_, _| panic!("an expired deadline must not read kernel membership"),
            54321,
            Instant::now(),
        )
        .unwrap_err();
        assert!(error.contains("control deadline"), "{error}");
    }

    #[test]
    fn placement_probe_report_is_bounded_and_preserves_host_refusal() {
        use std::io::Write;
        for reported in [Some(0i32), Some(libc::EACCES), None] {
            let mut fds = [-1; 2];
            assert_eq!(
                unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
                0
            );
            let reader = unsafe { File::from_raw_fd(fds[0]) };
            let mut writer = unsafe { File::from_raw_fd(fds[1]) };
            if let Some(error) = reported {
                writer.write_all(&error.to_ne_bytes()).unwrap();
            }
            drop(writer);
            let result = read_placement_report(&reader, Instant::now() + Duration::from_secs(1));
            match reported {
                Some(0) => result.unwrap(),
                Some(_) => assert!(result.unwrap_err().contains("placement refused by host")),
                None => assert!(result.unwrap_err().contains("before placement testimony")),
            }
        }
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        let reader = unsafe { File::from_raw_fd(fds[0]) };
        let _writer = unsafe { File::from_raw_fd(fds[1]) };
        assert!(
            read_placement_report(&reader, Instant::now())
                .unwrap_err()
                .contains("deadline")
        );
    }

    #[test]
    fn probe_lifeline_exits_when_the_controller_closes_without_a_signal() {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let reader = unsafe { File::from_raw_fd(fds[0]) };
        let writer = unsafe { File::from_raw_fd(fds[1]) };
        let (tx, rx) = std::sync::mpsc::channel();
        let probe = std::thread::spawn(move || {
            unsafe {
                wait_probe_controller_close(writer.as_raw_fd());
            }
            tx.send(()).unwrap();
        });
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(reader);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        probe.join().unwrap();
    }

    #[test]
    fn cgroup_events_require_exact_complete_boolean_fields() {
        assert_eq!(
            parse_events("populated 1\nfrozen 0\n").unwrap(),
            CgroupEvents {
                populated: true,
                frozen: false
            }
        );
        for raw in [
            "",
            "populated 0",
            "populated 0\nfrozen 2",
            "populated 0\nfrozen 0\nfrozen 1",
            "populated 0 1\nfrozen 0",
        ] {
            assert!(parse_events(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn cgroup_authority_has_no_ordinary_directory_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let error = DelegatedCgroup::open(dir.path()).unwrap_err();
        assert!(error.contains("not cgroup v2"), "{error}");
    }

    #[test]
    fn cgroup_children_cannot_escape_delegation() {
        for name in ["", ".", "..", "../other", "/system", "a/b", "a\0b"] {
            assert!(child_name(name).is_err());
        }
        assert!(child_name(&"a".repeat(161)).is_err());
        assert!(child_name("execution-123").is_ok());
    }

    #[test]
    #[ignore = "requires explicit disposable cgroup delegation and controller placement; never run against a node scope"]
    fn cgroup_native_freeze_covers_detached_nested_writers_and_recovery() {
        if run_in_disposable_delegation(
            "process_control::cgroup::tests::cgroup_native_freeze_covers_detached_nested_writers_and_recovery",
        ) {
            return;
        }
        use std::io::{BufRead, BufReader};
        use std::process::{Child, Command, Stdio};

        let parent_path = std::env::var_os("LILLUX_TEST_CGROUP_PARENT")
            .expect("native qualification requires an explicitly delegated parent");
        let parent_path = Path::new(&parent_path);
        let parent = DelegatedCgroup::open(parent_path).unwrap();
        let name = format!("qualification-{:032x}", rand::random::<u128>());
        let scope = parent.create(&name, Duration::from_secs(5)).unwrap();
        assert!(
            parent.create(&name, Duration::from_secs(5)).is_err(),
            "must never adopt an existing execution scope"
        );
        let sibling = parent
            .create(&format!("{name}-sibling"), Duration::from_secs(5))
            .unwrap();
        // A probe must obey the existing daemon fork/descriptor authority,
        // even though it never runs a workload or executes an image.
        let lease = crate::retain_fork_sensitive_descriptors();
        let refused = sibling.probe_lifecycle(Duration::from_secs(1)).unwrap_err();
        assert!(
            refused.contains("calling thread retains fork-sensitive descriptor authority"),
            "{refused}"
        );
        assert!(!sibling.events().unwrap().populated);
        drop(lease);
        let state = tempfile::tempdir().unwrap();
        let heartbeat = state.path().join("heartbeat");

        struct Fixture {
            child: Child,
            scope: Arc<ProcessCgroup>,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                // Terminate exactly the scope we created, never the delegation.
                // Reap is bounded by the completed cgroup-empty observation.
                if self
                    .scope
                    .terminate_and_wait(Duration::from_secs(5))
                    .is_ok()
                {
                    let _ = self.child.wait();
                }
            }
        }
        // util-linux/shell are only fixture construction. Production has no
        // dependency on these programs, their PATH, or their host libraries.
        let mut command = Command::new("unshare");
        command.args([
            "--user", "--map-root-user", "--pid", "--fork", "--kill-child=KILL",
            "/bin/sh", "-c",
            "setsid unshare --pid --fork --kill-child=KILL /bin/sh -c 'echo ready; while :; do printf x >> \"$1\"; sleep 0.01; done' fixture \"$1\" & wait",
            "fixture",
        ]).arg(&heartbeat).stdin(Stdio::null()).stdout(Stdio::piped());
        scope.configure_command(&mut command).unwrap();
        let mut fixture = Fixture {
            child: command.spawn().unwrap(),
            scope: Arc::clone(&scope),
        };
        let stdout = fixture.child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            "ready\n"
        );
        reader.join().unwrap();

        let mut command = Command::new("sleep");
        command.arg("60").stdin(Stdio::null()).stdout(Stdio::null());
        sibling.configure_command(&mut command).unwrap();
        let mut other = Fixture {
            child: command.spawn().unwrap(),
            scope: Arc::clone(&sibling),
        };
        let until = Instant::now() + Duration::from_secs(5);
        while !heartbeat.exists() {
            assert!(
                Instant::now() < until,
                "nested writer never produced its heartbeat"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let frozen = scope.freeze(Duration::from_secs(5)).unwrap();
        assert!(scope.events().unwrap().frozen);
        assert!(scope.freeze(Duration::ZERO).is_err());
        let reopened = parent
            .reopen(scope.identity(), Duration::from_secs(5))
            .unwrap();
        assert!(reopened.freeze(Duration::ZERO).is_err());
        assert!(
            scope.events().unwrap().frozen,
            "failed acquisition must not thaw an earlier owner"
        );
        let bytes = std::fs::read(&heartbeat).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            std::fs::read(&heartbeat).unwrap(),
            bytes,
            "detached/nested writer escaped freeze"
        );
        assert!(!sibling.events().unwrap().frozen);
        assert!(other.child.try_wait().unwrap().is_none());
        frozen.resume(Duration::from_secs(5)).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while std::fs::read(&heartbeat).unwrap().len() == bytes.len() {
            assert!(Instant::now() < until, "writer did not resume");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reopened.recover_freeze(Duration::ZERO).is_err());
        assert!(
            !scope.events().unwrap().frozen,
            "recovery invented a new freeze request"
        );
        // Model a retained kernel request without a live Rust guard. This
        // exercises reacquisition, not a full daemon crash qualification.
        write_control(&scope.freeze, b"1").unwrap();
        let recovered = reopened.recover_freeze(Duration::from_secs(5)).unwrap();
        assert!(scope.events().unwrap().frozen);
        assert!(reopened.recover_freeze(Duration::ZERO).is_err());
        recovered
            .resume_or_terminate(Duration::from_secs(5))
            .unwrap();
        assert!(!scope.events().unwrap().frozen);
        let mut wrong = scope.identity().clone();
        wrong.directory = sibling.identity().directory;
        assert!(parent.reopen(&wrong, Duration::from_secs(5)).is_err());
        assert!(parent.remove_exact_child(&wrong).is_err());
        assert!(
            parent.remove_exact_child(scope.identity()).is_err(),
            "kernel retirement must refuse a still-populated scope"
        );
        assert!(sibling.events().unwrap().populated);
        let frozen = scope.freeze(Duration::from_secs(5)).unwrap();
        frozen.terminate(Duration::from_secs(5)).unwrap();
        scope.wait_empty(Duration::ZERO).unwrap();
        assert!(
            other.child.try_wait().unwrap().is_none(),
            "termination crossed the sibling boundary"
        );
        assert!(!fixture.child.wait().unwrap().success());
        parent
            .retire_empty(scope.identity(), Duration::from_secs(5))
            .unwrap();
        assert!(
            parent
                .reopen(scope.identity(), Duration::from_secs(5))
                .is_err(),
            "absence grants no recovery authority"
        );
        other
            .scope
            .terminate_and_wait(Duration::from_secs(5))
            .unwrap();
        other.child.wait().unwrap();
        parent
            .retire_empty(sibling.identity(), Duration::from_secs(5))
            .unwrap();
    }

    /// Test-only host supervisor. sudo runs this small setup boundary; the
    /// qualification itself runs as the invoking non-root user, in a separate
    /// controller leaf beside execution leaves. No installed node is involved.
    pub(crate) fn run_in_disposable_delegation(test_name: &str) -> bool {
        if unsafe { libc::geteuid() } != 0 {
            return false;
        }
        let uid: u32 = std::env::var("SUDO_UID")
            .expect("invoke qualification with sudo from a non-root user")
            .parse()
            .unwrap();
        let gid: u32 = std::env::var("SUDO_GID").unwrap().parse().unwrap();
        assert_ne!(uid, 0, "qualification must drop to a non-root caller");
        let root = PinnedDirectory::open(Path::new("/sys/fs/cgroup"))
            .unwrap()
            .unwrap();
        let root_fd = root.try_clone_descriptor().unwrap();
        require_cgroup2(&root_fd).unwrap();
        let name = format!("lillux-qualification-{:032x}", rand::random::<u128>());
        let name_c = child_name(&name).unwrap();
        let bootstrap = provision_controller(&root.path().join(&name), uid, gid).unwrap();
        let parent = &bootstrap.parent;
        let parent_fd = parent.try_clone_descriptor().unwrap();
        eprintln!(
            "disposable qualification delegation: {}",
            parent.path().display()
        );
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                test_name,
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LILLUX_TEST_CGROUP_PARENT", parent.path());
        bootstrap.configure_command(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // Exact test-only parent was created above; never signal the host root.
        let kill = open_control(&parent_fd, c"cgroup.kill", libc::O_WRONLY).unwrap();
        write_control(&kill, b"1").unwrap();
        let events = open_control(&parent_fd, c"cgroup.events", libc::O_RDONLY).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while parse_events(&read_control(&events).unwrap())
            .unwrap()
            .populated
        {
            assert!(
                Instant::now() < deadline,
                "qualification teardown unproved; retained {}",
                parent.path().display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait().unwrap();
        assert_eq!(
            unsafe {
                libc::unlinkat(
                    parent_fd.as_raw_fd(),
                    c"controller".as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            },
            0
        );
        let removed =
            unsafe { libc::unlinkat(root_fd.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR) }
                == 0;
        if !removed {
            eprintln!(
                "retained failed qualification evidence at {}",
                parent.path().display()
            );
        }
        assert!(
            status.is_some_and(|status| status.success()),
            "native qualification failed"
        );
        assert!(removed, "successful qualification left unexpected cgroups");
        true
    }
}
