//! Linux process confinement below product and protocol vocabulary.
//!
//! This module owns the raw namespace, mount, descriptor, `pivot_root`,
//! `no_new_privs`, seccomp, fork, exec, and wait mechanics used by higher
//! layers. Callers supply an already-authorized set of open descriptors and a
//! complete target view; Lillux does not discover tools, projects, bundles, or
//! policy from ambient paths.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Access granted to one exact descriptor-backed mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxSandboxMountAccess {
    ReadOnly,
    Writable,
}

/// One exact already-open source mounted into the private root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxMount {
    pub source_fd: u32,
    pub destination: PathBuf,
    pub access: LinuxSandboxMountAccess,
    pub layer: u32,
}

/// One writable mount whose exact source is a directory below the retained
/// never-attached overlay template. The relative coordinate is not pathname
/// authority: the held descriptor remains the identity witness, and launch
/// reopens this coordinate without following links only after attaching and
/// verifying the exact template in the borrower's private namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxOverlayDescendantMount {
    pub source_fd: u32,
    pub relative_path: PathBuf,
    pub destination: PathBuf,
}

/// A filtered view of one already-authorized directory mount. Parent entries
/// along `denied_paths` are fixed for this launch; permitted mounted children
/// retain the access of the original mount. No policy paths are chosen here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxFixedParentView {
    pub destination: PathBuf,
    pub denied_paths: Vec<PathBuf>,
    pub max_entries: usize,
    pub max_depth: usize,
}

/// Borrow one exact never-attached template into a fresh private sandbox.
/// A borrower never creates another filesystem over the template's layers.
#[derive(Debug, Clone)]
pub struct LinuxSandboxOverlay {
    pub template: LinuxOverlayTemplate,
    pub destination: PathBuf,
    pub writable_descendant_mounts: Vec<LinuxSandboxOverlayDescendantMount>,
}

/// One never-attached overlay mount retained by exact descriptor authority.
///
/// Cloning this value shares the template's lifetime; it does not mount another
/// overlay or attach the template. Each confined borrower must instead clone
/// the mount into its own fresh namespace. This is process-local kernel
/// authority, not a durable receipt or a pathname that can be reconstructed.
#[derive(Debug, Clone)]
pub struct LinuxOverlayTemplate {
    authority: crate::InheritedDescriptorAuthority,
}

impl LinuxOverlayTemplate {
    /// Adopt the exact descriptor assigned this role by the validated launch
    /// protocol. This protects/registers the inherited owner before namespace
    /// entry; it never reopens the template through a pathname.
    ///
    /// # Safety
    /// The caller transfers unique descriptor ownership. No owning File or
    /// other adopted authority may exist for the same numeric descriptor.
    pub unsafe fn take_inherited_descriptor(fd: u32) -> Result<Self, String> {
        imp::take_overlay_template(fd)
    }

    /// Retain the exact template in the existing typed subprocess transport.
    pub fn inherited_authority(&self) -> &crate::InheritedDescriptorAuthority {
        &self.authority
    }

    /// Accept a descriptor delivered by an authenticated, correlated creator
    /// invocation. This checks its kernel object class, not its protocol role.
    /// The caller must already have proved that role and exact creator identity.
    /// A later clone in the borrower's fresh namespace also fails if the source
    /// is an ordinary attached mount; an equal host path is never substituted.
    pub fn from_transferred_authority(
        authority: crate::InheritedDescriptorAuthority,
    ) -> Result<Self, String> {
        imp::validate_overlay_template(&authority)?;
        Ok(Self { authority })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxSandboxNetwork {
    Host,
    Isolated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxSandboxProcFilesystem {
    Empty,
    PidNamespace,
    /// Writable process maps and kernel metadata for nested runtimes. Tasks
    /// remain PID-local; non-task visibility is broader than PidNamespace.
    PidNamespaceNested,
}

/// Final target-release boundary. EOF is refusal, never permission to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxSandboxLifecycle {
    Run,
    AwaitRelease {
        release_fd: u32,
        release_keepalive_fd: u32,
    },
}

/// Aggregate limits require a delegated cgroup-v2 authority. The first native
/// backend slice deliberately refuses this request until that exact authority
/// can be passed in; per-process rlimits are not represented as aggregate
/// containment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxSandboxAggregateLimits {
    pub max_processes: Option<u64>,
    pub max_memory_bytes: Option<u64>,
    pub max_cpu_micros_per_second: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxCharacterDevice {
    pub source_fd: u32,
    pub destination: PathBuf,
    pub access: crate::CharacterDeviceAccess,
    pub major: u32,
    pub minor: u32,
}

/// Complete low-level request for a private Linux execution view.
#[derive(Debug, Clone)]
pub struct LinuxSandboxRequest {
    pub executable: PathBuf,
    pub argv0: OsString,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub mounts: Vec<LinuxSandboxMount>,
    pub fixed_parent_views: Vec<LinuxSandboxFixedParentView>,
    pub overlay: Option<LinuxSandboxOverlay>,
    pub network: LinuxSandboxNetwork,
    pub private_tmp: bool,
    pub proc_filesystem: LinuxSandboxProcFilesystem,
    pub minimal_devices: bool,
    pub character_devices: Vec<LinuxSandboxCharacterDevice>,
    pub target_channels: Vec<(u32, u32)>,
    pub lifecycle: LinuxSandboxLifecycle,
    pub contain_process_group: bool,
    /// Explicitly admitted nested sandboxing. The outer launch owner MUST
    /// retain whole-execution containment; a shared group is insufficient.
    /// PID-local proc becomes writable for child UID/GID maps, never host proc.
    pub nested_sandbox: bool,
    pub aggregate_limits: Option<LinuxSandboxAggregateLimits>,
}

/// Whether a guest mount would overlap namespace structure owned by Lillux.
/// Higher layers may reserve their own authored destinations, but must never
/// reproduce the host backend's proc/device/system/private-tmp layout.
pub fn linux_sandbox_mount_overlaps_managed_namespace(destination: &Path) -> bool {
    ["/proc", "/dev", "/sys", "/tmp"]
        .into_iter()
        .map(Path::new)
        .any(|managed| destination.starts_with(managed) || managed.starts_with(destination))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxSandboxInspection {
    pub descriptor_mounts: bool,
    pub fixed_parent_views: bool,
    pub overlay_workspace: bool,
    pub isolated_network: bool,
    pub private_root: bool,
    pub private_tmp: bool,
    pub minimal_devices: bool,
    pub character_devices: bool,
    pub exact_environment: bool,
    pub isolated_pid_namespace: bool,
    pub pid_namespace_proc: bool,
    pub process_group_containment: bool,
    pub nested_sandbox: bool,
    pub aggregate_resource_isolation: bool,
}

impl LinuxSandboxInspection {
    /// Capabilities implemented by this Lillux backend when its runtime probe
    /// succeeds. This does not probe the current host; admission must use
    /// [`inspect_linux_sandbox`] before selecting the backend, and launch still
    /// fails closed if any kernel operation is unavailable.
    pub const fn declared_native_contract() -> Self {
        Self {
            descriptor_mounts: true,
            fixed_parent_views: true,
            overlay_workspace: true,
            isolated_network: true,
            private_root: true,
            private_tmp: true,
            minimal_devices: true,
            character_devices: true,
            exact_environment: true,
            isolated_pid_namespace: true,
            pid_namespace_proc: true,
            process_group_containment: true,
            nested_sandbox: true,
            aggregate_resource_isolation: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxSandboxExit {
    Code(i32),
    Signal(i32),
}

/// Exact child process created at the final sandbox boundary.
#[derive(Debug)]
pub struct LinuxSandboxProcess {
    #[cfg(target_os = "linux")]
    pid: libc::pid_t,
    /// Present only for the exact child which passed native PID-1 readiness,
    /// never for helper/probe children. Kept private and non-serializable.
    #[cfg(target_os = "linux")]
    namespace_lifetime: Option<std::os::fd::OwnedFd>,
    /// Lillux-private, close-on-exec evidence channel. A record proves setup
    /// or exec failed before the workload existed. EOF alone does not prove
    /// execution: killing a pre-exec child also closes its descriptor. It is
    /// never workload stderr or inherited by a successfully executed target.
    #[cfg(target_os = "linux")]
    launch_failure: Option<std::fs::File>,
    /// Private child-to-owner channel for one bounded post-chdir, pre-exec
    /// observation. This is never a proof that execve succeeded.
    #[cfg(target_os = "linux")]
    applied_launch: Option<std::fs::File>,
    #[cfg(target_os = "linux")]
    observed_applied_launch: Option<LinuxSandboxAppliedLaunchReceipt>,
    /// Child-origin observation of declared mount destinations after pivot,
    /// before readiness. The parent binds it to its exact host child PID.
    #[cfg(target_os = "linux")]
    mount_preparation: Option<LinuxSandboxMountPreparationReceipt>,
    /// Once cleanup has been requested, its observed status can never be
    /// presented as an independently observed target completion.
    #[cfg(target_os = "linux")]
    termination_requested: bool,
}

/// Native testimony from the exact owned PID-namespace init immediately
/// before its execve attempt. The four SHA-256 commitments cover the actual
/// executable, constructed argv/envp, and getcwd result; no argument or
/// environment bytes are retained. A low-entropy value can still be guessed
/// from its digest, so this is not a secrecy boundary or a public log record.
/// Readiness, this receipt, and failure-pipe EOF do not prove exec success.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxSandboxAppliedLaunchReceipt {
    /// Host PID of the still-owned, unreaped child whose private pipe delivered
    /// this record; attached by the owner, never trusted from child bytes.
    pub owned_child_pid: u32,
    pub namespace_pid: u32,
    pub effective_uid: u32,
    pub effective_gid: u32,
    pub no_new_privs: bool,
    pub seccomp_mode: u32,
    pub executable_sha256: [u8; 32],
    pub argv_sha256: [u8; 32],
    pub environment_sha256: [u8; 32],
    pub cwd_sha256: [u8; 32],
    /// Fresh target-view observation after release, before exec. Source
    /// identity remains the separate descriptor-backed pre-release proof.
    pub post_release_mount_view: LinuxSandboxMountPreparationCommitments,
}

/// Bounded final-root mount observation from the held namespace-init before
/// target release. It proves readiness checks, not subsequent exec or source
/// content. Only the exact owning parent supplies the host PID.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxSandboxMountPreparationReceipt {
    pub schema: u32,
    pub owned_child_pid: u32,
    pub mount_count: u32,
    pub destination_access_sha256: [u8; 32],
}

/// Independently computed expectation from admitted source descriptors and
/// destination/access policy. This is not an observation of the target view.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxSandboxMountPreparationCommitments {
    pub schema: u32,
    pub mount_count: u32,
    pub destination_access_sha256: [u8; 32],
}

impl LinuxSandboxMountPreparationCommitments {
    pub fn from_admitted_mounts(
        mounts: &[LinuxSandboxMount],
        writable_overlay_destinations: &[PathBuf],
    ) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            imp::expected_mount_preparation_from_mounts(mounts, writable_overlay_destinations)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (mounts, writable_overlay_destinations);
            Err("native mount commitments are unavailable on this platform".into())
        }
    }
}

impl LinuxSandboxMountPreparationReceipt {
    pub fn matches_commitments(
        &self,
        expected: &LinuxSandboxMountPreparationCommitments,
    ) -> bool {
        self.schema == expected.schema
            && self.owned_child_pid > 0
            && self.mount_count == expected.mount_count
            && self.destination_access_sha256 == expected.destination_access_sha256
    }

    /// Compare the observed final-root tuple set with the complete admitted
    /// native request. This does not independently attest the source content.
    pub fn matches_request(&self, request: &LinuxSandboxRequest) -> Result<bool, String> {
        #[cfg(target_os = "linux")]
        {
            let expected = imp::expected_mount_preparation(request)?;
            Ok(self.matches_commitments(&expected))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = request;
            Err("native mount preparation comparison is unavailable on this platform".into())
        }
    }
}

/// Borrowed expected target fields for an independent receipt comparison.
/// Filesystem mounts and lifecycle controls are deliberately not inferred
/// from these fields; their admission and application need separate evidence.
pub struct LinuxSandboxAppliedLaunchTarget<'a> {
    pub executable: &'a Path,
    pub argv0: &'a std::ffi::OsStr,
    pub arguments: &'a [OsString],
    pub cwd: &'a Path,
    pub environment: &'a BTreeMap<OsString, OsString>,
}

/// Secret-bearing-in-practice commitments to the exact requested pre-exec
/// target. Retain only in protected launch/evaluation state: low-entropy
/// environment values can be guessed from their hashes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxSandboxAppliedLaunchCommitments {
    pub executable_sha256: [u8; 32],
    pub argv_sha256: [u8; 32],
    pub environment_sha256: [u8; 32],
    pub cwd_sha256: [u8; 32],
}

impl LinuxSandboxAppliedLaunchCommitments {
    /// Derive only from an independently admitted target, never from a child
    /// receipt or a later observation object.
    pub fn from_target(target: LinuxSandboxAppliedLaunchTarget<'_>) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            imp::applied_launch_commitments_from_target(target)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = target;
            Err("native applied-launch commitments are unavailable on this platform".into())
        }
    }
}

impl LinuxSandboxAppliedLaunchReceipt {
    /// Compare actual pre-exec C-string commitments with one complete request.
    /// A mismatch refuses; this does not prove the subsequent execve succeeded.
    pub fn matches_request(&self, request: &LinuxSandboxRequest) -> Result<bool, String> {
        self.matches_target(LinuxSandboxAppliedLaunchTarget {
            executable: &request.executable,
            argv0: &request.argv0,
            arguments: &request.arguments,
            cwd: &request.cwd,
            environment: &request.environment,
        })
    }

    /// Compare only the actual pre-exec target fields with independently
    /// authored expected values. No request or mount authority is minted.
    pub fn matches_target(
        &self,
        target: LinuxSandboxAppliedLaunchTarget<'_>,
    ) -> Result<bool, String> {
        #[cfg(target_os = "linux")]
        {
            imp::applied_launch_matches_target(self, target)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = target;
            Err("native applied-launch comparison is unavailable on this platform".into())
        }
    }

    pub fn matches_commitments(&self, expected: &LinuxSandboxAppliedLaunchCommitments) -> bool {
        self.owned_child_pid > 0
            && self.namespace_pid == 1
            && self.effective_uid == 1
            && self.effective_gid == 1
            && self.no_new_privs
            && self.seccomp_mode == 2
            && self.executable_sha256 == expected.executable_sha256
            && self.argv_sha256 == expected.argv_sha256
            && self.environment_sha256 == expected.environment_sha256
            && self.cwd_sha256 == expected.cwd_sha256
    }

    pub fn matches_post_release_mounts(
        &self,
        expected: &LinuxSandboxMountPreparationCommitments,
    ) -> bool {
        self.post_release_mount_view == *expected
    }
}

/// Terminal writer exclusion from an owned native sandbox namespace.
///
/// This is not a live freeze, a process-scope replacement or a durable recovery
/// certificate. The trusted owner must already associate this exact process
/// with its candidate mounts. Only then may it enumerate/export those mounts.
/// A cloud occurrence still has its own independent cleanup obligation.
#[derive(Debug)]
pub struct LinuxSandboxTermination {
    exit: LinuxSandboxExit,
    launch_failure: Option<LinuxSandboxLaunchFailure>,
}

/// Terminal status observed for the exact released target without requesting
/// termination. The native target itself is namespace PID 1, not a wait shim.
/// A launch failure is distinct from a workload exit, even for code 125.
/// Namespace death excludes descendant writers; protocol/output draining is
/// still a separate obligation and is not performed by this observation.
#[derive(Debug)]
pub struct LinuxSandboxTargetExit {
    termination: LinuxSandboxTermination,
}

impl LinuxSandboxTargetExit {
    pub fn exit(&self) -> LinuxSandboxExit {
        self.termination.exit()
    }

    pub fn launch_failure(&self) -> Option<&LinuxSandboxLaunchFailure> {
        self.termination.launch_failure()
    }

    /// Transfer the already-established namespace writer-exclusion proof;
    /// there is no second signal or reap after natural target completion.
    pub fn into_termination(self) -> LinuxSandboxTermination {
        self.termination
    }
}

/// Trusted evidence that Lillux failed before the requested workload existed.
///
/// This must not be inferred from an exit code: a real workload may itself
/// return any code, including Lillux's private child-failure sentinel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxLaunchFailure {
    diagnostic: String,
}

impl LinuxSandboxTermination {
    pub fn exit(&self) -> LinuxSandboxExit {
        self.exit
    }

    pub fn launch_failure(&self) -> Option<&LinuxSandboxLaunchFailure> {
        self.launch_failure.as_ref()
    }
}

impl LinuxSandboxLaunchFailure {
    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }
}

/// Native target prepared through readiness, but held before exec until the
/// exact owner releases it. Like native launch, construction changes the
/// calling process's namespaces and belongs in a dedicated launcher process.
/// It is not safe to call from a daemon's shared async worker.
pub struct HeldLinuxSandboxProcess {
    process: LinuxSandboxProcess,
    #[cfg(target_os = "linux")]
    release: Option<std::fs::File>,
    #[cfg(target_os = "linux")]
    release_succeeded: bool,
}

/// Protected supervisor ends of candidate-only protocol pipes. They carry no
/// activation or bootstrap material. Ends are nonblocking so a finite relay
/// can enforce cancellation/backpressure without blocking on the candidate.
pub struct LinuxSandboxPipes {
    pub stdin: std::fs::File,
    pub stdout: std::fs::File,
    pub stderr: std::fs::File,
}

pub fn prepare_linux_sandbox_piped(
    request: LinuxSandboxRequest,
) -> Result<(HeldLinuxSandboxProcess, LinuxSandboxPipes), String> {
    if !request.target_channels.is_empty() {
        return Err("piped candidate cannot inherit extra target channels".into());
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        fn pipe() -> Result<(std::fs::File, std::fs::File), String> {
            let mut fds = [-1; 2];
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                return Err(format!(
                    "create candidate protocol pipe: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if let Err(error) = imp::relocate_internal_descriptors(
                &mut fds,
                &std::collections::BTreeSet::from([0, 1, 2]),
                "candidate protocol pipe",
            ) {
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(error);
            }
            Ok(unsafe {
                (
                    std::fs::File::from_raw_fd(fds[0]),
                    std::fs::File::from_raw_fd(fds[1]),
                )
            })
        }
        let (input, stdin) = pipe()?;
        let (stdout, output) = pipe()?;
        let (stderr, error) = pipe()?;
        for file in [&stdin, &stdout, &stderr] {
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                    < 0
            {
                return Err(format!(
                    "set candidate relay nonblocking: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        let process = prepare_linux_sandbox_inner(request, Some(&[input, output, error]))?;
        Ok((
            process,
            LinuxSandboxPipes {
                stdin,
                stdout,
                stderr,
            },
        ))
    }
    #[cfg(not(target_os = "linux"))]
    Err("piped native sandbox is unavailable on this platform".into())
}

impl HeldLinuxSandboxProcess {
    /// Poll the exact child-only applied-launch channel. `None` means the
    /// child has not reached the pre-exec boundary. EOF without a complete
    /// record is refusal, including a child killed before reaching it.
    pub fn try_observe_applied_launch(
        &mut self,
    ) -> Result<Option<LinuxSandboxAppliedLaunchReceipt>, String> {
        if !self.release_succeeded {
            return Err("native target has not been successfully released".into());
        }
        self.process.try_observe_applied_launch()
    }

    pub fn release_once(&mut self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            use std::io::Write as _;
            // Consume authority before writing: even an ambiguous write error
            // cannot authorize a second release attempt.
            let mut release = self
                .release
                .take()
                .ok_or("native target release was already consumed")?;
            release
                .write_all(&[1])
                .map_err(|error| format!("release held native target: {error}"))?;
            self.release_succeeded = true;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        Err("held native sandbox is unavailable on this platform".into())
    }

    /// Nonblocking observation of the actual target's terminal status. Pending
    /// returns `None` without changing ownership or signalling. A successful
    /// observation exclusively reaps once and consumes the live process handle.
    /// Refuses before successful release and after any termination request,
    /// including one whose settlement timed out.
    pub fn try_observe_target_exit(&mut self) -> Result<Option<LinuxSandboxTargetExit>, String> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            if !self.release_succeeded {
                return Err("native target has not been successfully released".into());
            }
            if self.process.termination_requested {
                return Err("native target completion cannot follow a termination request".into());
            }
            let lifetime = self
                .process
                .namespace_lifetime
                .as_ref()
                .filter(|_| self.process.pid > 0)
                .ok_or("sandbox handle is not a live owned namespace-init authority")?;
            let mut descriptor = libc::pollfd {
                fd: lifetime.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
            if ready == 0 {
                return Ok(None);
            }
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    return Ok(None);
                }
                return Err(format!("observe native target exit: {error}"));
            }
            if descriptor.revents & libc::POLLIN == 0 {
                return Err("sandbox namespace did not provide exact exit readiness".into());
            }
            self.process.reap_ready_namespace().map(|termination| {
                termination.map(|termination| LinuxSandboxTargetExit { termination })
            })
        }
        #[cfg(not(target_os = "linux"))]
        Err("native target observation is unavailable on this platform".into())
    }

    pub fn terminate_namespace_for_export(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<LinuxSandboxTermination, String> {
        if timeout.is_zero() || timeout > std::time::Duration::from_secs(60) {
            return Err("sandbox termination requires a timeout within (0,60s]".into());
        }
        #[cfg(target_os = "linux")]
        {
            self.release = None;
        }
        self.process.terminate_namespace_for_export(timeout)
    }

    /// Terminate beneath one caller-owned absolute deadline while keeping the
    /// native settlement-probe ceiling inside Lillux. Callers select lifecycle
    /// policy; they do not duplicate host wait limits or clock arithmetic.
    pub fn terminate_namespace_for_export_until(
        &mut self,
        deadline: crate::time::MonotonicDeadline,
    ) -> Result<LinuxSandboxTermination, String> {
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return Err("sandbox termination deadline already elapsed".into());
        }
        self.terminate_namespace_for_export(remaining.min(std::time::Duration::from_secs(60)))
    }
}

/// Construct the release channel inside Lillux, never from workload parameters.
pub fn prepare_linux_sandbox(
    request: LinuxSandboxRequest,
) -> Result<HeldLinuxSandboxProcess, String> {
    prepare_linux_sandbox_inner(request, None)
}

fn prepare_linux_sandbox_inner(
    mut request: LinuxSandboxRequest,
    stdio: Option<&[std::fs::File; 3]>,
) -> Result<HeldLinuxSandboxProcess, String> {
    if request.lifecycle != LinuxSandboxLifecycle::Run {
        return Err("held native preparation owns its release channel".into());
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        let mut reserved = request
            .target_channels
            .iter()
            .map(|(_, target)| i32::try_from(*target))
            .collect::<Result<std::collections::BTreeSet<_>, _>>()
            .map_err(|error| error.to_string())?;
        if stdio.is_some() {
            reserved.extend([0, 1, 2]);
        }
        let mut descriptors = [-1; 2];
        if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!(
                "create native release pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        if let Err(error) =
            imp::relocate_internal_descriptors(&mut descriptors, &reserved, "native release pipe")
        {
            unsafe {
                libc::close(descriptors[0]);
                libc::close(descriptors[1]);
            }
            return Err(error);
        }
        // SAFETY: pipe2/relocation returned distinct newly owned descriptors.
        let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
        let writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
        request.lifecycle = LinuxSandboxLifecycle::AwaitRelease {
            release_fd: u32::try_from(reader.as_raw_fd()).map_err(|error| error.to_string())?,
            release_keepalive_fd: u32::try_from(writer.as_raw_fd())
                .map_err(|error| error.to_string())?,
        };
        let mut process = imp::launch_with_stdio(request, stdio)?;
        // Only held execution requests the stronger terminal-export primitive.
        // Ordinary native launch retains its existing capability floor. The
        // child remains blocked on release while its exact lifetime is pinned.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0u32) };
        if fd < 0 {
            return Err(format!(
                "pin held namespace lifetime: {}",
                std::io::Error::last_os_error()
            ));
        }
        process.namespace_lifetime = Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) });
        drop(reader);
        Ok(HeldLinuxSandboxProcess {
            process,
            release: Some(writer),
            release_succeeded: false,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = &mut request;
        Err("held native sandbox is unavailable on this platform".into())
    }
}

impl LinuxSandboxProcess {
    /// Return the exact child-origin final-root preparation observation. This
    /// precedes release; it must not be presented as applied-exec evidence.
    pub fn mount_preparation_receipt(&self) -> Result<LinuxSandboxMountPreparationReceipt, String> {
        #[cfg(target_os = "linux")]
        {
            self.mount_preparation
                .clone()
                .ok_or("sandbox target has no final-root mount preparation receipt".into())
        }
        #[cfg(not(target_os = "linux"))]
        Err("native mount preparation is unavailable on this platform".into())
    }

    /// Poll a bounded post-chdir, pre-exec observation from this exact owned
    /// native target. Ordinary launch needs no separate release; held launch
    /// must be polled through the held wrapper after release.
    pub fn try_observe_applied_launch(
        &mut self,
    ) -> Result<Option<LinuxSandboxAppliedLaunchReceipt>, String> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            if let Some(receipt) = &self.observed_applied_launch {
                return Ok(Some(receipt.clone()));
            }
            if self.termination_requested {
                return Err("sandbox applied-launch cannot follow a termination request".into());
            }
            let channel = self
                .applied_launch
                .as_ref()
                .ok_or("sandbox applied-launch channel was already consumed")?;
            if self.pid <= 0 {
                return Err("sandbox applied-launch requires its exact unreaped child".into());
            }
            let mut descriptor = libc::pollfd {
                fd: channel.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
            if ready == 0 {
                return Ok(None);
            }
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    return Ok(None);
                }
                return Err(format!("poll sandbox applied-launch channel: {error}"));
            }
            if descriptor.revents & (libc::POLLIN | libc::POLLHUP) == 0 {
                return Err("sandbox applied-launch channel has invalid readiness".into());
            }
            let mut receipt = imp::read_applied_launch(channel.as_raw_fd())?;
            receipt.owned_child_pid = self.pid as u32;
            self.applied_launch = None;
            self.observed_applied_launch = Some(receipt.clone());
            Ok(Some(receipt))
        }
        #[cfg(not(target_os = "linux"))]
        Err("native applied-launch observation is unavailable on this platform".into())
    }

    /// End the complete namespace writer domain before terminal export.
    ///
    /// Native launch itself proved this child was namespace PID 1. Linux tears
    /// down the namespace's descendants before publishing PID-1 exit. Observe
    /// that exact pinned lifetime, then reap the owned child. No PID scan,
    /// process-group membership, signal acknowledgement or elapsed timer can
    /// produce the returned proof. On timeout this handle retains ownership.
    pub fn terminate_namespace_for_export(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<LinuxSandboxTermination, String> {
        if timeout.is_zero() || timeout > std::time::Duration::from_secs(60) {
            return Err("sandbox termination requires a timeout within (0,60s]".into());
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            let lifetime = self
                .namespace_lifetime
                .as_ref()
                .filter(|_| self.pid > 0)
                .ok_or("sandbox handle is not a live owned namespace-init authority")?;
            let fd = lifetime.as_raw_fd();
            let deadline = std::time::Instant::now()
                .checked_add(timeout)
                .ok_or("sandbox termination deadline overflow")?;
            self.termination_requested = true;
            let signalled = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd,
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                )
            };
            if signalled != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(format!("terminate exact sandbox namespace: {error}"));
                }
            }
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err("sandbox namespace termination remains unproved at deadline".into());
                }
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let milliseconds = i32::try_from(remaining.as_millis().max(1))
                    .map_err(|_| "sandbox termination poll bound overflow")?;
                let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
                if ready == 0 {
                    continue;
                }
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(format!("observe sandbox namespace exit: {error}"));
                }
                if descriptor.revents & libc::POLLIN == 0 {
                    return Err("sandbox namespace did not provide exact exit readiness".into());
                }
                if let Some(termination) = self.reap_ready_namespace()? {
                    return Ok(termination);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        Err("native sandbox namespace termination is unavailable on this platform".into())
    }

    /// Only called after pidfd readiness for this exact owned PID-1 child.
    /// After its reap the kernel has also ended descendants, so the private
    /// launch-failure pipe has no surviving writer and its bounded read cannot
    /// wait for workload output. EINTR retains authority for a later attempt.
    #[cfg(target_os = "linux")]
    fn reap_ready_namespace(&mut self) -> Result<Option<LinuxSandboxTermination>, String> {
        if self.pid <= 0 || self.namespace_lifetime.is_none() {
            return Err("sandbox handle is not a live owned namespace-init authority".into());
        }
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if reaped < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            return Ok(None);
        }
        if reaped != self.pid {
            return Err("sandbox namespace exit lacks exclusive child-reap evidence".into());
        }
        self.pid = 0;
        self.namespace_lifetime = None;
        let exit = if libc::WIFEXITED(status) {
            LinuxSandboxExit::Code(libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            LinuxSandboxExit::Signal(libc::WTERMSIG(status))
        } else {
            return Err("sandbox namespace has an unsupported terminal status".into());
        };
        let launch_failure = imp::read_launch_failure(self.launch_failure.take())?;
        Ok(Some(LinuxSandboxTermination {
            exit,
            launch_failure,
        }))
    }

    pub fn child_pid(&self) -> u32 {
        #[cfg(target_os = "linux")]
        {
            self.pid as u32
        }
        #[cfg(not(target_os = "linux"))]
        {
            0
        }
    }

    pub fn wait(mut self) -> Result<LinuxSandboxExit, String> {
        #[cfg(target_os = "linux")]
        {
            if self.pid <= 0 {
                return Err("sandbox process has already been reaped".into());
            }
            let mut status = 0;
            loop {
                let waited = unsafe { libc::waitpid(self.pid, &mut status, 0) };
                if waited == self.pid {
                    self.pid = 0;
                    if libc::WIFEXITED(status) {
                        let exit = LinuxSandboxExit::Code(libc::WEXITSTATUS(status));
                        if let Some(failure) = imp::read_launch_failure(self.launch_failure.take())?
                        {
                            return Err(format!(
                                "sandbox target was not executed: {}",
                                failure.diagnostic()
                            ));
                        }
                        return Ok(exit);
                    }
                    if libc::WIFSIGNALED(status) {
                        let exit = LinuxSandboxExit::Signal(libc::WTERMSIG(status));
                        if let Some(failure) = imp::read_launch_failure(self.launch_failure.take())?
                        {
                            return Err(format!(
                                "sandbox target was not executed: {}",
                                failure.diagnostic()
                            ));
                        }
                        return Ok(exit);
                    }
                    return Err("sandbox target returned an unsupported wait status".to_string());
                }
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINTR) {
                    return Err(format!("wait for sandbox target: {error}"));
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = &mut self;
            Err("Linux sandbox processes are unavailable on this platform".to_string())
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxSandboxProcess {
    fn drop(&mut self) {
        if self.pid > 0 {
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
                loop {
                    let result = libc::waitpid(self.pid, std::ptr::null_mut(), 0);
                    if result >= 0
                        || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                    {
                        break;
                    }
                }
            }
            self.pid = 0;
        }
    }
}

/// Generic overlay mutation observed below an exact upper directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxOverlayMutationKind {
    UpsertRegular {
        normalized_mode: u32,
        size: u64,
        sha256: String,
    },
    UpsertSymlink {
        target: String,
    },
    DeletePath,
    EnsureDirectory,
    OpaqueDirectory,
}

/// Portable retained-output link bound, applied before allocating its target.
pub const MAX_LINUX_OVERLAY_SYMLINK_TARGET_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxOverlayMutation {
    pub path: String,
    pub kind: LinuxOverlayMutationKind,
}

/// Exact roots and optional upper-layer mutations for one overlay workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxOverlayWorkspaceObservation {
    pub project_identity: String,
    pub state_identity: String,
    /// Exact child of the retained state authority containing every regular
    /// byte named by `mutations`. Present only for Observe.
    pub mutation_content_root: Option<String>,
    pub mutations: Vec<LinuxOverlayMutation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxOverlayWorkspaceOperation {
    Create,
    Observe,
    Destroy,
}

/// Inspect whether the current Linux kernel permits the complete native
/// confinement floor. Inspection occurs in a throwaway child, never mutating
/// the caller's namespaces.
pub fn inspect_linux_sandbox() -> Result<LinuxSandboxInspection, String> {
    imp::inspect()
}

/// Enter a private execution view and create a held final target.
///
/// On success the calling process is itself inside the new user/mount/IPC/UTS
/// namespace and private root. This API is intended for a dedicated adapter
/// process, not an application daemon.
pub fn launch_linux_sandbox(request: LinuxSandboxRequest) -> Result<LinuxSandboxProcess, String> {
    imp::launch(request)
}

/// Launch a held isolated target with one exact namespace-local listener.
/// Transfer the registered socket to the trusted controller before the target
/// can be released. A failed transfer drops and kills the owned target.
pub fn launch_linux_sandbox_with_loopback_ingress(
    request: LinuxSandboxRequest,
    address: std::net::SocketAddr,
    transfer_fd: u32,
    payload: &[u8],
    deadline: crate::time::MonotonicDeadline,
) -> Result<LinuxSandboxProcess, String> {
    if request.network != LinuxSandboxNetwork::Isolated
        || !matches!(
            request.lifecycle,
            LinuxSandboxLifecycle::AwaitRelease { .. }
        )
        || !matches!(address, std::net::SocketAddr::V4(v4) if v4.ip().is_loopback() && v4.port() != 0)
        || payload.is_empty()
    {
        return Err(
            "isolated loopback ingress requires a held isolated target and exact IPv4 endpoint"
                .into(),
        );
    }
    imp::launch_with_loopback_ingress(request, address, transfer_fd, payload, deadline)
}

/// Operate on exact inherited overlay-workspace descriptors without reopening
/// an ambient path.
pub fn operate_linux_overlay_workspace(
    project_fd: u32,
    state_fd: u32,
    operation: LinuxOverlayWorkspaceOperation,
    max_mutations: usize,
) -> Result<LinuxOverlayWorkspaceObservation, String> {
    imp::workspace(project_fd, state_fd, operation, max_mutations)
}

/// Create exactly one detached overlay in a dedicated trusted creator process.
///
/// This changes the calling process's user and mount namespaces. Like
/// [`launch_linux_sandbox`], it must not run inside a multithreaded daemon.
/// The caller supplies prepared, pinned lower and backend-state directories;
/// the latter must already contain the exact `upper` and `work` directories.
/// Namespace reanchoring re-proves those held objects before creating the
/// filesystem. No host-visible merged mountpoint or keeper process is needed.
/// Unsupported kernel mechanics fail; there is no legacy mount alternative.
/// An error after namespace entry leaves this creator partially transitioned,
/// and filesystem creation may already have changed backend state. The caller
/// must exit/reconcile that exact invocation, not reuse the process or assume
/// the prepared directories were untouched.
pub fn create_linux_overlay_template(
    project_fd: u32,
    state_fd: u32,
) -> Result<LinuxOverlayTemplate, String> {
    imp::create_overlay_template(project_fd, state_fd)
}

/// Write a bounded protocol payload to an inherited descriptor without
/// transferring raw descriptor ownership to the caller.
pub fn write_inherited_descriptor(fd: u32, bytes: &[u8]) -> Result<(), String> {
    imp::write_descriptor(fd, bytes)
}

/// Read one exact sealed inherited memfd with an explicit byte ceiling.
pub fn read_sealed_inherited_descriptor(fd: u32, max_bytes: usize) -> Result<Vec<u8>, String> {
    imp::read_sealed_descriptor(fd, max_bytes)
}

/// Consume, own and read one sealed inherited descriptor named by process-
/// environment transport. Descriptor parsing, environment mutation, sealing
/// validation and close-on-drop all remain inside Lillux.
///
/// # Safety
/// The trusted parent must have installed the named descriptor exactly once
/// for this process, with no pre-existing Rust owner in the child.
/// Call during single-threaded startup before concurrent environment access,
/// including access by foreign library threads; this consumes the named slot.
pub unsafe fn read_sealed_inherited_descriptor_from_env(
    name: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    // SAFETY: forwarded from this function's unique child-ownership contract.
    let authority = unsafe { crate::take_inherited_descriptor_authority_from_env(name) }?;
    let descriptor = authority.inherited_descriptor()?;
    read_sealed_inherited_descriptor(descriptor, max_bytes)
}

/// Prove an inherited descriptor identifies the executable image currently
/// running this dedicated adapter process.
pub fn validate_current_executable_descriptor(fd: u32) -> Result<(), String> {
    imp::validate_current_executable(fd)
}

/// Prove an inherited descriptor is one connected Unix stream socket.
pub fn validate_connected_unix_stream_descriptor(fd: u32) -> Result<(), String> {
    imp::validate_connected_unix_stream(fd)
}

/// Exit using the exact child outcome without exposing platform process APIs
/// to protocol adapters.
pub fn exit_with_linux_sandbox_status(status: LinuxSandboxExit) -> ! {
    imp::exit_with_status(status)
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub fn take_overlay_template(_fd: u32) -> Result<LinuxOverlayTemplate, String> {
        Err("detached Linux overlay templates are unavailable on this platform".to_string())
    }

    pub fn create_overlay_template(
        _project_fd: u32,
        _state_fd: u32,
    ) -> Result<LinuxOverlayTemplate, String> {
        Err("detached Linux overlay templates are unavailable on this platform".to_string())
    }

    pub fn validate_overlay_template(
        _authority: &crate::InheritedDescriptorAuthority,
    ) -> Result<(), String> {
        Err("detached Linux overlay templates are unavailable on this platform".to_string())
    }

    pub fn inspect() -> Result<LinuxSandboxInspection, String> {
        Err("native Linux sandboxing is unavailable on this platform".to_string())
    }

    pub fn launch(_request: LinuxSandboxRequest) -> Result<LinuxSandboxProcess, String> {
        Err("native Linux sandboxing is unavailable on this platform".to_string())
    }

    pub fn launch_with_loopback_ingress(
        _request: LinuxSandboxRequest,
        _address: std::net::SocketAddr,
        _transfer_fd: u32,
        _payload: &[u8],
        _deadline: crate::time::MonotonicDeadline,
    ) -> Result<LinuxSandboxProcess, String> {
        Err("isolated loopback ingress is unavailable on this platform".to_string())
    }

    pub fn workspace(
        _project_fd: u32,
        _state_fd: u32,
        _operation: LinuxOverlayWorkspaceOperation,
        _max_mutations: usize,
    ) -> Result<LinuxOverlayWorkspaceObservation, String> {
        Err("native Linux overlay workspaces are unavailable on this platform".to_string())
    }

    pub fn write_descriptor(_fd: u32, _bytes: &[u8]) -> Result<(), String> {
        Err("inherited Unix descriptors are unavailable on this platform".to_string())
    }

    pub fn read_sealed_descriptor(_fd: u32, _max_bytes: usize) -> Result<Vec<u8>, String> {
        Err("sealed inherited descriptors are unavailable on this platform".to_string())
    }

    pub fn validate_current_executable(_fd: u32) -> Result<(), String> {
        Err("executable descriptors are unavailable on this platform".to_string())
    }

    pub fn validate_connected_unix_stream(_fd: u32) -> Result<(), String> {
        Err("Unix stream descriptors are unavailable on this platform".to_string())
    }

    pub fn exit_with_status(_status: LinuxSandboxExit) -> ! {
        std::process::exit(125)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use sha2::{Digest as _, Sha256};
    use std::ffi::{CStr, CString, OsStr};
    use std::fs::File;
    use std::io::{Read as _, Seek as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
    use std::os::unix::ffi::OsStrExt as _;

    mod detached_overlay;
    mod fixed_parents;

    pub(super) use detached_overlay::{
        create_overlay_template, take_overlay_template, validate_overlay_template,
    };

    const ROOT: &str = "/tmp";
    const OLD_ROOT: &str = "/tmp/.lillux-old-root";
    const SEALED_STAGING_NAME: &str = ".lillux-sealed-staging";
    const CHILD_READY: u8 = 0;
    const CHILD_TARGET_READY: u8 = 2;
    const MOUNT_PREPARATION_SCHEMA: u8 = 1;
    const MOUNT_PREPARATION_RECORD_BYTES: usize = 1 + 1 + 4 + 32;
    // Wire cut: the applied record now includes a post-release mount view.
    const APPLIED_LAUNCH_RECORD: u8 = 0xA8;
    const APPLIED_LAUNCH_RECORD_BYTES: usize = 1 + 4 * 7 + 32 * 5;
    pub(super) const CHILD_ERROR: u8 = 1;
    pub(super) const MAX_CHILD_ERROR_BYTES: usize = 64 * 1024;
    // The private failure channel is drained only after the exact child exits.
    // Its sole record must fit in an otherwise empty Linux pipe without a
    // concurrent reader, including the kind and length prefix. The readiness
    // error uses the same bound because it is written after this record.
    pub(super) const MAX_LAUNCH_FAILURE_BYTES: usize = libc::PIPE_BUF - 5;
    const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
    const MOUNT_ATTR_NOSUID: u64 = 0x0000_0002;
    const MOUNT_ATTR_NODEV: u64 = 0x0000_0004;
    const AT_RECURSIVE: u32 = 0x8000;
    const OPEN_TREE_CLONE: libc::c_uint = 0x0000_0001;
    const OPEN_TREE_CLOEXEC: libc::c_uint = libc::O_CLOEXEC as libc::c_uint;
    const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x0000_0004;
    const MOVE_MOUNT_T_EMPTY_PATH: libc::c_uint = 0x0000_0040;

    #[repr(C)]
    struct MountAttr {
        attr_set: u64,
        attr_clr: u64,
        propagation: u64,
        userns_fd: u64,
    }

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;

    pub fn inspect() -> Result<LinuxSandboxInspection, String> {
        // Real launches inherit authorities from before CLONE_NEWNS. A probe
        // that opens every source afterwards misses the kernel's rejection of
        // bind mounts belonging to the former mount namespace.
        let inherited_directory =
            crate::secure_fs::pin_canonical_mount_source(std::path::Path::new(ROOT))
                .map_err(|error| format!("pin inherited namespace probe: {error}"))?;
        let inherited_bytes = crate::sealed_memfd(c"lillux-mount-probe", b"exact sealed bytes")?;
        let mut available = LinuxSandboxInspection::declared_native_contract();
        for nested in [false, true] {
            if nested && unsafe { libc::geteuid() } == 0 {
                available.nested_sandbox = false;
                continue;
            }
            let mut report = [0; 2];
            syscall_zero(
                unsafe { libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC) },
                "create sandbox probe report",
            )?;
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                close_fd(report[0]);
                close_fd(report[1]);
                return Err(format!(
                    "fork native sandbox inspection: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if pid == 0 {
                close_fd(report[0]);
                let result = (|| {
                    // The shared-view probe reaps its creator before starting
                    // sibling borrowers. Run before CLONE_NEWPID makes that first
                    // child PID 1 of our pending namespace; its exit would make
                    // subsequent forks fail instead of probing view reuse.
                    probe_overlay()?;
                    enter_namespaces(LinuxSandboxNetwork::Isolated)?;
                    let directory = reanchor_mount_source(inherited_directory.file().as_raw_fd())?;
                    mount_private_root()?;
                    create_minimal_devices(nested)?;
                    create_private_tmp()?;
                    let staging = SealedSourceStaging::create()?;
                    let bytes = materialize_sealed_mount_source(
                        inherited_bytes.file().as_raw_fd(),
                        &staging.content,
                    )?;
                    probe_inherited_descriptor_mounts(directory.file(), bytes.file())?;
                    staging.detach()?;
                    probe_descriptor_mount()?;
                    fixed_parents::probe()?;
                    probe_isolated_pid_child(nested)?;
                    Ok::<(), String>(())
                })();
                match &result {
                    Ok(()) => {
                        let _ = write_all_fd(report[1], &[CHILD_READY]);
                    }
                    Err(error) => {
                        let _ = write_child_error(report[1], error);
                    }
                }
                unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
            }
            close_fd(report[1]);
            let outcome = read_child_ready(report[0]);
            close_fd(report[0]);
            let mut status = 0;
            if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
                return Err(format!(
                    "wait for native sandbox inspection: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
                if nested {
                    available.nested_sandbox = false;
                    continue;
                }
                return Err(format!(
                    "native sandbox kernel probe refused: {}",
                    outcome.err().unwrap_or_else(|| {
                        "probe child terminated without a failure report".to_string()
                    })
                ));
            }
            if nested && outcome.is_err() {
                available.nested_sandbox = false;
            } else {
                outcome?;
            }
        }
        Ok(available)
    }

    pub fn launch(request: LinuxSandboxRequest) -> Result<LinuxSandboxProcess, String> {
        launch_with_stdio(request, None)
    }

    pub fn launch_with_loopback_ingress(
        request: LinuxSandboxRequest,
        address: std::net::SocketAddr,
        transfer_fd: u32,
        payload: &[u8],
        deadline: crate::time::MonotonicDeadline,
    ) -> Result<LinuxSandboxProcess, String> {
        if deadline.has_elapsed() {
            return Err("isolated loopback transfer deadline elapsed before launch".into());
        }
        // SAFETY: the dedicated adapter owns the inherited transfer endpoint
        // uniquely. The signed launch request supplies its exact coordinate.
        let sender = unsafe { crate::take_inherited_descriptor_transfer_sender(transfer_fd) }
            .map_err(|error| format!("adopt loopback transfer endpoint: {error}"))?;
        let (process, source) = launch_with_stdio_and_loopback(request, None, Some(address))?;
        let source = source.ok_or("isolated loopback listener was not created")?;
        source
            .transfer(sender, payload, deadline)
            .map_err(|error| format!("transfer isolated loopback listener: {error}"))?;
        Ok(process)
    }

    pub(super) fn launch_with_stdio(
        request: LinuxSandboxRequest,
        stdio: Option<&[std::fs::File; 3]>,
    ) -> Result<LinuxSandboxProcess, String> {
        launch_with_stdio_and_loopback(request, stdio, None).map(|(process, _)| process)
    }

    fn launch_with_stdio_and_loopback(
        mut request: LinuxSandboxRequest,
        stdio: Option<&[std::fs::File; 3]>,
        ingress: Option<std::net::SocketAddr>,
    ) -> Result<
        (
            LinuxSandboxProcess,
            Option<crate::loopback::LoopbackListenerTransferSource>,
        ),
        String,
    > {
        validate_request(&request)?;
        if request.aggregate_limits.is_some() {
            return Err(
                "aggregate resource isolation requires a delegated cgroup-v2 authority".to_string(),
            );
        }
        enter_namespaces(request.network)?;
        let loopback_source = if let Some(address) = ingress {
            if request.network != LinuxSandboxNetwork::Isolated {
                return Err("loopback ingress requires a new isolated network namespace".into());
            }
            crate::loopback::activate_loopback_in_current_namespace()
                .map_err(|error| format!("activate isolated loopback: {error}"))?;
            Some(
                crate::loopback::LoopbackListenerTransferSource::bind_exact(address)
                    .map_err(|error| format!("bind isolated loopback listener: {error}"))?,
            )
        } else {
            None
        };
        // Re-prove the same inherited filesystem objects in the cloned
        // namespace before the private root hides /tmp. The retained original
        // descriptors remain the identity authority, never a caller pathname.
        let mut sources = reanchor_request_sources(&request)?;
        mount_private_root()?;
        let sealed_staging = request
            .mounts
            .iter()
            .any(|mount| !sources.contains_key(&mount.source_fd))
            .then(SealedSourceStaging::create)
            .transpose()?;
        for mount in &request.mounts {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                sources.entry(mount.source_fd)
            {
                if mount.access != LinuxSandboxMountAccess::ReadOnly {
                    return Err("sealed mount source cannot grant writable access".to_string());
                }
                let staging = sealed_staging
                    .as_ref()
                    .ok_or_else(|| "sealed source lacks private staging authority".to_string())?;
                entry.insert(materialize_sealed_mount_source(
                    raw_fd(mount.source_fd)?,
                    &staging.content,
                )?);
            }
        }
        for mount in &mut request.mounts {
            mount.source_fd = sources[&mount.source_fd].inherited_descriptor()?;
        }
        // The detached template is NOT an ordinary mount source: reanchoring
        // it would replace the admitted filesystem with unrelated path bytes.
        if request.minimal_devices {
            create_minimal_devices(request.nested_sandbox)?;
        }
        for device in &request.character_devices {
            attach_character_device(device)?;
        }
        if request.private_tmp {
            create_private_tmp()?;
        }
        if let Some(overlay) = &request.overlay {
            create_directory_target(&rooted(&overlay.destination)?)?;
            mount_overlay(overlay)?;
        }
        let mut mounts = request.mounts.clone();
        let mut overlay_descendant_sources = Vec::new();
        if let Some(overlay) = &request.overlay {
            for descendant in &overlay.writable_descendant_mounts {
                let source = reanchor_overlay_descendant_source(&overlay.destination, descendant)?;
                mounts.push(LinuxSandboxMount {
                    source_fd: source.inherited_descriptor()?,
                    destination: descendant.destination.clone(),
                    access: LinuxSandboxMountAccess::Writable,
                    layer: 10,
                });
                overlay_descendant_sources.push(source);
            }
        }
        mounts.sort_by(|left, right| {
            left.layer
                .cmp(&right.layer)
                .then_with(|| left.destination.cmp(&right.destination))
        });
        for (index, mount) in mounts.iter().enumerate() {
            prepare_descriptor_mount_target(
                &mount.destination,
                descriptor_kind(mount.source_fd)?,
                request.overlay.as_ref().map(|overlay| &overlay.destination),
                &mounts[..index],
            )
            .map_err(|error| format!("prepare mount {}: {error}", mount.destination.display()))?;
            bind_descriptor_mount(mount)
                .map_err(|error| format!("mount {}: {error}", mount.destination.display()))?;
            verify_descriptor_mount(mount).map_err(|error| {
                format!("verify mounted {}: {error}", mount.destination.display())
            })?;
            if let Some(view) = request
                .fixed_parent_views
                .iter()
                .find(|view| view.destination == mount.destination)
            {
                fixed_parents::install(mount, view, index)?;
            }
        }
        if let Some(staging) = sealed_staging {
            staging.detach()?;
        }
        // Exact reopened descendants must remain live until their detached
        // bind mounts have been attached. They grant no authority afterward.
        drop(overlay_descendant_sources);
        let executable = rooted(&request.executable)?;
        ensure_regular_path(&executable, "sandbox executable")?;
        let cwd = rooted(&request.cwd)?;
        ensure_directory_path(&cwd, "sandbox cwd")?;
        spawn_target(request, stdio).map(|process| (process, loopback_source))
    }

    fn validate_request(request: &LinuxSandboxRequest) -> Result<(), String> {
        if request.nested_sandbox && unsafe { libc::geteuid() } == 0 {
            return Err(
                "nested sandbox requires an unprivileged launch identity, not node root".to_owned(),
            );
        }
        if request.nested_sandbox
            != (request.proc_filesystem == LinuxSandboxProcFilesystem::PidNamespaceNested)
            || (request.nested_sandbox && request.contain_process_group)
        {
            return Err(
                "nested sandbox requires external whole-execution containment and PID-local proc"
                    .to_owned(),
            );
        }
        const MAX_SANDBOX_MOUNTS: usize = 4096;
        let descendant_mount_count = request
            .overlay
            .as_ref()
            .map_or(0, |overlay| overlay.writable_descendant_mounts.len());
        if request
            .mounts
            .len()
            .checked_add(descendant_mount_count)
            .is_none_or(|count| count > MAX_SANDBOX_MOUNTS)
        {
            return Err("sandbox mount count exceeds its bound".to_string());
        }
        let staging_path = PathBuf::from(format!("/{SEALED_STAGING_NAME}"));
        if request
            .mounts
            .iter()
            .any(|mount| mount.destination.starts_with(&staging_path))
            || request.overlay.as_ref().is_some_and(|overlay| {
                overlay.destination.starts_with(&staging_path)
                    || staging_path.starts_with(&overlay.destination)
                    || overlay.writable_descendant_mounts.iter().any(|mount| {
                        mount.destination.starts_with(&staging_path)
                            || staging_path.starts_with(&mount.destination)
                    })
            })
        {
            return Err("mount conflicts with private sealed-source staging".to_string());
        }
        // Reserve the surface even for Empty; an admitted mount must not
        // silently replace that choice with a host procfs alias.
        {
            let proc_path = std::path::Path::new("/proc");
            if request
                .mounts
                .iter()
                .any(|mount| mount.destination.starts_with(proc_path))
                || request.overlay.as_ref().is_some_and(|overlay| {
                    overlay.destination.starts_with(proc_path)
                        || proc_path.starts_with(&overlay.destination)
                        || overlay.writable_descendant_mounts.iter().any(|mount| {
                            mount.destination.starts_with(proc_path)
                                || proc_path.starts_with(&mount.destination)
                        })
                })
            {
                return Err("mount conflicts with reserved PID procfs".to_string());
            }
        }
        validate_absolute_path(&request.executable, "sandbox executable")?;
        validate_absolute_path(&request.cwd, "sandbox cwd")?;
        if request.argv0.as_bytes().is_empty() || request.argv0.as_bytes().contains(&0) {
            return Err("sandbox argv0 is empty or contains NUL".to_string());
        }
        for argument in &request.arguments {
            if argument.as_bytes().contains(&0) {
                return Err("sandbox argument contains NUL".to_string());
            }
        }
        for (name, value) in &request.environment {
            if name.as_bytes().is_empty()
                || name.as_bytes().contains(&0)
                || name.as_bytes().contains(&b'=')
                || value.as_bytes().contains(&0)
            {
                return Err("sandbox environment contains an invalid entry".to_string());
            }
        }
        let mut destinations = BTreeSet::new();
        let mut descriptor_roles = BTreeSet::new();
        for mount in &request.mounts {
            validate_inherited_fd(mount.source_fd, "sandbox mount")?;
            descriptor_roles.insert(mount.source_fd);
            validate_absolute_path(&mount.destination, "sandbox mount destination")?;
            if mount.destination == PathBuf::from("/") {
                return Err("a descriptor mount cannot replace the private root".to_string());
            }
            if !destinations.insert(mount.destination.clone()) {
                return Err("sandbox mount destinations must be unique".to_string());
            }
        }
        for device in &request.character_devices {
            validate_inherited_fd(device.source_fd, "sandbox character device")?;
            if !descriptor_roles.insert(device.source_fd) {
                return Err("character-device descriptor aliases another authority role".to_owned());
            }
            validate_absolute_path(&device.destination, "sandbox character-device destination")?;
            if !device.destination.starts_with("/dev")
                || device.destination == PathBuf::from("/dev")
                || !destinations.insert(device.destination.clone())
            {
                return Err(
                    "sandbox character-device destination is invalid or duplicated".to_owned(),
                );
            }
            validate_character_device_descriptor(device)?;
        }
        fixed_parents::validate(request)?;
        if let Some(overlay) = &request.overlay {
            let fd = overlay
                .template
                .inherited_authority()
                .inherited_descriptor()?;
            validate_overlay_template(overlay.template.inherited_authority())?;
            if !descriptor_roles.insert(fd) {
                return Err(
                    "descriptor is aliased across mount and overlay authority roles".to_string(),
                );
            }
            validate_absolute_path(&overlay.destination, "sandbox overlay destination")?;
            if overlay.destination == PathBuf::from("/")
                || !destinations.insert(overlay.destination.clone())
            {
                return Err("sandbox overlay destination is invalid or duplicated".to_string());
            }
            let mut previous_destination: Option<&PathBuf> = None;
            let mut descendant_descriptors = BTreeSet::new();
            let mut descendant_destinations: Vec<&PathBuf> = Vec::new();
            for descendant in &overlay.writable_descendant_mounts {
                validate_inherited_fd(descendant.source_fd, "sandbox overlay-descendant mount")?;
                if descriptor_kind(descendant.source_fd)? != DescriptorKind::Directory {
                    return Err("sandbox overlay-descendant source must be a directory".to_string());
                }
                if !descendant_descriptors.insert(descendant.source_fd)
                    || !descriptor_roles.insert(descendant.source_fd)
                {
                    return Err(
                        "overlay-descendant descriptor aliases another authority role".to_string(),
                    );
                }
                validate_relative_path(
                    &descendant.relative_path,
                    "sandbox overlay-descendant source",
                )?;
                validate_absolute_path(
                    &descendant.destination,
                    "sandbox overlay-descendant destination",
                )?;
                if descendant.destination == PathBuf::from("/")
                    || previous_destination
                        .is_some_and(|previous| previous >= &descendant.destination)
                    || !destinations.insert(descendant.destination.clone())
                    || paths_overlap(&descendant.destination, &overlay.destination)
                    || request
                        .mounts
                        .iter()
                        .any(|mount| paths_overlap(&descendant.destination, &mount.destination))
                    || descendant_destinations
                        .iter()
                        .any(|other| paths_overlap(&descendant.destination, other))
                {
                    return Err(
                        "overlay-descendant destinations must be sorted, unique, and nonoverlapping with other mounts"
                            .to_string(),
                    );
                }
                previous_destination = Some(&descendant.destination);
                descendant_destinations.push(&descendant.destination);
            }
        }
        let mut previous_target = None;
        for (source, target) in &request.target_channels {
            let (source, target) = (*source, *target);
            validate_inherited_fd(source, "sandbox target channel")?;
            let _ = raw_fd(target)?;
            if !descriptor_roles.insert(source) {
                return Err("target-channel descriptor aliases filesystem authority".to_string());
            }
            if matches!(target, 1 | 2) || previous_target.is_some_and(|previous| previous >= target)
            {
                return Err(
                    "sandbox target channels must be uniquely target-fd sorted and cannot replace stdout/stderr"
                        .to_string(),
                );
            }
            previous_target = Some(target);
        }
        match request.lifecycle {
            LinuxSandboxLifecycle::Run => {}
            LinuxSandboxLifecycle::AwaitRelease {
                release_fd,
                release_keepalive_fd,
            } => {
                validate_inherited_fd(release_fd, "sandbox release reader")?;
                validate_inherited_fd(release_keepalive_fd, "sandbox release keepalive")?;
                if !descriptor_roles.insert(release_fd)
                    || !descriptor_roles.insert(release_keepalive_fd)
                {
                    return Err(
                        "sandbox release descriptors alias another authority role".to_string()
                    );
                }
            }
        }
        for (source, target) in &request.target_channels {
            if *target > 2 && target != source && descriptor_roles.contains(target) {
                return Err(
                    "sandbox target channel destination aliases an inherited authority descriptor"
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    // A private namespace coordinate, NOT a host account or project identity.
    // Only the invoking host UID/GID is mapped. Keep the inner coordinate
    // nonzero: after dropping setup capabilities, mapping parent UID 0 into a
    // nested user namespace would require retaining CAP_SETFCAP in the outer
    // namespace (Linux 5.12+). Ordinary nested sandboxing must not need that.
    const NAMESPACE_USER_ID: libc::uid_t = 1;
    const NAMESPACE_GROUP_ID: libc::gid_t = 1;

    fn enter_mapped_user_namespace() -> Result<(), String> {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        syscall_zero(
            unsafe { libc::unshare(libc::CLONE_NEWUSER) },
            "create user namespace",
        )?;
        write_proc_mapping("/proc/self/setgroups", "deny\n", true)?;
        write_proc_mapping(
            "/proc/self/uid_map",
            &format!("{NAMESPACE_USER_ID} {uid} 1\n"),
            false,
        )?;
        write_proc_mapping(
            "/proc/self/gid_map",
            &format!("{NAMESPACE_GROUP_ID} {gid} 1\n"),
            false,
        )?;
        syscall_zero(
            unsafe { libc::setresgid(NAMESPACE_GROUP_ID, NAMESPACE_GROUP_ID, NAMESPACE_GROUP_ID) },
            "enter mapped sandbox gid",
        )?;
        syscall_zero(
            unsafe { libc::setresuid(NAMESPACE_USER_ID, NAMESPACE_USER_ID, NAMESPACE_USER_ID) },
            "enter mapped sandbox uid",
        )
    }

    fn enter_namespaces(network: LinuxSandboxNetwork) -> Result<(), String> {
        enter_mapped_user_namespace()?;
        let mut flags =
            libc::CLONE_NEWNS | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS | libc::CLONE_NEWPID;
        if network == LinuxSandboxNetwork::Isolated {
            flags |= libc::CLONE_NEWNET;
        }
        syscall_zero(unsafe { libc::unshare(flags) }, "create sandbox namespaces")?;
        mount_raw(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)
            .map_err(|error| format!("make sandbox mount propagation private: {error}"))
    }

    fn write_proc_mapping(path: &str, value: &str, missing_is_ok: bool) -> Result<(), String> {
        match std::fs::write(path, value.as_bytes()) {
            Ok(()) => Ok(()),
            Err(error) if missing_is_ok && error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            Err(error) => Err(format!("write sandbox identity mapping {path}: {error}")),
        }
    }

    fn mount_private_root() -> Result<(), String> {
        mount_raw(
            Some("tmpfs"),
            ROOT,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=0755"),
        )
        .map_err(|error| format!("mount private sandbox root: {error}"))?;
        mkdir_one(&format!("{ROOT}/proc"), 0o555)
            .map_err(|error| format!("create private proc mountpoint: {error}"))?;
        mkdir_path(OLD_ROOT, 0o700)
    }

    // Keep the native backend aligned with the already-admitted mount-source
    // classes. Callback protocols may carry an exact filesystem Unix socket;
    // this does not grant a socket to callback-free or captured-only tools.
    use crate::secure_fs::OpenMountEntryKind as DescriptorKind;

    fn mount_source_stat(fd: RawFd) -> Result<libc::stat, String> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe { libc::fstat(fd, stat.as_mut_ptr()) },
            "inspect mount source",
        )?;
        Ok(unsafe { stat.assume_init() })
    }

    fn reanchor_mount_source(fd: RawFd) -> Result<crate::InheritedDescriptorAuthority, String> {
        let path = std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .map_err(|error| format!("locate inherited mount descriptor: {error}"))?;
        reanchor_mount_source_at(fd, &path)
    }

    fn reanchor_mount_source_at(
        fd: RawFd,
        path: &std::path::Path,
    ) -> Result<crate::InheritedDescriptorAuthority, String> {
        let expected = mount_source_stat(fd)?;
        let source = crate::secure_fs::pin_canonical_mount_source(path)
            .map_err(|error| format!("pin mount source in cloned namespace: {error}"))?;
        let observed = mount_source_stat(source.file().as_raw_fd())?;
        // open_tree cannot clone a vfsmount owned by the former namespace.
        // A kernel-reported path is only a locator: a replacement, symlink,
        // deleted object, or inaccessible source must fail, not authorize new
        // bytes. The original fd pins the inode against reuse throughout this
        // comparison; only this newly proven fd is subsequently mounted.
        if expected.st_dev != observed.st_dev
            || expected.st_ino != observed.st_ino
            || expected.st_mode & libc::S_IFMT != observed.st_mode & libc::S_IFMT
        {
            return Err("mount source changed across namespace transition".to_string());
        }
        Ok(source)
    }

    fn mount_source_is_sealed(fd: RawFd) -> Result<bool, String> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(format!(
                "inspect mount source flags: {}",
                std::io::Error::last_os_error()
            ));
        }
        if flags & libc::O_PATH != 0 {
            return Ok(false);
        }
        let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
        if seals < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                return Ok(false);
            }
            return Err(format!("inspect mount source seals: {error}"));
        }
        let required =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        Ok(seals & required == required)
    }

    fn reanchor_request_sources(
        request: &LinuxSandboxRequest,
    ) -> Result<BTreeMap<u32, crate::InheritedDescriptorAuthority>, String> {
        let descriptors = request
            .mounts
            .iter()
            .map(|mount| mount.source_fd)
            .collect::<BTreeSet<_>>();
        let mut sources = BTreeMap::new();
        for descriptor in descriptors {
            let fd = raw_fd(descriptor)?;
            if descriptor_kind(descriptor)? == DescriptorKind::Regular
                && mount_source_is_sealed(fd)?
            {
                // One source may have several destinations. Check every use
                // before deduplication: a preceding read-only alias must not
                // let a later writable alias reuse the private byte copy.
                if request.mounts.iter().any(|mount| {
                    mount.source_fd == descriptor
                        && mount.access != LinuxSandboxMountAccess::ReadOnly
                }) {
                    return Err("sealed mount source cannot grant writable access".to_string());
                }
                continue;
            }
            let path = std::fs::read_link(format!("/proc/self/fd/{fd}"))
                .map_err(|error| format!("locate inherited request source: {error}"))?;
            // A recursive directory clone of the construction root (or its
            // ancestor) would also clone our new private setup mounts. Those
            // are backend authority, never part of an admitted source. This
            // kernel locator is rejection evidence only; inode/type equality
            // below still grants the exact reanchored source authority.
            if descriptor_kind(descriptor)? == DescriptorKind::Directory
                && std::path::Path::new(ROOT).starts_with(&path)
            {
                return Err("directory source contains the sandbox construction root".to_string());
            }
            sources.insert(descriptor, reanchor_mount_source_at(fd, &path)?);
        }
        Ok(sources)
    }

    fn reanchor_overlay_descendant_source(
        overlay_destination: &PathBuf,
        descendant: &LinuxSandboxOverlayDescendantMount,
    ) -> Result<crate::InheritedDescriptorAuthority, String> {
        let mounted = open_mount_target_no_symlinks(overlay_destination)?;
        let mut current =
            crate::PinnedDirectory::from_open_directory(rooted(overlay_destination)?, mounted)
                .map_err(|error| format!("pin mounted overlay root: {error}"))?;
        for component in descendant.relative_path.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(
                    "sandbox overlay-descendant source must be a normalized relative path"
                        .to_string(),
                );
            };
            current = current
                .open_child_directory(name)
                .map_err(|error| format!("open mounted overlay descendant: {error}"))?
                .ok_or_else(|| "mounted overlay descendant is missing".to_string())?;
        }
        let expected = mount_source_stat(raw_fd(descendant.source_fd)?)?;
        let observed_descriptor = current
            .try_clone_descriptor()
            .map_err(|error| format!("inspect mounted overlay descendant: {error}"))?;
        let observed = mount_source_stat(observed_descriptor.as_raw_fd())?;
        if expected.st_dev != observed.st_dev
            || expected.st_ino != observed.st_ino
            || expected.st_mode & libc::S_IFMT != observed.st_mode & libc::S_IFMT
        {
            return Err(
                "overlay descendant changed between retained authority and mounted template"
                    .to_string(),
            );
        }
        current
            .into_inherited_descriptor_path()
            .map_err(|error| format!("retain mounted overlay descendant: {error}"))
    }

    struct SealedSourceStaging {
        root: crate::PinnedDirectory,
        mountpoint: crate::PinnedDirectory,
        content: crate::PinnedDirectory,
    }

    impl SealedSourceStaging {
        fn create() -> Result<Self, String> {
            let root = crate::PinnedDirectory::open(std::path::Path::new(ROOT))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "private materialization root is missing".to_string())?;
            let mountpoint = root
                .create_child(OsStr::new(SEALED_STAGING_NAME), 0o700)
                .map_err(|error| format!("create private sealed staging: {error}"))?;
            let path = root.path().join(SEALED_STAGING_NAME);
            mount_raw(
                Some("tmpfs"),
                path_string(&path)?,
                Some("tmpfs"),
                libc::MS_NOSUID | libc::MS_NODEV,
                Some("mode=0700"),
            )
            .map_err(|error| format!("mount private sealed staging: {error}"))?;
            let content = root
                .open_child_directory(OsStr::new(SEALED_STAGING_NAME))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "private sealed staging mount disappeared".to_string())?;
            Ok(Self {
                root,
                mountpoint,
                content,
            })
        }

        fn detach(self) -> Result<(), String> {
            let name = OsStr::new(SEALED_STAGING_NAME);
            let current = self
                .root
                .open_child_directory(name)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "sealed staging disappeared before detach".to_string())?;
            if !self
                .content
                .is_same_directory(&current)
                .map_err(|error| error.to_string())?
            {
                return Err("sealed staging mount identity changed".to_string());
            }
            // Do not unlink source files: that marks the executable dentry
            // deleted even through its admitted bind mount, breaking self
            // lookup. Detach the private filesystem instead. Only the exact
            // read-only target mounts keep these linked inodes alive.
            unmount_path(&self.root.path().join(name))?;
            if !self
                .root
                .remove_empty_child_if_same(name, &self.mountpoint)
                .map_err(|error| format!("remove detached sealed mountpoint: {error:#}"))?
            {
                return Err("detached sealed mountpoint is not empty".to_string());
            }
            Ok(())
        }
    }

    fn materialize_sealed_mount_source(
        fd: RawFd,
        root: &crate::PinnedDirectory,
    ) -> Result<crate::InheritedDescriptorAuthority, String> {
        use std::os::unix::fs::FileExt as _;

        let lease = crate::retain_fork_sensitive_descriptors();
        if !mount_source_is_sealed(fd)? {
            return Err("private byte materialization requires a sealed source".to_string());
        }
        let source = unsafe { File::from_raw_fd(duplicate_fd(fd)?) };
        let metadata = source
            .metadata()
            .map_err(|error| format!("inspect sealed mount: {error}"))?;
        if !metadata.is_file() {
            return Err("sealed mount source is not a regular file".to_string());
        }
        let name = OsString::from(format!(".lillux-sealed-source-{fd}"));
        let root_fd = root
            .try_clone_descriptor()
            .map_err(|error| format!("retain private materialization root: {error}"))?;
        let encoded = c_string(&name, "private sealed source")?;
        let output_fd = unsafe {
            libc::openat(
                root_fd.as_raw_fd(),
                encoded.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if output_fd < 0 {
            return Err(format!(
                "create private sealed source: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut output = unsafe { File::from_raw_fd(output_fd) };
        // Copy only the immutable, already-admitted length with bounded memory.
        // Positional reads do not consume the sender's shared file offset.
        let mut offset = 0;
        let mut buffer = [0_u8; 64 * 1024];
        while offset < metadata.len() {
            let limit = (metadata.len() - offset).min(buffer.len() as u64) as usize;
            let count = source
                .read_at(&mut buffer[..limit], offset)
                .map_err(|error| format!("read sealed mount bytes: {error}"))?;
            if count == 0 {
                return Err("sealed mount source ended before its exact length".to_string());
            }
            output
                .write_all(&buffer[..count])
                .map_err(|error| format!("write private sealed mount: {error}"))?;
            offset += count as u64;
        }
        let mode = mount_source_stat(fd)?.st_mode & 0o555;
        syscall_zero(
            unsafe { libc::fchmod(output.as_raw_fd(), mode) },
            "restrict private sealed mount",
        )?;
        let pinned_fd = unsafe {
            libc::openat(
                root_fd.as_raw_fd(),
                encoded.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if pinned_fd < 0 {
            return Err(format!(
                "retain private sealed source: {}",
                std::io::Error::last_os_error()
            ));
        }
        let pinned = unsafe { File::from_raw_fd(pinned_fd) };
        // No write-open handle may survive into exec, including in the adapter
        // parent: Linux correctly refuses ETXTBSY while such a handle exists.
        drop(output);
        // Keep the source name until move_mount: the kernel refuses attaching
        // an unlinked source. The caller detaches the whole private staging
        // filesystem after mounting, preserving linked executable identity
        // without exposing this setup alias to the workload.
        crate::InheritedDescriptorAuthority::from_owned_file(pinned, &lease)
    }

    fn descriptor_kind(fd: u32) -> Result<DescriptorKind, String> {
        let file = unsafe { File::from_raw_fd(duplicate_fd(raw_fd(fd)?)?) };
        crate::secure_fs::open_mount_entry_kind(&file)
            .map_err(|error| format!("inspect admitted mount descriptor: {error}"))
    }

    fn create_target(path: &PathBuf, kind: DescriptorKind) -> Result<(), String> {
        match kind {
            DescriptorKind::Directory => create_directory_target(path),
            DescriptorKind::Regular | DescriptorKind::UnixSocket => create_regular_target(path),
        }
    }

    fn prepare_descriptor_mount_target(
        destination: &PathBuf,
        kind: DescriptorKind,
        overlay_destination: Option<&PathBuf>,
        preceding_mounts: &[LinuxSandboxMount],
    ) -> Result<(), String> {
        let target = rooted(destination)?;
        // Source-backed ancestors must already contain their targets. The
        // overlay is mounted before the ordinary mounts: creating even an empty
        // target beneath it writes into the retained CoW upper and contaminates
        // later project capture. The same rule protects earlier bind sources.
        // Only private namespace-owned paths may acquire mount placeholders;
        // precreating a child before mounting its source would merely hide it.
        let source_backed = overlay_destination
            .is_some_and(|parent| destination.starts_with(parent))
            || preceding_mounts
                .iter()
                .any(|parent| destination.starts_with(&parent.destination));
        if source_backed {
            match kind {
                DescriptorKind::Directory => {
                    ensure_directory_path(&target, "source-backed sandbox mount target")
                }
                DescriptorKind::Regular | DescriptorKind::UnixSocket => {
                    ensure_regular_path(&target, "source-backed sandbox mount target")
                }
            }
        } else {
            create_target(&target, kind)
        }
    }

    fn create_directory_target(path: &PathBuf) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or_else(|| "sandbox directory target has no parent".to_string())?;
        mkdir_path(path_string(parent)?, 0o755)?;
        match mkdir_one(path_string(path)?, 0o755) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                ensure_directory_path(path, "sandbox mount target")
            }
            Err(error) => Err(format!(
                "create sandbox mount target {}: {error}",
                path.display()
            )),
        }
    }

    fn create_regular_target(path: &PathBuf) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or_else(|| "sandbox file target has no parent".to_string())?;
        mkdir_path(path_string(parent)?, 0o755)?;
        let c_path = c_string(path.as_os_str(), "sandbox file target")?;
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd >= 0 {
            unsafe { libc::close(fd) };
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EEXIST) {
            ensure_regular_path(path, "sandbox mount target")
        } else {
            Err(format!(
                "create sandbox mount target {}: {error}",
                path.display()
            ))
        }
    }

    fn mkdir_path(path: &str, mode: libc::mode_t) -> Result<(), String> {
        let path = PathBuf::from(path);
        let mut current = PathBuf::from("/");
        for component in path.components().skip(1) {
            let std::path::Component::Normal(component) = component else {
                return Err("sandbox directory path is not normalized".to_string());
            };
            current.push(component);
            match mkdir_one(path_string(&current)?, mode) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                    ensure_directory_path(&current, "sandbox directory")?;
                }
                Err(error) => {
                    return Err(format!(
                        "create sandbox directory {}: {error}",
                        current.display()
                    ));
                }
            }
        }
        Ok(())
    }

    fn mkdir_one(path: &str, mode: libc::mode_t) -> std::io::Result<()> {
        let path =
            CString::new(path).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        if unsafe { libc::mkdir(path.as_ptr(), mode) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn create_private_tmp() -> Result<(), String> {
        let target = format!("{ROOT}/tmp");
        mkdir_path(&target, 0o1777)?;
        mount_raw(
            Some("tmpfs"),
            &target,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=1777"),
        )
        .map_err(|error| format!("mount private sandbox tmp: {error}"))
    }

    fn create_minimal_devices(nested: bool) -> Result<(), String> {
        let dev = format!("{ROOT}/dev");
        mkdir_path(&dev, 0o755)?;
        mount_raw(
            Some("tmpfs"),
            &dev,
            Some("tmpfs"),
            libc::MS_NOSUID,
            Some("mode=0755"),
        )
        .map_err(|error| format!("mount minimal device filesystem: {error}"))?;
        // Linux's synthetic device floor, not a workload package dependency.
        // Nested runtimes reconstruct this surface inside their own namespace.
        // /dev/tty is only exposed to nested targets after they lose their
        // inherited controlling terminal without changing launcher identity.
        for (name, major, minor) in [
            ("null", 1, 3),
            ("zero", 1, 5),
            ("full", 1, 7),
            ("random", 1, 8),
            ("urandom", 1, 9),
            ("tty", 5, 0),
        ] {
            if name == "tty" && !nested {
                continue;
            }
            let source = format!("/dev/{name}");
            let source_c = CString::new(source.as_str()).expect("static device path");
            let fd = unsafe {
                libc::open(
                    source_c.as_ptr(),
                    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "open minimal device {source}: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            syscall_zero(
                unsafe { libc::fstat(file.as_raw_fd(), metadata.as_mut_ptr()) },
                "inspect pinned minimal device",
            )?;
            let metadata = unsafe { metadata.assume_init() };
            if metadata.st_mode & libc::S_IFMT != libc::S_IFCHR
                || libc::major(metadata.st_rdev) != major
                || libc::minor(metadata.st_rdev) != minor
            {
                return Err(format!(
                    "minimal device {source} is not its exact kernel device"
                ));
            }
            let target = PathBuf::from(format!("{dev}/{name}"));
            create_regular_target(&target)?;
            bind_fd_to_path(file.as_raw_fd(), &target, false)?;
            // Device nodes remain read-only and non-setuid, but setting NODEV
            // on their bind mounts would make the deliberately admitted
            // `/dev/null`, `/dev/zero`, and random devices unusable.
            set_mount_attributes(&target, true, false, false)?;
            if name != "tty" {
                prove_minimal_device_usable(&target)?;
            }
        }
        Ok(())
    }

    fn validate_character_device_descriptor(
        device: &LinuxSandboxCharacterDevice,
    ) -> Result<(), String> {
        if device.access == crate::CharacterDeviceAccess::ReadOnly {
            return Err(
                "read-only character-device access has no qualified enforcement backend".to_owned(),
            );
        }
        let fd = raw_fd(device.source_fd)?;
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe { libc::fstat(fd, metadata.as_mut_ptr()) },
            "inspect admitted character device",
        )?;
        let metadata = unsafe { metadata.assume_init() };
        if metadata.st_mode & libc::S_IFMT != libc::S_IFCHR
            || libc::major(metadata.st_rdev) != device.major
            || libc::minor(metadata.st_rdev) != device.minor
        {
            return Err("admitted character-device descriptor changed identity".to_owned());
        }
        Ok(())
    }

    fn attach_character_device(device: &LinuxSandboxCharacterDevice) -> Result<(), String> {
        validate_character_device_descriptor(device)?;
        if matches!(
            device.destination.as_path(),
            path if [
                PathBuf::from("/dev/null"),
                PathBuf::from("/dev/zero"),
                PathBuf::from("/dev/full"),
                PathBuf::from("/dev/random"),
                PathBuf::from("/dev/urandom"),
                PathBuf::from("/dev/tty"),
            ]
            .iter()
            .any(|reserved| reserved == path)
        ) {
            return Err("selected character device overlaps the minimal device floor".to_owned());
        }
        let target = rooted(&device.destination)?;
        create_regular_target(&target)?;
        bind_fd_to_path(raw_fd(device.source_fd)?, &target, false)?;
        set_mount_attributes(
            &target,
            device.access == crate::CharacterDeviceAccess::ReadOnly,
            false,
            false,
        )?;
        let encoded = c_string(target.as_os_str(), "attached character device")?;
        let flags = match device.access {
            crate::CharacterDeviceAccess::ReadOnly => libc::O_RDONLY,
            crate::CharacterDeviceAccess::ReadWrite => libc::O_RDWR,
        };
        let opened = unsafe {
            libc::open(
                encoded.as_ptr(),
                flags | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if opened < 0 {
            return Err(format!(
                "open attached character device: {}",
                std::io::Error::last_os_error()
            ));
        }
        let file = unsafe { File::from_raw_fd(opened) };
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe { libc::fstat(file.as_raw_fd(), metadata.as_mut_ptr()) },
            "revalidate attached character device",
        )?;
        let metadata = unsafe { metadata.assume_init() };
        if metadata.st_mode & libc::S_IFMT != libc::S_IFCHR
            || libc::major(metadata.st_rdev) != device.major
            || libc::minor(metadata.st_rdev) != device.minor
        {
            return Err("attached character device differs from admitted descriptor".to_owned());
        }
        Ok(())
    }

    fn prove_minimal_device_usable(target: &PathBuf) -> Result<(), String> {
        let target = c_string(target.as_os_str(), "minimal device")?;
        let fd = unsafe {
            libc::open(
                target.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(format!(
                "open admitted minimal device after mount hardening: {}",
                std::io::Error::last_os_error()
            ));
        }
        close_fd(fd);
        Ok(())
    }

    fn detach_nested_target_terminal() -> Result<(), String> {
        // Called in the freshly forked PID-namespace child, never the adapter
        // or an ordinary process-group-contained target. Do NOT use setsid:
        // held-launch attachment still proves the inherited launcher PGID,
        // even when a scope will contain later target session changes.
        let original_pgid = unsafe { libc::getpgrp() };
        let original_sid = unsafe { libc::getsid(0) };
        let fd = unsafe {
            libc::open(
                c"/dev/tty".as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd >= 0 {
            // A session leader's TIOCNOTTY can affect its entire foreground
            // group. This fresh child must only detach its own terminal.
            if original_sid == unsafe { libc::getpid() } {
                close_fd(fd);
                return Err(
                    "nested terminal detachment requires a non-session-leader child".to_owned(),
                );
            }
            let detached = syscall_zero(
                unsafe { libc::ioctl(fd, libc::TIOCNOTTY) },
                "detach nested target controlling terminal",
            );
            close_fd(fd);
            detached?;
        } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENXIO) {
            return Err(format!(
                "open nested target controlling terminal: {}",
                std::io::Error::last_os_error()
            ));
        }
        if unsafe { libc::getpgrp() } != original_pgid || unsafe { libc::getsid(0) } != original_sid
        {
            return Err("nested terminal detachment changed held-launch identity".to_owned());
        }
        let fd = unsafe {
            libc::open(
                c"/dev/tty".as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd >= 0 {
            close_fd(fd);
            return Err("nested target retained a controlling terminal".to_owned());
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENXIO) {
            return Err(format!(
                "verify detached nested terminal: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn probe_overlay() -> Result<(), String> {
        detached_overlay::probe()
    }

    fn probe_inherited_descriptor_mounts(directory: &File, bytes: &File) -> Result<(), String> {
        for (source, destination) in [
            (directory, "/.inherited-directory-probe"),
            (bytes, "/tmp/.sealed-bytes-probe"),
        ] {
            create_target(
                &rooted(&PathBuf::from(destination))?,
                descriptor_kind(source.as_raw_fd() as u32)?,
            )?;
            let mount = LinuxSandboxMount {
                source_fd: source.as_raw_fd() as u32,
                destination: PathBuf::from(destination),
                access: LinuxSandboxMountAccess::ReadOnly,
                layer: 0,
            };
            bind_descriptor_mount(&mount)
                .map_err(|error| format!("probe mount {destination}: {error}"))?;
            verify_descriptor_mount(&mount)
                .map_err(|error| format!("probe mounted {destination}: {error}"))?;
            let mut wrong_access = mount.clone();
            wrong_access.access = LinuxSandboxMountAccess::Writable;
            if verify_descriptor_mount(&wrong_access).is_ok() {
                return Err("sealed mount probe accepted wrong applied access".into());
            }
        }
        let path = format!("{ROOT}/tmp/.sealed-bytes-probe");
        if std::fs::read(&path).map_err(|error| format!("read sealed mount probe: {error}"))?
            != b"exact sealed bytes"
        {
            return Err("sealed mount probe changed bytes".to_string());
        }
        let encoded = CString::new(path).expect("static probe path");
        let writable = unsafe { libc::open(encoded.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
        if writable >= 0 {
            close_fd(writable);
            return Err("sealed mount probe admitted a writable handle".to_string());
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EROFS) {
            return Err("sealed mount probe did not prove read-only mount enforcement".to_string());
        }
        // The inherited probe source is /tmp, the ancestor of this fresh
        // sandbox root. Its recursive clone also retains the setup subtree's
        // mounts. Drop that probe-only alias after proving attachment, before
        // removing the sealed staging mountpoint; otherwise the cloned mount
        // keeps its underlying dentry busy. The sealed-byte target remains
        // attached, exactly as it does in a real launch.
        unmount_path(&rooted(&PathBuf::from("/.inherited-directory-probe"))?)?;
        Ok(())
    }

    fn probe_descriptor_mount() -> Result<(), String> {
        let source = PathBuf::from(format!("{ROOT}/.descriptor-probe-source"));
        let target = PathBuf::from(format!("{ROOT}/.descriptor-probe-target"));
        create_regular_target(&source)?;
        create_regular_target(&target)?;
        let source_path = c_string(source.as_os_str(), "descriptor probe source")?;
        let source_fd = unsafe {
            libc::open(
                source_path.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if source_fd < 0 {
            return Err(format!(
                "open descriptor-mount probe source: {}",
                std::io::Error::last_os_error()
            ));
        }
        let source = unsafe { File::from_raw_fd(source_fd) };
        let mount = LinuxSandboxMount {
            source_fd: u32::try_from(source.as_raw_fd())
                .map_err(|_| "descriptor probe source exceeds u32".to_string())?,
            destination: PathBuf::from("/.descriptor-probe-target"),
            access: LinuxSandboxMountAccess::ReadOnly,
            layer: 0,
        };
        bind_descriptor_mount(&mount)?;
        verify_descriptor_mount(&mount)
    }

    fn mount_overlay(overlay: &LinuxSandboxOverlay) -> Result<(), String> {
        let target = rooted(&overlay.destination)?;
        ensure_directory_path(&target, "overlay target")?;
        let target_authority = open_mount_target_no_symlinks(&overlay.destination)?;
        let source = overlay.template.inherited_authority();
        validate_overlay_template(source)?;
        // Clone the exact never-attached template in this fresh namespace.
        // No layer path/state descriptor or new overlay construction enters a
        // borrower. Per-child mounts are installed only on this private clone.
        bind_fd_to_mount_target(
            source.file().as_raw_fd(),
            target_authority.as_raw_fd(),
            false,
            false,
        )?;
        let mounted = open_mount_target_no_symlinks(&overlay.destination)?;
        let expected = mount_source_stat(source.file().as_raw_fd())?;
        let observed = mount_source_stat(mounted.as_raw_fd())?;
        if expected.st_dev != observed.st_dev || expected.st_ino != observed.st_ino {
            return Err("borrower mount differs from its exact admitted template".to_string());
        }
        Ok(())
    }

    fn bind_descriptor_mount(mount: &LinuxSandboxMount) -> Result<(), String> {
        let target = rooted(&mount.destination)?;
        let recursive = match descriptor_kind(mount.source_fd)? {
            DescriptorKind::Directory => {
                ensure_directory_path(&target, "descriptor mount target")?;
                true
            }
            DescriptorKind::Regular | DescriptorKind::UnixSocket => {
                ensure_regular_path(&target, "descriptor mount target")?;
                false
            }
        };
        // This proof runs after every lower layer has been mounted and before
        // any untrusted process exists. The exact descriptor remains live
        // through move_mount, so a mutable source cannot replace a proven path
        // component between inspection and mount attachment.
        let target_authority = open_mount_target_no_symlinks(&mount.destination)?;
        bind_fd_to_mount_target(
            raw_fd(mount.source_fd)?,
            target_authority.as_raw_fd(),
            mount.access == LinuxSandboxMountAccess::ReadOnly,
            recursive,
        )
    }

    /// Reopen the mounted destination, not the pre-attach target handle, while
    /// the private namespace still has no untrusted process. This checks
    /// effective root access and kind; non-directory mounts also permit an
    /// inode comparison. Directory source identity remains bound by the exact
    /// open_tree/move_mount descriptor path, not this post-attach observation.
    fn verify_descriptor_mount(mount: &LinuxSandboxMount) -> Result<(), String> {
        let mounted = open_mount_target_no_symlinks(&mount.destination)?;
        let source = mount_source_stat(raw_fd(mount.source_fd)?)?;
        let observed = mount_source_stat(mounted.as_raw_fd())?;
        let source_kind = source.st_mode & libc::S_IFMT;
        let observed_kind = observed.st_mode & libc::S_IFMT;
        // A recursive directory clone may contain a nested mount at its root.
        // The effective destination then has that nested mount's inode, not
        // the enclosing source descriptor's inode. The exact open_tree and
        // move_mount calls retain source authority; compare inode identity
        // only for non-directory mounts, which cannot contain nested mounts.
        if source_kind != observed_kind
            || (source_kind != libc::S_IFDIR
                && (source.st_dev != observed.st_dev || source.st_ino != observed.st_ino))
        {
            return Err("mounted destination kind or non-directory inode differs".into());
        }
        let path = rooted(&mount.destination)?;
        let path = c_string(path.as_os_str(), "mounted destination")?;
        let mut filesystem = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        syscall_zero(
            unsafe { libc::statvfs(path.as_ptr(), filesystem.as_mut_ptr()) },
            "observe mounted destination access",
        )?;
        let read_only = unsafe { filesystem.assume_init() }.f_flag & libc::ST_RDONLY != 0;
        if read_only != (mount.access == LinuxSandboxMountAccess::ReadOnly) {
            return Err("mounted destination access differs from admitted mount".into());
        }
        Ok(())
    }

    fn verify_final_descriptor_mounts(
        request: &LinuxSandboxRequest,
    ) -> Result<LinuxSandboxMountPreparationReceipt, String> {
        let mut observed = Vec::with_capacity(
            request.mounts.len()
                + request
                    .overlay
                    .as_ref()
                    .map_or(0, |overlay| overlay.writable_descendant_mounts.len()),
        );
        for mount in &request.mounts {
            let (kind, read_only) = verify_final_descriptor_mount(mount)?;
            observed.push((&mount.destination, kind, read_only));
        }
        if let Some(overlay) = &request.overlay {
            for descendant in &overlay.writable_descendant_mounts {
                // Reanchoring selected the exact mounted source before pivot.
                // The original descriptor may name the enclosing template,
                // so this final-view check covers kind and access only.
                let (kind, read_only) = verify_final_mount_view(
                    &descendant.destination,
                    libc::S_IFDIR,
                    LinuxSandboxMountAccess::Writable,
                    None,
                )?;
                observed.push((&descendant.destination, kind, read_only));
            }
        }
        let commitments = digest_mount_tuples(observed)?;
        Ok(LinuxSandboxMountPreparationReceipt {
            schema: commitments.schema,
            owned_child_pid: 0,
            mount_count: commitments.mount_count,
            destination_access_sha256: commitments.destination_access_sha256,
        })
    }

    /// Reobserve only destination kind and effective access after release.
    /// The admitted source descriptors are intentionally already closed; this
    /// cannot replace the descriptor-backed pre-release identity check.
    fn observe_post_release_mounts(
        request: &LinuxSandboxRequest,
    ) -> Result<LinuxSandboxMountPreparationCommitments, String> {
        let mut observed = Vec::with_capacity(
            request.mounts.len()
                + request
                    .overlay
                    .as_ref()
                    .map_or(0, |overlay| overlay.writable_descendant_mounts.len()),
        );
        for mount in &request.mounts {
            let (kind, read_only) = observe_final_mount_view(&mount.destination)?;
            observed.push((&mount.destination, kind, read_only));
        }
        if let Some(overlay) = &request.overlay {
            for descendant in &overlay.writable_descendant_mounts {
                let (kind, read_only) = observe_final_mount_view(&descendant.destination)?;
                observed.push((&descendant.destination, kind, read_only));
            }
        }
        digest_mount_tuples(observed)
    }

    fn observe_final_mount_view(destination: &PathBuf) -> Result<(u8, bool), String> {
        let mounted = open_mount_destination_beneath(Path::new("/"), destination)?;
        let observed = mount_source_stat(mounted.as_raw_fd())?;
        let mut filesystem = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        syscall_zero(
            unsafe { libc::fstatvfs(mounted.as_raw_fd(), filesystem.as_mut_ptr()) },
            "observe post-release target mount access",
        )?;
        Ok((
            mount_kind_code(observed.st_mode & libc::S_IFMT)?,
            unsafe { filesystem.assume_init() }.f_flag & libc::ST_RDONLY != 0,
        ))
    }

    pub(super) fn expected_mount_preparation(
        request: &LinuxSandboxRequest,
    ) -> Result<LinuxSandboxMountPreparationCommitments, String> {
        let descendants = request
            .overlay
            .as_ref()
            .map(|overlay| {
                overlay
                    .writable_descendant_mounts
                    .iter()
                    .map(|mount| mount.destination.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        expected_mount_preparation_from_mounts(&request.mounts, &descendants)
    }

    pub(super) fn expected_mount_preparation_from_mounts(
        mounts: &[LinuxSandboxMount],
        writable_overlay_destinations: &[PathBuf],
    ) -> Result<LinuxSandboxMountPreparationCommitments, String> {
        let mut expected = Vec::with_capacity(mounts.len() + writable_overlay_destinations.len());
        for mount in mounts {
            validate_absolute_path(&mount.destination, "expected mount destination")?;
            let source = mount_source_stat(raw_fd(mount.source_fd)?)?;
            let kind = mount_kind_code(source.st_mode & libc::S_IFMT)?;
            expected.push((
                &mount.destination,
                kind,
                mount.access == LinuxSandboxMountAccess::ReadOnly,
            ));
        }
        for destination in writable_overlay_destinations {
            validate_absolute_path(destination, "expected overlay descendant")?;
            expected.push((destination, 1, false));
        }
        digest_mount_tuples(expected)
    }

    fn digest_mount_tuples(
        mut observed: Vec<(&PathBuf, u8, bool)>,
    ) -> Result<LinuxSandboxMountPreparationCommitments, String> {
        observed.sort_by(|left, right| left.0.cmp(right.0));
        if observed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err("final target mount destinations are duplicated".into());
        }
        let count = u32::try_from(observed.len())
            .map_err(|_| "final target mount count exceeds native receipt bound")?;
        let mut digest = Sha256::new();
        digest.update(b"lillux.final-target-mounts/v1\0");
        digest.update(count.to_le_bytes());
        for (destination, kind, read_only) in observed {
            let path = destination.as_os_str().as_bytes();
            let length = u32::try_from(path.len())
                .map_err(|_| "final target mount path exceeds native receipt bound")?;
            digest.update(length.to_le_bytes());
            digest.update(path);
            digest.update([kind, u8::from(read_only)]);
        }
        Ok(LinuxSandboxMountPreparationCommitments {
            schema: u32::from(MOUNT_PREPARATION_SCHEMA),
            mount_count: count,
            destination_access_sha256: digest.finalize().into(),
        })
    }

    fn verify_final_descriptor_mount(mount: &LinuxSandboxMount) -> Result<(u8, bool), String> {
        let source = mount_source_stat(raw_fd(mount.source_fd)?)?;
        let kind = source.st_mode & libc::S_IFMT;
        let inode = (kind != libc::S_IFDIR).then_some((source.st_dev, source.st_ino));
        verify_final_mount_view(&mount.destination, kind, mount.access, inode)
    }

    fn verify_final_mount_view(
        destination: &PathBuf,
        expected_kind: libc::mode_t,
        access: LinuxSandboxMountAccess,
        expected_inode: Option<(libc::dev_t, libc::ino_t)>,
    ) -> Result<(u8, bool), String> {
        let mounted = open_mount_destination_beneath(Path::new("/"), destination)?;
        let observed = mount_source_stat(mounted.as_raw_fd())?;
        if observed.st_mode & libc::S_IFMT != expected_kind
            || expected_inode
                .is_some_and(|(dev, ino)| observed.st_dev != dev || observed.st_ino != ino)
        {
            return Err(format!(
                "final target mount {} differs in kind or non-directory inode",
                destination.display()
            ));
        }
        let mut filesystem = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        syscall_zero(
            unsafe { libc::fstatvfs(mounted.as_raw_fd(), filesystem.as_mut_ptr()) },
            "observe final target mount access",
        )?;
        let read_only = unsafe { filesystem.assume_init() }.f_flag & libc::ST_RDONLY != 0;
        if read_only != (access == LinuxSandboxMountAccess::ReadOnly) {
            return Err(format!(
                "final target mount {} differs in access",
                destination.display()
            ));
        }
        let kind = mount_kind_code(observed.st_mode & libc::S_IFMT)?;
        Ok((kind, read_only))
    }

    fn mount_kind_code(kind: libc::mode_t) -> Result<u8, String> {
        Ok(match kind {
            libc::S_IFDIR => 1,
            libc::S_IFREG => 2,
            libc::S_IFSOCK => 3,
            _ => return Err("final target mount has unsupported kind".into()),
        })
    }

    fn bind_fd_to_mount_target(
        source_fd: RawFd,
        target_fd: RawFd,
        read_only: bool,
        recursive: bool,
    ) -> Result<(), String> {
        let flags = OPEN_TREE_CLONE
            | OPEN_TREE_CLOEXEC
            | libc::AT_EMPTY_PATH as libc::c_uint
            | if recursive { AT_RECURSIVE } else { 0 };
        let mount_fd =
            unsafe { libc::syscall(libc::SYS_open_tree, source_fd, c"".as_ptr(), flags) } as RawFd;
        if mount_fd < 0 {
            return Err(format!(
                "clone exact descriptor mount: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mount = unsafe { File::from_raw_fd(mount_fd) };
        set_mount_attributes_fd(mount.as_raw_fd(), read_only, true, recursive)?;
        move_detached_mount_to_target(mount.as_raw_fd(), target_fd)
    }

    fn bind_fd_to_path(fd: RawFd, target: &PathBuf, recursive: bool) -> Result<(), String> {
        let source = format!("/proc/self/fd/{fd}");
        mount_raw(
            Some(&source),
            path_string(target)?,
            None,
            libc::MS_BIND | if recursive { libc::MS_REC } else { 0 },
            None,
        )
        .map_err(|error| format!("bind exact descriptor at {}: {error}", target.display()))
    }

    fn set_mount_attributes(
        target: &PathBuf,
        read_only: bool,
        deny_devices: bool,
        recursive: bool,
    ) -> Result<(), String> {
        let path = c_string(target.as_os_str(), "mount-attribute target")?;
        set_mount_attributes_at(
            libc::AT_FDCWD,
            path.as_ptr(),
            read_only,
            deny_devices,
            recursive,
            0,
        )
    }

    fn set_mount_attributes_fd(
        target_fd: RawFd,
        read_only: bool,
        deny_devices: bool,
        recursive: bool,
    ) -> Result<(), String> {
        set_mount_attributes_at(
            target_fd,
            c"".as_ptr(),
            read_only,
            deny_devices,
            recursive,
            libc::AT_EMPTY_PATH as libc::c_uint,
        )
    }

    fn set_mount_attributes_at(
        directory_fd: RawFd,
        path: *const libc::c_char,
        read_only: bool,
        deny_devices: bool,
        recursive: bool,
        flags: libc::c_uint,
    ) -> Result<(), String> {
        let mut attr_set = MOUNT_ATTR_NOSUID;
        if deny_devices {
            attr_set |= MOUNT_ATTR_NODEV;
        }
        if read_only {
            attr_set |= MOUNT_ATTR_RDONLY;
        }
        let attributes = MountAttr {
            attr_set,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        let result = unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                directory_fd,
                path,
                flags | if recursive { AT_RECURSIVE } else { 0 },
                &attributes,
                std::mem::size_of::<MountAttr>(),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(format!(
                "set sandbox mount attributes: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    fn move_detached_mount_to_target(mount_fd: RawFd, target_fd: RawFd) -> Result<(), String> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_move_mount,
                mount_fd,
                c"".as_ptr(),
                target_fd,
                c"".as_ptr(),
                MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(format!(
                "attach detached mount to exact target: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    fn move_path_mount_to_target(source: &PathBuf, target_fd: RawFd) -> Result<(), String> {
        let source = c_string(source.as_os_str(), "mount move source")?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_move_mount,
                libc::AT_FDCWD,
                source.as_ptr(),
                target_fd,
                c"".as_ptr(),
                MOVE_MOUNT_T_EMPTY_PATH,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(format!(
                "move mount to exact target: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    fn unmount_path(path: &PathBuf) -> Result<(), String> {
        let path = c_string(path.as_os_str(), "unmount target")?;
        syscall_zero(
            unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) },
            "detach sandbox staging mount",
        )
    }

    fn pivot_into_private_root() -> Result<(), String> {
        let root = CString::new(ROOT).expect("static root");
        syscall_zero(
            unsafe { libc::chdir(root.as_ptr()) },
            "enter private root mount",
        )?;
        let dot = c".";
        let old = c".lillux-old-root";
        let result = unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), old.as_ptr()) };
        if result != 0 {
            return Err(format!(
                "pivot into private sandbox root: {}",
                std::io::Error::last_os_error()
            ));
        }
        syscall_zero(unsafe { libc::chdir(c"/".as_ptr()) }, "enter sandbox root")?;
        syscall_zero(
            unsafe { libc::umount2(c"/.lillux-old-root".as_ptr(), libc::MNT_DETACH) },
            "detach former host root",
        )?;
        syscall_zero(
            unsafe { libc::rmdir(c"/.lillux-old-root".as_ptr()) },
            "remove former-root mountpoint",
        )?;
        // Seal the synthetic namespace scaffolding only after setup and
        // former-root removal. Nonrecursive is essential: explicitly mounted
        // writable workspaces and private /tmp keep their admitted access.
        // The target's confinement filter subsequently prevents remounts.
        set_mount_attributes(&PathBuf::from("/"), true, true, false)
    }

    fn probe_isolated_pid_child(nested: bool) -> Result<(), String> {
        let mut report = [0; 2];
        syscall_zero(
            unsafe { libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC) },
            "create PID namespace probe report",
        )?;
        let parent_pid = unsafe { libc::getpid() };
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            close_fd(report[0]);
            close_fd(report[1]);
            return Err(format!(
                "fork isolated PID namespace probe: {}",
                std::io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            close_fd(report[0]);
            let result = (|| {
                if unsafe { libc::getpid() } != 1 {
                    return Err("sandbox child is not PID 1 in its isolated namespace".to_string());
                }
                mount_pid_namespace_proc(nested)?;
                pivot_into_private_root()?;
                if nested {
                    detach_nested_target_terminal()?;
                }
                if !matches!(std::fs::create_dir("/namespace-replacement"), Err(ref error)
                    if error.raw_os_error() == Some(libc::EROFS))
                {
                    return Err("synthetic namespace root is not read-only".to_string());
                }
                std::fs::write("/tmp/namespace-writable-probe", b"writable")
                    .map_err(|error| format!("private tmp lost writable access: {error}"))?;
                if std::fs::read_link("/proc/self").map_err(|error| error.to_string())?
                    != PathBuf::from("1")
                    || std::path::Path::new(&format!("/proc/{parent_pid}")).exists()
                    || (!nested
                        && (std::path::Path::new("/proc/sys").exists()
                            || std::path::Path::new("/proc/meminfo").exists()))
                {
                    return Err("PID procfs exposes a foreign or non-task surface".to_string());
                }
                let flags = std::fs::OpenOptions::new()
                    .write(true)
                    .open("/proc/self/oom_score_adj");
                if !nested
                    && !matches!(flags, Err(ref error) if error.raw_os_error() == Some(libc::EROFS))
                {
                    return Err("PID procfs is not read-only".to_string());
                }
                let descendant = unsafe { libc::fork() };
                if descendant < 0 {
                    return Err("fork PID procfs visibility probe failed".to_string());
                }
                if descendant == 0 {
                    let own = unsafe { libc::getpid() }.to_string();
                    let visible = std::fs::read_link("/proc/self").ok() == Some(PathBuf::from(own));
                    let strict_filter = syscall_zero(
                        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
                        "probe strict NNP",
                    )
                    .and_then(|()| install_confinement_filter(true, false));
                    unsafe {
                        libc::_exit(if visible && strict_filter.is_ok() {
                            0
                        } else {
                            125
                        })
                    };
                }
                let visible = std::path::Path::new(&format!("/proc/{descendant}")).exists();
                let mut status = 0;
                if unsafe { libc::waitpid(descendant, &mut status, 0) } != descendant
                    || !visible
                    || !libc::WIFEXITED(status)
                    || libc::WEXITSTATUS(status) != 0
                {
                    return Err("PID procfs descendant visibility probe failed".to_string());
                }
                syscall_zero(
                    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
                    "set no_new_privs during sandbox probe",
                )?;
                drop_target_namespace_capabilities()?;
                install_confinement_filter(!nested, nested)?;
                if nested {
                    probe_nested_sandbox()
                } else {
                    Ok(())
                }
            })();
            match &result {
                Ok(()) => {
                    let _ = write_all_fd(report[1], &[CHILD_READY]);
                }
                Err(error) => {
                    let _ = write_child_error(report[1], error);
                }
            }
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
        }
        close_fd(report[1]);
        let outcome = read_child_ready(report[0]);
        close_fd(report[0]);
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
            return Err(format!(
                "wait for isolated PID namespace probe: {}",
                std::io::Error::last_os_error()
            ));
        }
        if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
            outcome
        } else {
            Err(format!(
                "isolated PID namespace probe failed: {}",
                outcome
                    .err()
                    .unwrap_or_else(|| "child exited without failure report".to_string())
            ))
        }
    }

    // Kernel-only generation inspection, not a workload executable or a
    // provider-specific preflight. The explicit nested mode admits non-task
    // kernel metadata, unlike task-only proc. A fresh namespace must still
    // exclude host PIDs and global write authority after gaining child caps.
    fn probe_nested_sandbox() -> Result<(), String> {
        verify_nested_host_controls_unavailable()?;
        enter_mapped_user_namespace()?;
        verify_nested_host_controls_unavailable()?;
        syscall_zero(
            unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID) },
            "probe nested namespaces",
        )?;
        if unsafe { libc::unshare(libc::CLONE_NEWCGROUP) } != -1
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
        {
            return Err("nested sandbox did not refuse scope namespace creation".to_owned());
        }
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(format!(
                "fork nested namespace probe: {}",
                std::io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            let result = (|| {
                if unsafe { libc::getpid() } != 1 {
                    return Err("nested probe did not enter its PID namespace".to_owned());
                }
                mount_raw(
                    Some("proc"),
                    "/proc",
                    Some("proc"),
                    libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                    None,
                )
                .map_err(|error| format!("mount nested isolated proc: {error}"))?;
                if std::fs::read_link("/proc/self").map_err(|error| error.to_string())?
                    != PathBuf::from("1")
                {
                    return Err("nested proc lost its PID-local surface".to_owned());
                }
                verify_nested_host_controls_unavailable()?;
                Ok::<(), String>(())
            })();
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
        }
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, 0) } != pid
            || !libc::WIFEXITED(status)
            || libc::WEXITSTATUS(status) != 0
        {
            return Err("nested PID/proc generation probe refused".to_owned());
        }
        Ok(())
    }

    /// These are Linux kernel-control interfaces, not configurable project
    /// files. Check opening only: qualification NEVER writes a host control.
    /// Child user-namespace capabilities must not grant initial-namespace
    /// control, even when fresh procfs is mounted by the nested runtime.
    fn verify_nested_host_controls_unavailable() -> Result<(), String> {
        for path in [
            "/proc/sys/kernel/overflowuid",
            "/proc/sys/kernel/overflowgid",
            "/proc/sysrq-trigger",
        ] {
            match std::fs::OpenOptions::new().write(true).open(path) {
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::EROFS)
                    ) => {}
                Ok(_) => {
                    return Err(format!(
                        "nested sandbox can open host kernel control {path}"
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "cannot qualify kernel control denial {path}: {error}"
                    ));
                }
            }
        }
        Ok(())
    }

    fn spawn_target(
        request: LinuxSandboxRequest,
        stdio: Option<&[std::fs::File; 3]>,
    ) -> Result<LinuxSandboxProcess, String> {
        let mut ready = [0; 2];
        syscall_zero(
            unsafe { libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC) },
            "create sandbox readiness pipe",
        )?;
        let mut launch_failure = [0; 2];
        if let Err(error) = syscall_zero(
            unsafe { libc::pipe2(launch_failure.as_mut_ptr(), libc::O_CLOEXEC) },
            "create sandbox launch-failure pipe",
        ) {
            close_fd(ready[0]);
            close_fd(ready[1]);
            return Err(error);
        }
        let mut applied_launch = [0; 2];
        if let Err(error) = syscall_zero(
            unsafe { libc::pipe2(applied_launch.as_mut_ptr(), libc::O_CLOEXEC) },
            "create sandbox applied-launch pipe",
        ) {
            for descriptor in ready.into_iter().chain(launch_failure) {
                close_fd(descriptor);
            }
            return Err(error);
        }
        let mut reserved_target_descriptors = request
            .target_channels
            .iter()
            .map(|(_, target)| raw_fd(*target))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if stdio.is_some() {
            reserved_target_descriptors.extend([0, 1, 2]);
        }
        let mut private_descriptors = [
            ready[0],
            ready[1],
            launch_failure[0],
            launch_failure[1],
            applied_launch[0],
            applied_launch[1],
        ];
        if let Err(error) = relocate_internal_descriptors(
            &mut private_descriptors,
            &reserved_target_descriptors,
            "sandbox control pipes",
        ) {
            for descriptor in private_descriptors {
                close_fd(descriptor);
            }
            return Err(error);
        }
        ready.copy_from_slice(&private_descriptors[..2]);
        launch_failure.copy_from_slice(&private_descriptors[2..4]);
        applied_launch.copy_from_slice(&private_descriptors[4..]);
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            close_fd(ready[0]);
            close_fd(ready[1]);
            close_fd(launch_failure[0]);
            close_fd(launch_failure[1]);
            close_fd(applied_launch[0]);
            close_fd(applied_launch[1]);
            return Err(format!(
                "fork sandbox target: {}",
                std::io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            close_fd(ready[0]);
            close_fd(launch_failure[0]);
            close_fd(applied_launch[0]);
            let outcome = child_target_main(
                &request,
                ready[1],
                launch_failure[1],
                applied_launch[1],
                stdio,
            );
            if let Err(error) = outcome {
                let _ = write_child_error(launch_failure[1], &error);
                let _ = write_child_error(ready[1], &error);
            }
            unsafe { libc::_exit(125) };
        }
        close_fd(ready[1]);
        close_fd(launch_failure[1]);
        close_fd(applied_launch[1]);
        let result = read_child_target_ready(ready[0]);
        close_fd(ready[0]);
        let mut mount_preparation = match result {
            Ok(receipt) => receipt,
            Err(error) => {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, std::ptr::null_mut(), 0);
                }
                close_fd(launch_failure[0]);
                close_fd(applied_launch[0]);
                return Err(error);
            }
        };
        mount_preparation.owned_child_pid = pid as u32;
        Ok(LinuxSandboxProcess {
            pid,
            namespace_lifetime: None,
            // SAFETY: the parent exclusively owns this surviving pipe end.
            launch_failure: Some(unsafe { File::from_raw_fd(launch_failure[0]) }),
            // SAFETY: the parent exclusively owns this surviving pipe end.
            applied_launch: Some(unsafe { File::from_raw_fd(applied_launch[0]) }),
            observed_applied_launch: None,
            mount_preparation: Some(mount_preparation),
            termination_requested: false,
        })
    }

    fn child_target_main(
        request: &LinuxSandboxRequest,
        ready_fd: RawFd,
        launch_failure_fd: RawFd,
        applied_launch_fd: RawFd,
        stdio: Option<&[std::fs::File; 3]>,
    ) -> Result<(), String> {
        if unsafe { libc::getpid() } != 1 {
            return Err("sandbox target is not PID 1 in its isolated namespace".to_string());
        }
        syscall_zero(
            unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) },
            "bind sandbox target lifetime to adapter",
        )?;
        // An outside-namespace parent appears as PID 0 here, so getppid
        // cannot prove adapter liveness. The exact readiness pipe refuses a
        // missing parent even if it died before PDEATHSIG was installed.
        if request.proc_filesystem != LinuxSandboxProcFilesystem::Empty {
            mount_pid_namespace_proc(request.nested_sandbox)?;
        }
        pivot_into_private_root()?;
        if request.nested_sandbox {
            detach_nested_target_terminal()?;
        }
        // Earlier attachment checks ran against the private root before pivot.
        // Recheck the target view after layering, fixed-parent changes and pivot,
        // while admitted descriptors are live and before readiness or exec.
        let mount_preparation = verify_final_descriptor_mounts(request)?;
        let mut mapped_channels = Vec::with_capacity(request.target_channels.len());
        if let Some(stdio) = stdio {
            use std::os::fd::AsRawFd as _;
            for (target, source) in stdio.iter().enumerate() {
                place_target_channel(source.as_raw_fd(), target as RawFd)?;
            }
        }
        for (source, target) in &request.target_channels {
            let source = raw_fd(*source)?;
            let target = RawFd::try_from(*target)
                .map_err(|_| "target channel descriptor exceeds RawFd".to_string())?;
            place_target_channel(source, target)?;
            mapped_channels.push(target);
        }
        let mut keep = vec![ready_fd, launch_failure_fd, applied_launch_fd];
        keep.extend(
            mapped_channels
                .into_iter()
                .filter(|fd| *fd > libc::STDERR_FILENO),
        );
        if let LinuxSandboxLifecycle::AwaitRelease {
            release_fd,
            release_keepalive_fd,
        } = request.lifecycle
        {
            keep.push(raw_fd(release_fd)?);
            keep.push(raw_fd(release_keepalive_fd)?);
        }
        close_all_except(&keep)?;
        syscall_zero(
            unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
            "set no_new_privs for sandbox target",
        )?;
        drop_target_namespace_capabilities()?;
        if request.nested_sandbox {
            verify_nested_host_controls_unavailable()?;
        }
        install_confinement_filter(request.contain_process_group, request.nested_sandbox)?;
        let mut ready = [0_u8; MOUNT_PREPARATION_RECORD_BYTES];
        ready[0] = CHILD_TARGET_READY;
        ready[1] = MOUNT_PREPARATION_SCHEMA;
        ready[2..6].copy_from_slice(&mount_preparation.mount_count.to_le_bytes());
        ready[6..].copy_from_slice(&mount_preparation.destination_access_sha256);
        write_all_fd(ready_fd, &ready)?;
        if let LinuxSandboxLifecycle::AwaitRelease {
            release_fd,
            release_keepalive_fd,
        } = request.lifecycle
        {
            let mut release = [0_u8; 1];
            let count = loop {
                let count = unsafe {
                    libc::read(
                        raw_fd(release_fd)?,
                        release.as_mut_ptr().cast(),
                        release.len(),
                    )
                };
                if count >= 0 {
                    break count;
                }
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINTR) {
                    return Err(format!(
                        "stage=release errno={} read sandbox release boundary: {error}",
                        error.raw_os_error().unwrap_or(0)
                    ));
                }
            };
            if count != 1 || release[0] != 1 {
                return Err(
                    "stage=release errno=0 sandbox release boundary closed or carried invalid data"
                        .to_string(),
                );
            }
            close_fd(raw_fd(release_fd)?);
            close_fd(raw_fd(release_keepalive_fd)?);
        }
        close_fd(ready_fd);
        let post_release_mount_view = observe_post_release_mounts(request)?;
        if post_release_mount_view.mount_count != mount_preparation.mount_count
            || post_release_mount_view.destination_access_sha256
                != mount_preparation.destination_access_sha256
        {
            return Err("post-release target mounts differ from held final-root preparation".into());
        }
        exec_target_with_receipt(request, Some((applied_launch_fd, post_release_mount_view)))
    }

    /// Called only in the actual PID-namespace child before pivot_root. A
    /// parent which merely unshared CLONE_NEWPID still belongs to its old PID
    /// namespace and must never mount this filesystem. Linux's unprivileged
    /// mount visibility check needs the original proc mount still present at
    /// this setup boundary. Immediately pivot/detach the old root afterwards,
    /// before descriptor closure, confinement, readiness or untrusted exec.
    /// No host procfs is bound into the target view.
    fn mount_pid_namespace_proc(nested_sandbox: bool) -> Result<(), String> {
        if unsafe { libc::getpid() } != 1 {
            return Err("PID procfs must be mounted by the isolated namespace init".to_string());
        }
        let target = CString::new(format!("{ROOT}/proc")).expect("static proc mountpoint");
        syscall_zero(
            unsafe {
                libc::mount(
                    c"proc".as_ptr(),
                    target.as_ptr(),
                    c"proc".as_ptr(),
                    (if nested_sandbox { 0 } else { libc::MS_RDONLY })
                        | libc::MS_NOSUID
                        | libc::MS_NODEV
                        | libc::MS_NOEXEC,
                    if nested_sandbox {
                        std::ptr::null()
                    } else {
                        c"subset=pid".as_ptr().cast()
                    },
                )
            },
            "mount isolated PID namespace procfs",
        )
    }

    #[cfg(test)]
    fn exec_target(request: &LinuxSandboxRequest) -> Result<(), String> {
        exec_target_with_receipt(request, None)
    }

    fn exec_target_with_receipt(
        request: &LinuxSandboxRequest,
        applied_launch: Option<(RawFd, LinuxSandboxMountPreparationCommitments)>,
    ) -> Result<(), String> {
        let executable = c_string(request.executable.as_os_str(), "sandbox executable")?;
        let argv0 = c_string(&request.argv0, "sandbox argv0")?;
        let mut arguments = Vec::with_capacity(request.arguments.len() + 1);
        arguments.push(argv0);
        for argument in &request.arguments {
            arguments.push(c_string(argument, "sandbox argument")?);
        }
        let mut environment = Vec::with_capacity(request.environment.len());
        for (name, value) in &request.environment {
            let mut entry = OsString::from(name);
            entry.push("=");
            entry.push(value);
            environment.push(c_string(&entry, "sandbox environment")?);
        }
        let argv = arguments
            .iter()
            .map(|value| value.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect::<Vec<_>>();
        let envp = environment
            .iter()
            .map(|value| value.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect::<Vec<_>>();
        let cwd = c_string(request.cwd.as_os_str(), "sandbox cwd")?;
        syscall_zero(
            unsafe { libc::chdir(cwd.as_ptr()) },
            "enter sandbox target cwd",
        )?;
        if let Some((fd, mounts)) = applied_launch {
            write_applied_launch(fd, &executable, &arguments, &environment, &mounts)?;
        }
        unsafe {
            libc::execve(executable.as_ptr(), argv.as_ptr(), envp.as_ptr());
        }
        let error = std::io::Error::last_os_error();
        Err(format!(
            "stage=exec errno={} target={}: {error}",
            error.raw_os_error().unwrap_or(0),
            request.executable.display(),
        ))
    }

    fn hash_c_strings(values: &[CString]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update((values.len() as u64).to_le_bytes());
        for value in values {
            let bytes = value.as_bytes_with_nul();
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        hasher.finalize().into()
    }

    fn hash_c_string(value: &CStr) -> [u8; 32] {
        Sha256::digest(value.to_bytes_with_nul()).into()
    }

    pub(super) fn applied_launch_matches_target(
        receipt: &LinuxSandboxAppliedLaunchReceipt,
        target: LinuxSandboxAppliedLaunchTarget<'_>,
    ) -> Result<bool, String> {
        Ok(receipt.matches_commitments(&applied_launch_commitments_from_target(target)?))
    }

    pub(super) fn applied_launch_commitments_from_target(
        target: LinuxSandboxAppliedLaunchTarget<'_>,
    ) -> Result<LinuxSandboxAppliedLaunchCommitments, String> {
        let executable = c_string(target.executable.as_os_str(), "sandbox executable")?;
        let cwd = c_string(target.cwd.as_os_str(), "sandbox cwd")?;
        let mut arguments = Vec::with_capacity(target.arguments.len() + 1);
        arguments.push(c_string(target.argv0, "sandbox argv0")?);
        for argument in target.arguments {
            arguments.push(c_string(argument, "sandbox argument")?);
        }
        let mut environment = Vec::with_capacity(target.environment.len());
        for (name, value) in target.environment {
            let mut entry = OsString::from(name);
            entry.push("=");
            entry.push(value);
            environment.push(c_string(&entry, "sandbox environment")?);
        }
        Ok(LinuxSandboxAppliedLaunchCommitments {
            executable_sha256: hash_c_string(&executable),
            argv_sha256: hash_c_strings(&arguments),
            environment_sha256: hash_c_strings(&environment),
            cwd_sha256: hash_c_string(&cwd),
        })
    }

    #[cfg(test)]
    mod applied_launch_tests {
        use super::*;

        #[test]
        fn commitments_match_only_the_exact_exec_vector() {
            let mut request = super::super::tests::minimal_request();
            request
                .environment
                .insert("TOKEN".into(), "test-secret".into());
            let argv = [CString::new("tool").unwrap()];
            let env = [CString::new("TOKEN=test-secret").unwrap()];
            let receipt = LinuxSandboxAppliedLaunchReceipt {
                owned_child_pid: 123,
                namespace_pid: 1,
                effective_uid: NAMESPACE_USER_ID,
                effective_gid: NAMESPACE_GROUP_ID,
                no_new_privs: true,
                seccomp_mode: libc::SECCOMP_MODE_FILTER,
                executable_sha256: hash_c_string(c"/bin/tool"),
                argv_sha256: hash_c_strings(&argv),
                environment_sha256: hash_c_strings(&env),
                cwd_sha256: hash_c_string(c"/workspace"),
                post_release_mount_view: LinuxSandboxMountPreparationCommitments {
                    schema: 1,
                    mount_count: 0,
                    destination_access_sha256: [0; 32],
                },
            };
            assert!(receipt.matches_request(&request).unwrap());
            assert!(receipt.matches_post_release_mounts(&receipt.post_release_mount_view));
            let mut wrong_mounts = receipt.post_release_mount_view.clone();
            wrong_mounts.destination_access_sha256[0] ^= 1;
            assert!(!receipt.matches_post_release_mounts(&wrong_mounts));
            request
                .environment
                .insert("TOKEN".into(), "other-secret".into());
            assert!(!receipt.matches_request(&request).unwrap());
            request
                .environment
                .insert("TOKEN".into(), "test-secret".into());
            request.cwd = "/other".into();
            assert!(!receipt.matches_request(&request).unwrap());
        }
    }

    fn write_applied_launch(
        fd: RawFd,
        executable: &CStr,
        arguments: &[CString],
        environment: &[CString],
        post_release_mount_view: &LinuxSandboxMountPreparationCommitments,
    ) -> Result<(), String> {
        // Bounded native observation of the final cwd, not the caller's
        // requested spelling. Refuse a cwd that cannot be represented here.
        let mut cwd = [0_u8; 8192];
        if unsafe { libc::getcwd(cwd.as_mut_ptr().cast(), cwd.len()) }.is_null() {
            return Err(format!(
                "observe sandbox target cwd: {}",
                std::io::Error::last_os_error()
            ));
        }
        let cwd =
            CStr::from_bytes_until_nul(&cwd).map_err(|_| "sandbox target cwd lacks termination")?;
        let no_new_privs = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
        if no_new_privs != 1 {
            return Err("sandbox target lost no_new_privs before exec".into());
        }
        let seccomp_mode = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) };
        if seccomp_mode != libc::SECCOMP_MODE_FILTER as i32 {
            return Err("sandbox target lacks seccomp filter before exec".into());
        }
        let mut record = [0_u8; APPLIED_LAUNCH_RECORD_BYTES];
        record[0] = APPLIED_LAUNCH_RECORD;
        let mut offset = 1;
        for value in [
            unsafe { libc::getpid() as u32 },
            unsafe { libc::geteuid() },
            unsafe { libc::getegid() },
            no_new_privs as u32,
            seccomp_mode as u32,
            post_release_mount_view.schema,
            post_release_mount_view.mount_count,
        ] {
            record[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            offset += 4;
        }
        for digest in [
            hash_c_string(executable),
            hash_c_strings(arguments),
            hash_c_strings(environment),
            hash_c_string(cwd),
            post_release_mount_view.destination_access_sha256,
        ] {
            record[offset..offset + 32].copy_from_slice(&digest);
            offset += 32;
        }
        // PIPE_BUF-sized atomic write: a pollable record is always complete.
        let written = unsafe { libc::write(fd, record.as_ptr().cast(), record.len()) };
        if written != record.len() as isize {
            return Err(format!(
                "write sandbox applied-launch record: {}",
                std::io::Error::last_os_error()
            ));
        }
        close_fd(fd);
        Ok(())
    }

    pub(super) fn read_applied_launch(
        fd: RawFd,
    ) -> Result<LinuxSandboxAppliedLaunchReceipt, String> {
        let mut record = [0_u8; APPLIED_LAUNCH_RECORD_BYTES];
        // The child emits a single atomic PIPE_BUF-sized record. EOF, including
        // death before this boundary, is never promoted to applied evidence.
        read_exact_fd(fd, &mut record).map_err(|error| {
            format!("sandbox applied-launch record missing or incomplete: {error}")
        })?;
        if record[0] != APPLIED_LAUNCH_RECORD {
            return Err("sandbox applied-launch record has invalid kind".into());
        }
        let mut offset = 1;
        let mut read_u32 = || {
            let value = u32::from_le_bytes(record[offset..offset + 4].try_into().unwrap());
            offset += 4;
            value
        };
        let namespace_pid = read_u32();
        let effective_uid = read_u32();
        let effective_gid = read_u32();
        let no_new_privs = read_u32();
        let seccomp_mode = read_u32();
        let mount_schema = read_u32();
        let mount_count = read_u32();
        if namespace_pid != 1
            || no_new_privs != 1
            || seccomp_mode != libc::SECCOMP_MODE_FILTER as u32
            || mount_schema != u32::from(MOUNT_PREPARATION_SCHEMA)
            || mount_count > 4096
        {
            return Err("sandbox applied-launch record has invalid native controls".into());
        }
        let mut digest = || {
            let value = record[offset..offset + 32].try_into().unwrap();
            offset += 32;
            value
        };
        Ok(LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 0,
            namespace_pid,
            effective_uid,
            effective_gid,
            no_new_privs: true,
            seccomp_mode,
            executable_sha256: digest(),
            argv_sha256: digest(),
            environment_sha256: digest(),
            cwd_sha256: digest(),
            post_release_mount_view: LinuxSandboxMountPreparationCommitments {
                schema: mount_schema,
                mount_count,
                destination_access_sha256: digest(),
            },
        })
    }

    // Linux capability ABI v3, from <linux/capability.h>. libc exposes the
    // syscalls but not these wire structs. Keep OS ABI details inside Lillux;
    // this is not a configurable workload privilege inventory.
    const CAPABILITY_VERSION: u32 = 0x2008_0522;
    const CAPABILITY_WORDS: usize = 2;

    #[repr(C)]
    struct CapabilityHeader {
        version: u32,
        pid: libc::c_int,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    struct CapabilityData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    const TARGET_SECUREBITS: libc::c_int = libc::SECBIT_NOROOT
        | libc::SECBIT_NOROOT_LOCKED
        | libc::SECBIT_NO_SETUID_FIXUP
        | libc::SECBIT_NO_SETUID_FIXUP_LOCKED
        | libc::SECBIT_KEEP_CAPS_LOCKED
        | libc::SECBIT_NO_CAP_AMBIENT_RAISE
        | libc::SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED;

    fn capability_count() -> Result<u32, String> {
        // Discover the kernel inventory, not a remembered CAP_LAST_CAP. Refuse
        // a kernel beyond the ABI we can clear, rather than truncate its sets.
        for capability in 0..=CAPABILITY_WORDS as u32 * u32::BITS {
            match unsafe { libc::prctl(libc::PR_CAPBSET_READ, capability, 0, 0, 0) } {
                0 | 1 => {}
                -1 if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
                    && capability > 0 =>
                {
                    return Ok(capability);
                }
                _ => {
                    return Err(format!(
                        "inspect capability bounding set: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
        }
        Err("kernel capabilities exceed the supported capability ABI".to_string())
    }

    /// Irreversibly give up setup authority in the user namespace which owns
    /// the outer mounts/PID namespace, before readiness or any untrusted exec.
    /// The single-ID mapping is not a privilege grant. Lock exec/UID capability
    /// fixups as well as clearing the sets: NNP alone does not remove the
    /// setup capabilities acquired when creating a user namespace.
    ///
    /// Do not replace the syscall filter with this step. A later nested user
    /// namespace has its own capabilities/securebits; permitting it separately
    /// requires locked inherited mounts and whole-execution lifecycle proof.
    fn drop_target_namespace_capabilities() -> Result<(), String> {
        let count = capability_count()?;
        syscall_zero(
            unsafe { libc::prctl(libc::PR_SET_SECUREBITS, TARGET_SECUREBITS, 0, 0, 0) },
            "lock target namespace privilege reduction",
        )?;
        syscall_zero(
            unsafe {
                libc::prctl(
                    libc::PR_CAP_AMBIENT,
                    libc::PR_CAP_AMBIENT_CLEAR_ALL,
                    0,
                    0,
                    0,
                )
            },
            "clear target ambient capabilities",
        )?;
        for capability in 0..count {
            syscall_zero(
                unsafe { libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0) },
                "drop target capability bounding set",
            )?;
        }
        let header = CapabilityHeader {
            version: CAPABILITY_VERSION,
            pid: 0,
        };
        let empty = [CapabilityData::default(); CAPABILITY_WORDS];
        if unsafe { libc::syscall(libc::SYS_capset, &header, empty.as_ptr()) } != 0 {
            return Err(format!(
                "clear target capability sets: {}",
                std::io::Error::last_os_error()
            ));
        }
        verify_target_namespace_capabilities()
    }

    fn verify_target_namespace_capabilities() -> Result<(), String> {
        let mut header = CapabilityHeader {
            version: CAPABILITY_VERSION,
            pid: 0,
        };
        let mut observed = [CapabilityData::default(); CAPABILITY_WORDS];
        if unsafe { libc::syscall(libc::SYS_capget, &mut header, observed.as_mut_ptr()) } != 0 {
            return Err(format!(
                "read target capability sets: {}",
                std::io::Error::last_os_error()
            ));
        }
        if header.version != CAPABILITY_VERSION
            || observed != [CapabilityData::default(); CAPABILITY_WORDS]
            || unsafe { libc::prctl(libc::PR_GET_SECUREBITS, 0, 0, 0, 0) } != TARGET_SECUREBITS
            || unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1
        {
            return Err("target retained namespace setup privileges".to_string());
        }
        for capability in 0..capability_count()? {
            if unsafe { libc::prctl(libc::PR_CAPBSET_READ, capability, 0, 0, 0) } != 0
                || unsafe {
                    libc::prctl(
                        libc::PR_CAP_AMBIENT,
                        libc::PR_CAP_AMBIENT_IS_SET,
                        capability,
                        0,
                        0,
                    )
                } != 0
            {
                return Err("target retained bounding or ambient capabilities".to_string());
            }
        }
        Ok(())
    }

    fn install_confinement_filter(
        contain_process_group: bool,
        nested_sandbox: bool,
    ) -> Result<(), String> {
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            let _ = (contain_process_group, nested_sandbox);
            return Err("native sandbox seccomp is unsupported on this architecture".to_string());
        }
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            const BPF_LD_W_ABS: u16 = 0x20;
            const BPF_JMP_JEQ_K: u16 = 0x15;
            const BPF_JMP_JSET_K: u16 = 0x45;
            const BPF_RET_K: u16 = 0x06;
            const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
            const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
            const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
            #[cfg(target_arch = "x86_64")]
            const AUDIT_ARCH: u32 = 0xc000_003e;
            #[cfg(target_arch = "aarch64")]
            const AUDIT_ARCH: u32 = 0xc000_00b7;

            let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
            let mut filter = vec![
                instruction(BPF_LD_W_ABS, 0, 0, 4),
                instruction(BPF_JMP_JEQ_K, 1, 0, AUDIT_ARCH),
                instruction(BPF_RET_K, 0, 0, SECCOMP_RET_KILL_PROCESS),
                instruction(BPF_LD_W_ABS, 0, 0, 0),
            ];
            #[cfg(target_arch = "x86_64")]
            filter.extend([
                instruction(BPF_JMP_JSET_K, 0, 1, 0x4000_0000),
                instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::ENOSYS as u32),
            ]);
            // The target runs as PID 1 in a fresh PID namespace, so ordinary
            // signal syscalls remain available for its own descendants. Do
            // not blanket-deny kill/tkill/tgkill: Cargo and language runtimes
            // legitimately use them. Namespace translation prevents those
            // PIDs from naming host processes; the filter instead removes
            // namespace/root mutation and cross-process authority-stealing
            // surfaces.
            let denied = [
                libc::SYS_open_by_handle_at,
                libc::SYS_ptrace,
                libc::SYS_process_vm_readv,
                libc::SYS_process_vm_writev,
                libc::SYS_pidfd_getfd,
                libc::SYS_bpf,
                libc::SYS_perf_event_open,
                libc::SYS_keyctl,
                libc::SYS_add_key,
                libc::SYS_request_key,
                libc::SYS_reboot,
                libc::SYS_init_module,
                libc::SYS_finit_module,
                libc::SYS_delete_module,
                libc::SYS_swapon,
                libc::SYS_swapoff,
                libc::SYS_acct,
            ];
            for syscall in denied {
                filter.push(instruction(BPF_JMP_JEQ_K, 0, 1, syscall as u32));
                filter.push(instruction(
                    BPF_RET_K,
                    0,
                    0,
                    SECCOMP_RET_ERRNO | libc::EPERM as u32,
                ));
            }
            // Nested mutation is confined by the dropped/locked outer
            // capabilities and inherited less-privileged mount locks. It is
            // NEVER enabled merely because group escape was permitted.
            if !nested_sandbox {
                for syscall in [
                    libc::SYS_setns,
                    libc::SYS_unshare,
                    libc::SYS_mount,
                    libc::SYS_umount2,
                    libc::SYS_pivot_root,
                    libc::SYS_chroot,
                    libc::SYS_open_tree,
                    libc::SYS_move_mount,
                    libc::SYS_mount_setattr,
                    libc::SYS_fsopen,
                    libc::SYS_fsconfig,
                    libc::SYS_fsmount,
                    libc::SYS_fspick,
                ] {
                    filter.push(instruction(BPF_JMP_JEQ_K, 0, 1, syscall as u32));
                    filter.push(instruction(
                        BPF_RET_K,
                        0,
                        0,
                        SECCOMP_RET_ERRNO | libc::EPERM as u32,
                    ));
                }
            } else {
                // Scope control and migration never enter the target's
                // authority. Also deny cgroup namespace creation explicitly.
                filter.extend([
                    instruction(BPF_JMP_JEQ_K, 0, 3, libc::SYS_unshare as u32),
                    instruction(BPF_LD_W_ABS, 0, 0, 16),
                    instruction(BPF_JMP_JSET_K, 0, 1, libc::CLONE_NEWCGROUP as u32),
                    instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::EPERM as u32),
                    instruction(BPF_LD_W_ABS, 0, 0, 0),
                ]);
            }
            filter.push(instruction(BPF_JMP_JEQ_K, 0, 1, libc::SYS_clone3 as u32));
            filter.push(instruction(
                BPF_RET_K,
                0,
                0,
                SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
            ));
            if contain_process_group {
                for syscall in [libc::SYS_setsid, libc::SYS_setpgid] {
                    filter.push(instruction(BPF_JMP_JEQ_K, 0, 1, syscall as u32));
                    filter.push(instruction(
                        BPF_RET_K,
                        0,
                        0,
                        SECCOMP_RET_ERRNO | libc::EPERM as u32,
                    ));
                }
            }
            filter.extend([
                instruction(BPF_JMP_JEQ_K, 0, 3, libc::SYS_clone as u32),
                instruction(BPF_LD_W_ABS, 0, 0, 16),
                instruction(
                    BPF_JMP_JSET_K,
                    0,
                    1,
                    (if nested_sandbox {
                        libc::CLONE_NEWCGROUP | libc::CLONE_UNTRACED
                    } else {
                        libc::CLONE_NEWCGROUP
                            | libc::CLONE_NEWIPC
                            | libc::CLONE_NEWNET
                            | libc::CLONE_NEWNS
                            | libc::CLONE_NEWPID
                            | libc::CLONE_NEWUSER
                            | libc::CLONE_NEWUTS
                            | libc::CLONE_UNTRACED
                    }) as u32,
                ),
                instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::EPERM as u32),
                instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
            ]);
            let mut program = libc::sock_fprog {
                len: u16::try_from(filter.len())
                    .map_err(|_| "sandbox seccomp program is too large".to_string())?,
                filter: filter.as_mut_ptr(),
            };
            syscall_zero(
                unsafe {
                    libc::prctl(
                        libc::PR_SET_SECCOMP,
                        libc::SECCOMP_MODE_FILTER,
                        &mut program as *mut libc::sock_fprog,
                    )
                },
                "install sandbox seccomp filter",
            )
        }
    }

    pub fn workspace(
        project_fd: u32,
        state_fd: u32,
        operation: LinuxOverlayWorkspaceOperation,
        max_mutations: usize,
    ) -> Result<LinuxOverlayWorkspaceObservation, String> {
        let _project = inherited_directory(project_fd, "overlay project")?;
        let state = inherited_directory(state_fd, "overlay state")?;
        if operation == LinuxOverlayWorkspaceOperation::Destroy {
            // The journal retains this exact backend-state authority until
            // destruction settles. A prior attempt may have removed either
            // child already; absence is completion, never permission to
            // recreate state. Validate the complete remaining layout first.
            let entries = state
                .entries_no_follow_bounded(3)
                .map_err(|error| format!("inventory overlay destruction state: {error}"))?;
            if entries.iter().any(|entry| {
                entry.entry_type != crate::PinnedEntryType::Directory
                    || !matches!(entry.name.to_str(), Some("upper" | "work"))
            }) {
                return Err("overlay destruction state has an invalid layout".to_string());
            }
            for entry in entries {
                let child = state
                    .open_child_directory(&entry.name)
                    .map_err(|error| format!("open overlay destruction child: {error}"))?
                    .ok_or_else(|| "overlay destruction child disappeared".to_string())?;
                child
                    .remove_contents_recursive_bounded(crate::DirectoryTraversalBudget::new(
                        max_mutations.saturating_add(2),
                        256,
                    ))
                    .map_err(|error| {
                        format!("remove overlay {:?} contents: {error:#}", entry.name)
                    })?;
                if !state
                    .remove_empty_child_if_same(&entry.name, &child)
                    .map_err(|error| format!("remove overlay destruction child: {error:#}"))?
                {
                    return Err("overlay destruction child remained non-empty".to_string());
                }
            }
            return Ok(LinuxOverlayWorkspaceObservation {
                project_identity: directory_identity(project_fd)?,
                state_identity: directory_identity(state_fd)?,
                mutation_content_root: None,
                mutations: Vec::new(),
            });
        }
        let upper = match operation {
            LinuxOverlayWorkspaceOperation::Create => state
                .open_or_create_child(OsStr::new("upper"), 0o700)
                .map_err(|error| format!("create overlay upper directory: {error}"))?,
            LinuxOverlayWorkspaceOperation::Observe | LinuxOverlayWorkspaceOperation::Destroy => {
                state
                    .open_child_directory(OsStr::new("upper"))
                    .map_err(|error| format!("open overlay upper directory: {error}"))?
                    .ok_or_else(|| "overlay upper directory is missing".to_string())?
            }
        };
        let _work = match operation {
            LinuxOverlayWorkspaceOperation::Create => state
                .open_or_create_child(OsStr::new("work"), 0o700)
                .map_err(|error| format!("create overlay work directory: {error}"))?,
            LinuxOverlayWorkspaceOperation::Observe | LinuxOverlayWorkspaceOperation::Destroy => {
                state
                    .open_child_directory(OsStr::new("work"))
                    .map_err(|error| format!("open overlay work directory: {error}"))?
                    .ok_or_else(|| "overlay work directory is missing".to_string())?
            }
        };
        let entries = state
            .entries_no_follow_bounded(3)
            .map_err(|error| format!("inventory overlay state: {error}"))?;
        if entries.len() != 2
            || entries.iter().any(|entry| {
                entry.entry_type != crate::PinnedEntryType::Directory
                    || !matches!(entry.name.to_str(), Some("upper" | "work"))
            })
        {
            return Err("overlay state has an invalid layout".to_string());
        }
        let mutations = if operation == LinuxOverlayWorkspaceOperation::Observe {
            let upper = upper
                .try_clone_descriptor()
                .map_err(|error| format!("clone overlay upper directory: {error}"))?;
            scan_overlay_upper(upper.as_raw_fd(), max_mutations)?
        } else {
            Vec::new()
        };
        Ok(LinuxOverlayWorkspaceObservation {
            project_identity: directory_identity(project_fd)?,
            state_identity: directory_identity(state_fd)?,
            mutation_content_root: (operation == LinuxOverlayWorkspaceOperation::Observe)
                .then(|| "upper".to_string()),
            mutations,
        })
    }

    fn inherited_directory(fd: u32, label: &str) -> Result<crate::PinnedDirectory, String> {
        validate_inherited_directory(fd, label)?;
        let duplicate = duplicate_fd(raw_fd(fd)?)?;
        let file = unsafe { File::from_raw_fd(duplicate) };
        crate::PinnedDirectory::from_open_directory(PathBuf::from(format!("<{label}>")), file)
            .map_err(|error| format!("pin inherited {label}: {error}"))
    }

    fn scan_overlay_upper(
        directory_fd: RawFd,
        max_mutations: usize,
    ) -> Result<Vec<LinuxOverlayMutation>, String> {
        let mut mutations = Vec::new();
        let root = inherited_directory(directory_fd as u32, "overlay scan root")?;
        let mut pending = vec![(
            String::new(),
            root.identity()
                .map_err(|error| format!("inspect overlay scan root: {error:#}"))?,
        )];
        // Keep paths, not open ancestors. The lifecycle adapter has a bounded
        // descriptor budget independent of the admitted tree's depth. Every
        // component is reopened beneath the pinned root without following links,
        // and the selected directory must still be the inode we inventoried.
        while let Some((prefix, expected)) = pending.pop() {
            let mut directory = root
                .try_clone()
                .map_err(|error| format!("clone overlay scan root: {error:#}"))?;
            for component in prefix.split('/').filter(|component| !component.is_empty()) {
                directory = directory
                    .open_child_directory(OsStr::new(component))
                    .map_err(|error| format!("reopen overlay directory {prefix}: {error:#}"))?
                    .ok_or_else(|| format!("overlay directory disappeared: {prefix}"))?;
            }
            if directory
                .identity()
                .map_err(|error| format!("inspect overlay directory {prefix}: {error:#}"))?
                != expected
            {
                return Err(format!("overlay directory changed during scan: {prefix}"));
            }
            let descriptor = directory
                .try_clone_descriptor()
                .map_err(|error| format!("clone overlay directory {prefix}: {error:#}"))?;
            scan_overlay_directory(
                descriptor.as_raw_fd(),
                &prefix,
                &mut mutations,
                &mut pending,
                max_mutations,
            )?;
        }
        mutations.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(mutations)
    }

    fn scan_overlay_directory(
        directory_fd: RawFd,
        prefix: &str,
        mutations: &mut Vec<LinuxOverlayMutation>,
        pending: &mut Vec<(String, crate::PinnedDirectoryIdentity)>,
        max_mutations: usize,
    ) -> Result<(), String> {
        let directory = inherited_directory(directory_fd as u32, "overlay scan root")?;
        let entries = directory
            .entries_no_follow_bounded(max_mutations.saturating_add(1))
            .map_err(|error| format!("read overlay upper directory: {error:#}"))?;
        for entry in entries {
            if mutations.len() >= max_mutations {
                return Err(format!("overlay delta exceeds {max_mutations} mutations"));
            }
            let name = entry
                .name
                .into_string()
                .map_err(|_| "overlay mutation path is not UTF-8".to_string())?;
            if matches!(name.as_str(), "." | "..") || name.contains('/') {
                return Err("overlay mutation has an invalid path component".to_string());
            }
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let name_c = CString::new(name).map_err(|_| "overlay path contains NUL".to_string())?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            syscall_zero(
                unsafe {
                    libc::fstatat(
                        directory_fd,
                        name_c.as_ptr(),
                        stat.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                },
                "inspect overlay mutation",
            )?;
            let stat = unsafe { stat.assume_init() };
            let kind = match stat.st_mode & libc::S_IFMT {
                libc::S_IFREG => {
                    let (size, sha256) = hash_regular_at(directory_fd, &name_c, &stat, &relative)?;
                    LinuxOverlayMutationKind::UpsertRegular {
                        normalized_mode: if stat.st_mode & 0o111 != 0 {
                            0o755
                        } else {
                            0o644
                        },
                        size,
                        sha256,
                    }
                }
                libc::S_IFLNK => LinuxOverlayMutationKind::UpsertSymlink {
                    target: read_overlay_symlink_at(directory_fd, &name_c, &stat, &relative)?,
                },
                libc::S_IFDIR => {
                    let child = unsafe {
                        libc::openat(
                            directory_fd,
                            name_c.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    if child < 0 {
                        return Err(format!(
                            "open overlay mutation directory {relative}: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                    let child = unsafe { File::from_raw_fd(child) };
                    let mut opened = std::mem::MaybeUninit::<libc::stat>::uninit();
                    syscall_zero(
                        unsafe { libc::fstat(child.as_raw_fd(), opened.as_mut_ptr()) },
                        "inspect opened overlay directory",
                    )?;
                    let opened = unsafe { opened.assume_init() };
                    if opened.st_dev != stat.st_dev || opened.st_ino != stat.st_ino {
                        return Err(format!("overlay directory changed before scan: {relative}"));
                    }
                    let opaque = directory_is_opaque(child.as_raw_fd())?;
                    let pinned = crate::PinnedDirectory::from_open_directory(
                        PathBuf::from(&relative),
                        child,
                    )
                    .map_err(|error| format!("pin overlay directory {relative}: {error:#}"))?;
                    let identity = pinned.identity().map_err(|error| {
                        format!("inspect overlay directory {relative}: {error:#}")
                    })?;
                    mutations.push(LinuxOverlayMutation {
                        path: relative.clone(),
                        kind: if opaque {
                            LinuxOverlayMutationKind::OpaqueDirectory
                        } else {
                            LinuxOverlayMutationKind::EnsureDirectory
                        },
                    });
                    pending.push((relative, identity));
                    continue;
                }
                libc::S_IFCHR if stat.st_rdev == 0 => LinuxOverlayMutationKind::DeletePath,
                _ => {
                    return Err(format!(
                        "overlay delta contains unsupported entry type at {relative}"
                    ));
                }
            };
            mutations.push(LinuxOverlayMutation {
                path: relative,
                kind,
            });
        }
        Ok(())
    }

    fn read_overlay_symlink_at(
        directory_fd: RawFd,
        name: &CStr,
        expected: &libc::stat,
        relative: &str,
    ) -> Result<String, String> {
        if expected.st_size < 1
            || expected.st_size as u64 > MAX_LINUX_OVERLAY_SYMLINK_TARGET_BYTES as u64
        {
            return Err(format!(
                "overlay symlink target exceeds its bound: {relative}"
            ));
        }
        // Pin the link inode itself. A directory-relative readlink alone could
        // read a replacement between two equal observations of the original.
        let fd = unsafe {
            libc::openat(
                directory_fd,
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(format!(
                "pin overlay symlink {relative}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let link = unsafe { File::from_raw_fd(fd) };
        let observe_link = || -> Result<libc::stat, String> {
            let mut observed = std::mem::MaybeUninit::<libc::stat>::uninit();
            syscall_zero(
                unsafe { libc::fstat(link.as_raw_fd(), observed.as_mut_ptr()) },
                "inspect pinned overlay symlink",
            )?;
            Ok(unsafe { observed.assume_init() })
        };
        let same_identity = |observed: &libc::stat| {
            observed.st_dev == expected.st_dev
                && observed.st_ino == expected.st_ino
                && observed.st_mode == expected.st_mode
                && observed.st_mode & libc::S_IFMT == libc::S_IFLNK
                && observed.st_size == expected.st_size
                && observed.st_mtime == expected.st_mtime
                && observed.st_mtime_nsec == expected.st_mtime_nsec
                && observed.st_ctime == expected.st_ctime
                && observed.st_ctime_nsec == expected.st_ctime_nsec
        };
        if !same_identity(&observe_link()?) {
            return Err(format!(
                "overlay symlink changed before target capture: {relative}"
            ));
        }
        let mut bytes = vec![0_u8; MAX_LINUX_OVERLAY_SYMLINK_TARGET_BYTES + 1];
        let read = unsafe {
            libc::readlinkat(
                link.as_raw_fd(),
                c"".as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if read < 0 {
            return Err(format!(
                "read pinned overlay symlink {relative}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if read as usize > MAX_LINUX_OVERLAY_SYMLINK_TARGET_BYTES || read as i64 != expected.st_size
        {
            return Err(format!(
                "overlay symlink target changed or exceeds its bound: {relative}"
            ));
        }
        let mut selected = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe {
                libc::fstatat(
                    directory_fd,
                    name.as_ptr(),
                    selected.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            },
            "recheck selected overlay symlink",
        )?;
        if !same_identity(&observe_link()?) || !same_identity(&unsafe { selected.assume_init() }) {
            return Err(format!(
                "overlay symlink changed during target capture: {relative}"
            ));
        }
        bytes.truncate(read as usize);
        let target = String::from_utf8(bytes)
            .map_err(|_| format!("overlay symlink target is not UTF-8: {relative}"))?;
        if target.is_empty() || target.starts_with('/') || target.as_bytes().contains(&0) {
            return Err(format!(
                "overlay symlink target is not portable relative content: {relative}"
            ));
        }
        Ok(target)
    }

    #[cfg(test)]
    mod overlay_symlink_tests {
        use super::*;

        #[test]
        fn overlay_scan_depth_does_not_consume_descriptor_budget() {
            const CHILD_ROOT: &str = "RYEOS_OVERLAY_SCAN_DESCRIPTOR_TEST_ROOT";
            if let Some(path) = std::env::var_os(CHILD_ROOT) {
                let root = crate::PinnedDirectory::open(std::path::Path::new(&path))
                    .unwrap()
                    .unwrap();
                let descriptor = root.try_clone_descriptor().unwrap();
                let limit = libc::rlimit {
                    rlim_cur: 32,
                    rlim_max: 32,
                };
                assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
                let mutations = scan_overlay_upper(descriptor.as_raw_fd(), 65).unwrap();
                assert_eq!(mutations.len(), 65);
                assert!(matches!(
                    mutations.last().unwrap().kind,
                    LinuxOverlayMutationKind::UpsertRegular { size: 5, .. }
                ));
                assert!(
                    scan_overlay_upper(descriptor.as_raw_fd(), 64)
                        .unwrap_err()
                        .contains("exceeds 64 mutations")
                );
                return;
            }
            let temporary = tempfile::tempdir().unwrap();
            let mut leaf = temporary.path().to_path_buf();
            for _ in 0..64 {
                leaf.push("child");
                std::fs::create_dir(&leaf).unwrap();
            }
            std::fs::write(leaf.join("value"), b"value").unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "overlay_scan_depth_does_not_consume_descriptor_budget",
                    "--nocapture",
                ])
                .env(CHILD_ROOT, temporary.path())
                .status()
                .unwrap();
            assert!(status.success());
        }

        #[test]
        fn captured_symlink_refuses_replaced_inode_and_overbound_identity() {
            let temporary = tempfile::tempdir().unwrap();
            let root = crate::PinnedDirectory::open(temporary.path())
                .unwrap()
                .unwrap();
            root.create_symlink(OsStr::new("link"), b"first").unwrap();
            let descriptor = root.try_clone_descriptor().unwrap();
            let mut expected = std::mem::MaybeUninit::<libc::stat>::uninit();
            assert_eq!(
                unsafe {
                    libc::fstatat(
                        descriptor.as_raw_fd(),
                        c"link".as_ptr(),
                        expected.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                },
                0
            );
            let expected = unsafe { expected.assume_init() };
            assert_eq!(
                read_overlay_symlink_at(descriptor.as_raw_fd(), c"link", &expected, "link")
                    .unwrap(),
                "first"
            );
            // Keep the selected original inode alive under another name so
            // replacement cannot accidentally reuse its inode coordinate.
            std::fs::rename(
                temporary.path().join("link"),
                temporary.path().join("old-link"),
            )
            .unwrap();
            root.create_symlink(OsStr::new("link"), b"other").unwrap();
            assert!(
                read_overlay_symlink_at(descriptor.as_raw_fd(), c"link", &expected, "link")
                    .is_err()
            );
            let mut oversized = expected;
            oversized.st_size = MAX_LINUX_OVERLAY_SYMLINK_TARGET_BYTES as i64 + 1;
            assert!(
                read_overlay_symlink_at(descriptor.as_raw_fd(), c"link", &oversized, "link")
                    .unwrap_err()
                    .contains("bound")
            );
        }
    }

    fn hash_regular_at(
        directory_fd: RawFd,
        name: &CStr,
        expected: &libc::stat,
        relative: &str,
    ) -> Result<(u64, String), String> {
        let fd = unsafe {
            libc::openat(
                directory_fd,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(format!(
                "open overlay mutation {relative}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        verify_regular_identity(&file, expected, relative)?;
        let mut digest = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| format!("read overlay mutation {relative}: {error}"))?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .ok_or_else(|| "overlay mutation size overflow".to_string())?;
            digest.update(&buffer[..read]);
        }
        let after = verify_regular_identity(&file, expected, relative)?;
        if total != after.st_size as u64 {
            return Err(format!(
                "overlay mutation changed size during scan: {relative}"
            ));
        }
        Ok((total, format!("{:x}", digest.finalize())))
    }

    fn verify_regular_identity(
        file: &File,
        expected: &libc::stat,
        relative: &str,
    ) -> Result<libc::stat, String> {
        let mut observed = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe { libc::fstat(file.as_raw_fd(), observed.as_mut_ptr()) },
            "inspect opened overlay mutation",
        )?;
        let observed = unsafe { observed.assume_init() };
        if observed.st_dev != expected.st_dev
            || observed.st_ino != expected.st_ino
            || observed.st_size != expected.st_size
            || observed.st_mode & libc::S_IFMT != libc::S_IFREG
        {
            return Err(format!(
                "overlay mutation changed identity during scan: {relative}"
            ));
        }
        Ok(observed)
    }

    fn directory_is_opaque(fd: RawFd) -> Result<bool, String> {
        for name in [c"trusted.overlay.opaque", c"user.overlay.opaque"] {
            let mut value = [0_u8; 16];
            let read = unsafe {
                libc::fgetxattr(fd, name.as_ptr(), value.as_mut_ptr().cast(), value.len())
            };
            if read > 0 && matches!(value[0], b'y' | b'Y') {
                return Ok(true);
            }
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(code) if code == libc::ENODATA || code == libc::ENOTSUP)
                {
                    return Err(format!("read overlay opaque marker: {error}"));
                }
            }
        }
        Ok(false)
    }

    fn directory_identity(fd: u32) -> Result<String, String> {
        let fd = raw_fd(fd)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe { libc::fstat(fd, stat.as_mut_ptr()) },
            "inspect directory identity",
        )?;
        let stat = unsafe { stat.assume_init() };
        Ok(format!("dev{}-ino{}", stat.st_dev, stat.st_ino))
    }

    pub fn write_descriptor(fd: u32, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_CHILD_ERROR_BYTES * 16 {
            return Err("inherited-descriptor payload exceeds Lillux bound".to_string());
        }
        let duplicate = duplicate_fd(raw_fd(fd)?)?;
        let mut file = unsafe { File::from_raw_fd(duplicate) };
        file.write_all(bytes)
            .map_err(|error| format!("write inherited descriptor: {error}"))
    }

    pub fn read_sealed_descriptor(fd: u32, max_bytes: usize) -> Result<Vec<u8>, String> {
        validate_inherited_fd(fd, "sealed input")?;
        let fd = raw_fd(fd)?;
        let required =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
        if seals < 0 || seals & required != required {
            return Err("inherited input is not sealed against mutation".to_string());
        }
        let duplicate = duplicate_fd(fd)?;
        let mut file = unsafe { File::from_raw_fd(duplicate) };
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(|error| format!("rewind sealed input: {error}"))?;
        let length = file
            .metadata()
            .map_err(|error| format!("inspect sealed input: {error}"))?
            .len();
        if length > max_bytes as u64 {
            return Err(format!("sealed input exceeds {max_bytes} bytes"));
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.read_to_end(&mut bytes)
            .map_err(|error| format!("read sealed input: {error}"))?;
        if bytes.len() > max_bytes {
            return Err(format!("sealed input exceeds {max_bytes} bytes"));
        }
        Ok(bytes)
    }

    pub fn validate_current_executable(fd: u32) -> Result<(), String> {
        validate_inherited_fd(fd, "adapter executable")?;
        let mut expected = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe { libc::fstat(raw_fd(fd)?, expected.as_mut_ptr()) },
            "inspect adapter executable descriptor",
        )?;
        let current = CString::new("/proc/self/exe").expect("static executable path");
        let mut observed = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe { libc::stat(current.as_ptr(), observed.as_mut_ptr()) },
            "inspect current adapter executable",
        )?;
        let expected = unsafe { expected.assume_init() };
        let observed = unsafe { observed.assume_init() };
        if expected.st_dev != observed.st_dev
            || expected.st_ino != observed.st_ino
            || expected.st_mode & libc::S_IFMT != libc::S_IFREG
            || observed.st_mode & libc::S_IFMT != libc::S_IFREG
        {
            return Err("adapter descriptor does not identify the current executable".to_string());
        }
        Ok(())
    }

    pub fn validate_connected_unix_stream(fd: u32) -> Result<(), String> {
        validate_inherited_fd(fd, "Unix stream")?;
        let fd = raw_fd(fd)?;
        let mut socket_type: libc::c_int = 0;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        syscall_zero(
            unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_TYPE,
                    (&mut socket_type as *mut libc::c_int).cast(),
                    &mut length,
                )
            },
            "inspect Unix stream type",
        )?;
        if socket_type != libc::SOCK_STREAM {
            return Err("target channel is not a stream socket".to_string());
        }
        let mut peer = std::mem::MaybeUninit::<libc::sockaddr_storage>::uninit();
        let mut peer_length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        syscall_zero(
            unsafe { libc::getpeername(fd, peer.as_mut_ptr().cast(), &mut peer_length) },
            "inspect Unix stream peer",
        )?;
        if unsafe { peer.assume_init() }.ss_family as libc::c_int != libc::AF_UNIX {
            return Err("target channel is not an AF_UNIX socket".to_string());
        }
        Ok(())
    }

    pub fn exit_with_status(status: LinuxSandboxExit) -> ! {
        let code = match status {
            LinuxSandboxExit::Code(code) => code.clamp(0, 255),
            LinuxSandboxExit::Signal(signal) => 128_i32.saturating_add(signal).clamp(1, 255),
        };
        unsafe { libc::_exit(code) }
    }

    fn read_child_ready(fd: RawFd) -> Result<(), String> {
        let mut kind = [0_u8; 1];
        read_exact_fd(fd, &mut kind)?;
        match kind[0] {
            CHILD_READY => Ok(()),
            CHILD_ERROR => Err(read_child_error(fd)?),
            _ => Err("sandbox child emitted an invalid readiness record".to_string()),
        }
    }

    fn read_child_target_ready(fd: RawFd) -> Result<LinuxSandboxMountPreparationReceipt, String> {
        read_child_target_ready_until(
            fd,
            crate::time::MonotonicDeadline::after(crate::time::Duration::from_secs(30)),
        )
    }

    fn read_child_target_ready_until(
        fd: RawFd,
        deadline: crate::time::MonotonicDeadline,
    ) -> Result<LinuxSandboxMountPreparationReceipt, String> {
        let mut kind = [0_u8; 1];
        read_exact_fd_until(fd, &mut kind, deadline)?;
        match kind[0] {
            CHILD_TARGET_READY => {
                let mut record = [0_u8; MOUNT_PREPARATION_RECORD_BYTES - 1];
                read_exact_fd_until(fd, &mut record, deadline)?;
                if record[0] != MOUNT_PREPARATION_SCHEMA {
                    return Err("sandbox target mount preparation has invalid schema".into());
                }
                let mount_count = u32::from_le_bytes(record[1..5].try_into().unwrap());
                if mount_count > 4096 {
                    return Err("sandbox target mount preparation exceeds native bound".into());
                }
                Ok(LinuxSandboxMountPreparationReceipt {
                    schema: u32::from(MOUNT_PREPARATION_SCHEMA),
                    owned_child_pid: 0,
                    mount_count,
                    destination_access_sha256: record[5..].try_into().unwrap(),
                })
            }
            CHILD_ERROR => Err(read_child_error_until(fd, deadline)?),
            _ => Err("sandbox child emitted an invalid target readiness record".into()),
        }
    }

    fn read_child_error(fd: RawFd) -> Result<String, String> {
        let mut length = [0_u8; 4];
        read_exact_fd(fd, &mut length)?;
        let length = u32::from_ne_bytes(length) as usize;
        if length > MAX_CHILD_ERROR_BYTES {
            return Err("sandbox child error exceeds Lillux bound".to_string());
        }
        let mut bytes = vec![0_u8; length];
        read_exact_fd(fd, &mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn read_child_error_until(
        fd: RawFd,
        deadline: crate::time::MonotonicDeadline,
    ) -> Result<String, String> {
        let mut length = [0_u8; 4];
        read_exact_fd_until(fd, &mut length, deadline)?;
        let length = u32::from_ne_bytes(length) as usize;
        if length > MAX_CHILD_ERROR_BYTES {
            return Err("sandbox child error exceeds Lillux bound".to_string());
        }
        let mut bytes = vec![0_u8; length];
        read_exact_fd_until(fd, &mut bytes, deadline)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub(super) fn write_child_error(fd: RawFd, error: &str) -> Result<(), String> {
        let bytes = error.as_bytes();
        let bytes = &bytes[..bytes.len().min(MAX_LAUNCH_FAILURE_BYTES)];
        write_all_fd(fd, &[CHILD_ERROR])?;
        write_all_fd(fd, &(bytes.len() as u32).to_ne_bytes())?;
        write_all_fd(fd, bytes)
    }

    pub(super) fn read_launch_failure(
        channel: Option<std::fs::File>,
    ) -> Result<Option<LinuxSandboxLaunchFailure>, String> {
        use std::os::fd::AsRawFd as _;
        let Some(channel) = channel else {
            return Ok(None);
        };
        let fd = channel.as_raw_fd();
        let mut kind = [0_u8; 1];
        let count = loop {
            let count = unsafe { libc::read(fd, kind.as_mut_ptr().cast(), kind.len()) };
            if count >= 0 {
                break count;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(format!("read sandbox launch-failure boundary: {error}"));
            }
        };
        if count == 0 {
            return Ok(None);
        }
        if count != 1 || kind[0] != CHILD_ERROR {
            return Err("sandbox child emitted an invalid launch-failure record".to_string());
        }
        let mut length = [0_u8; 4];
        read_exact_fd(fd, &mut length)?;
        let length = u32::from_ne_bytes(length) as usize;
        if length > MAX_LAUNCH_FAILURE_BYTES {
            return Err("sandbox launch failure exceeds Lillux bound".to_string());
        }
        let mut bytes = vec![0_u8; length];
        read_exact_fd(fd, &mut bytes)?;
        let mut trailing = [0_u8; 1];
        let trailing_count = loop {
            let count = unsafe { libc::read(fd, trailing.as_mut_ptr().cast(), trailing.len()) };
            if count >= 0 {
                break count;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(format!("finish sandbox launch-failure boundary: {error}"));
            }
        };
        if trailing_count != 0 {
            return Err("sandbox child emitted multiple launch-failure records".to_string());
        }
        Ok(Some(LinuxSandboxLaunchFailure {
            diagnostic: String::from_utf8_lossy(&bytes).into_owned(),
        }))
    }

    fn read_exact_fd(fd: RawFd, bytes: &mut [u8]) -> Result<(), String> {
        let mut read = 0;
        while read < bytes.len() {
            let count =
                unsafe { libc::read(fd, bytes[read..].as_mut_ptr().cast(), bytes.len() - read) };
            if count > 0 {
                read += count as usize;
                continue;
            }
            if count == 0 {
                return Err("sandbox child closed readiness boundary".to_string());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(format!("read sandbox readiness: {error}"));
            }
        }
        Ok(())
    }

    fn read_exact_fd_until(
        fd: RawFd,
        bytes: &mut [u8],
        deadline: crate::time::MonotonicDeadline,
    ) -> Result<(), String> {
        let mut read = 0;
        while read < bytes.len() {
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err("sandbox target readiness deadline elapsed".into());
            }
            let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
            let mut descriptor = libc::pollfd {
                fd,
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if ready == 0 {
                continue;
            }
            if ready < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(format!(
                    "poll sandbox target readiness: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let count =
                unsafe { libc::read(fd, bytes[read..].as_mut_ptr().cast(), bytes.len() - read) };
            if count > 0 {
                read += count as usize;
            } else if count == 0 {
                return Err("sandbox child closed target readiness boundary".into());
            } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return Err(format!(
                    "read sandbox target readiness: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }

    fn write_all_fd(fd: RawFd, bytes: &[u8]) -> Result<(), String> {
        let mut written = 0;
        while written < bytes.len() {
            let count =
                unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
            if count > 0 {
                written += count as usize;
                continue;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(format!("write sandbox boundary: {error}"));
            }
        }
        Ok(())
    }

    fn close_all_except(keep: &[RawFd]) -> Result<(), String> {
        let mut keep = keep
            .iter()
            .copied()
            .filter(|fd| *fd > libc::STDERR_FILENO)
            .collect::<Vec<_>>();
        keep.sort_unstable();
        keep.dedup();
        let mut first = 3_u32;
        for fd in keep {
            let fd = u32::try_from(fd).map_err(|_| "negative retained descriptor".to_string())?;
            if first < fd {
                close_range(first, fd - 1)?;
            }
            first = fd.saturating_add(1);
        }
        close_range(first, u32::MAX)
    }

    fn close_range(first: u32, last: u32) -> Result<(), String> {
        if first > last {
            return Ok(());
        }
        let result = unsafe { libc::syscall(libc::SYS_close_range, first, last, 0_u32) };
        if result == 0 {
            Ok(())
        } else {
            Err(format!(
                "close sandbox descriptor range {first}..={last}: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    fn validate_absolute_path(path: &PathBuf, label: &str) -> Result<(), String> {
        let bytes = path.as_os_str().as_bytes();
        if !path.is_absolute() || bytes.contains(&0) || bytes.len() >= libc::PATH_MAX as usize {
            return Err(format!("{label} must be a bounded absolute NUL-free path"));
        }
        let normalized = path.components().collect::<PathBuf>();
        if normalized.as_os_str().as_bytes() != bytes
            || path.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
        {
            return Err(format!("{label} must have one canonical lexical spelling"));
        }
        Ok(())
    }

    fn validate_relative_path(path: &PathBuf, label: &str) -> Result<(), String> {
        let bytes = path.as_os_str().as_bytes();
        let normalized = path.components().collect::<PathBuf>();
        if bytes.is_empty()
            || bytes.len() >= libc::PATH_MAX as usize
            || bytes.contains(&0)
            || path.is_absolute()
            || normalized.as_os_str().as_bytes() != bytes
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!("{label} must be a bounded canonical relative path"));
        }
        Ok(())
    }

    fn paths_overlap(left: &std::path::Path, right: &std::path::Path) -> bool {
        left.starts_with(right) || right.starts_with(left)
    }

    fn open_mount_target_no_symlinks(destination: &PathBuf) -> Result<File, String> {
        open_mount_destination_beneath(Path::new(ROOT), destination)
    }

    fn open_mount_destination_beneath(
        root_path: &Path,
        destination: &PathBuf,
    ) -> Result<File, String> {
        validate_absolute_path(destination, "sandbox mount destination")?;
        let root = c_string(root_path.as_os_str(), "sandbox mount root")?;
        let root_fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if root_fd < 0 {
            return Err(format!(
                "open private root for mount-target proof: {}",
                std::io::Error::last_os_error()
            ));
        }
        let root = unsafe { File::from_raw_fd(root_fd) };
        let relative = destination
            .strip_prefix("/")
            .map_err(|_| "sandbox mount destination is not absolute".to_string())?;
        let relative = c_string(relative.as_os_str(), "sandbox mount destination")?;
        let how = OpenHow {
            flags: (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: 0,
            resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                relative.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        } as RawFd;
        if fd < 0 {
            return Err(format!(
                "prove no-follow sandbox mount destination {}: {}",
                destination.display(),
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: openat2 returned a new uniquely owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn rooted(path: &PathBuf) -> Result<PathBuf, String> {
        validate_absolute_path(path, "sandbox path")?;
        let relative = path
            .strip_prefix("/")
            .map_err(|_| "sandbox path is not absolute".to_string())?;
        Ok(PathBuf::from(ROOT).join(relative))
    }

    fn ensure_directory_path(path: &PathBuf, label: &str) -> Result<(), String> {
        let c_path = c_string(path.as_os_str(), label)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe { libc::lstat(c_path.as_ptr(), stat.as_mut_ptr()) },
            &format!("inspect {label}"),
        )?;
        if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(format!("{label} is not a directory"));
        }
        Ok(())
    }

    fn ensure_regular_path(path: &PathBuf, label: &str) -> Result<(), String> {
        let c_path = c_string(path.as_os_str(), label)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        syscall_zero(
            unsafe { libc::lstat(c_path.as_ptr(), stat.as_mut_ptr()) },
            &format!("inspect {label}"),
        )?;
        if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(format!("{label} is not a regular file"));
        }
        Ok(())
    }

    fn validate_inherited_fd(fd: u32, label: &str) -> Result<(), String> {
        let fd = raw_fd(fd)?;
        if fd <= libc::STDERR_FILENO || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(format!(
                "{label} descriptor is not a live inherited descriptor"
            ));
        }
        Ok(())
    }

    fn validate_inherited_directory(fd: u32, label: &str) -> Result<(), String> {
        validate_inherited_fd(fd, label)?;
        if descriptor_kind(fd)? != DescriptorKind::Directory {
            return Err(format!("{label} descriptor is not a directory"));
        }
        Ok(())
    }

    fn raw_fd(fd: u32) -> Result<RawFd, String> {
        RawFd::try_from(fd).map_err(|_| format!("descriptor {fd} exceeds RawFd"))
    }

    fn duplicate_fd(fd: RawFd) -> Result<RawFd, String> {
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            Err(format!(
                "duplicate inherited descriptor {fd}: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(duplicate)
        }
    }

    /// Move Lillux-private control descriptors away from every exact target
    /// coordinate before fork. Target remapping may deliberately replace fd 0
    /// or any descriptor above stderr; an internal readiness pipe must never
    /// silently occupy one of those signed destinations.
    pub(super) fn relocate_internal_descriptors(
        descriptors: &mut [RawFd],
        reserved: &BTreeSet<RawFd>,
        label: &str,
    ) -> Result<(), String> {
        for index in 0..descriptors.len() {
            if !reserved.contains(&descriptors[index]) {
                continue;
            }
            let original = descriptors[index];
            let mut occupied_reservations = Vec::new();
            let replacement = loop {
                let candidate =
                    duplicate_fd(original).map_err(|error| format!("relocate {label}: {error}"))?;
                if reserved.contains(&candidate) {
                    // Keep this duplicate open while searching so F_DUPFD
                    // advances past an intentionally reserved target.
                    // SAFETY: `duplicate_fd` returned one newly owned fd.
                    occupied_reservations.push(unsafe { File::from_raw_fd(candidate) });
                    continue;
                }
                break candidate;
            };
            close_fd(original);
            descriptors[index] = replacement;
        }
        Ok(())
    }

    fn mount_raw(
        source: Option<&str>,
        target: &str,
        filesystem: Option<&str>,
        flags: libc::c_ulong,
        data: Option<&str>,
    ) -> std::io::Result<()> {
        let source = source.map(CString::new).transpose().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "mount source contains NUL",
            )
        })?;
        let target = CString::new(target).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "mount target contains NUL",
            )
        })?;
        let filesystem = filesystem.map(CString::new).transpose().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "filesystem contains NUL")
        })?;
        let data = data.map(CString::new).transpose().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "mount data contains NUL")
        })?;
        let result = unsafe {
            libc::mount(
                source
                    .as_ref()
                    .map_or(std::ptr::null(), |value| value.as_ptr()),
                target.as_ptr(),
                filesystem
                    .as_ref()
                    .map_or(std::ptr::null(), |value| value.as_ptr()),
                flags,
                data.as_ref()
                    .map_or(std::ptr::null(), |value| value.as_ptr().cast()),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn c_string(value: &OsStr, label: &str) -> Result<CString, String> {
        CString::new(value.as_bytes()).map_err(|_| format!("{label} contains NUL"))
    }

    fn path_string(path: &std::path::Path) -> Result<&str, String> {
        path.to_str()
            .ok_or_else(|| format!("sandbox path is not UTF-8: {}", path.display()))
    }

    fn place_target_channel(source: RawFd, target: RawFd) -> Result<(), String> {
        if source == target {
            // A channel already at its admitted coordinate still has to
            // survive exec. dup3 rejects equal descriptors instead of clearing
            // CLOEXEC, so preserve the other flags explicitly in this case.
            let flags = unsafe { libc::fcntl(source, libc::F_GETFD) };
            if flags < 0 {
                return Err(format!(
                    "inspect sandbox target channel: {}",
                    std::io::Error::last_os_error()
                ));
            }
            syscall_zero(
                unsafe { libc::fcntl(target, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
                "retain sandbox target channel across exec",
            )
        } else if unsafe { libc::dup3(source, target, 0) } < 0 {
            Err(format!(
                "place sandbox target channel: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            // dup3 returns the target descriptor, not zero. Do not send
            // descriptor-returning syscalls through syscall_zero.
            Ok(())
        }
    }

    /// Only for syscalls whose success value is zero, never descriptor results.
    fn syscall_zero(result: libc::c_int, label: &str) -> Result<(), String> {
        if result == 0 {
            Ok(())
        } else {
            Err(format!("{label}: {}", std::io::Error::last_os_error()))
        }
    }

    fn close_fd(fd: RawFd) {
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
    }

    #[cfg(test)]
    mod namespace_source_tests {
        use super::*;

        #[test]
        fn target_ready_receipt_is_fixed_size_and_versioned() {
            fn parse(bytes: &[u8]) -> Result<LinuxSandboxMountPreparationReceipt, String> {
                let mut pipe = [-1; 2];
                assert_eq!(
                    unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
                    0
                );
                write_all_fd(pipe[1], bytes).unwrap();
                close_fd(pipe[1]);
                let result = read_child_target_ready(pipe[0]);
                close_fd(pipe[0]);
                result
            }

            let mut record = [0_u8; MOUNT_PREPARATION_RECORD_BYTES];
            record[0] = CHILD_TARGET_READY;
            record[1] = MOUNT_PREPARATION_SCHEMA;
            record[2..6].copy_from_slice(&3_u32.to_le_bytes());
            record[6..].copy_from_slice(&[0xA5; 32]);
            let receipt = parse(&record).unwrap();
            assert_eq!(receipt.schema, 1);
            assert_eq!(receipt.owned_child_pid, 0);
            assert_eq!(receipt.mount_count, 3);
            assert_eq!(receipt.destination_access_sha256, [0xA5; 32]);
            assert!(parse(&record[..record.len() - 1]).is_err());
            record[1] = 2;
            assert!(parse(&record).unwrap_err().contains("invalid schema"));
            record[1] = MOUNT_PREPARATION_SCHEMA;
            record[2..6].copy_from_slice(&4097_u32.to_le_bytes());
            assert!(parse(&record).unwrap_err().contains("exceeds native bound"));
            record[0] = CHILD_READY;
            assert!(
                parse(&record)
                    .unwrap_err()
                    .contains("invalid target readiness")
            );

            let mut pipe = [-1; 2];
            assert_eq!(
                unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            record[0] = CHILD_TARGET_READY;
            write_all_fd(pipe[1], &record[..1]).unwrap();
            let error = read_child_target_ready_until(
                pipe[0],
                crate::time::MonotonicDeadline::after(crate::time::Duration::from_millis(5)),
            )
            .unwrap_err();
            assert!(error.contains("deadline elapsed"));
            close_fd(pipe[0]);
            close_fd(pipe[1]);
        }

        #[test]
        #[cfg(target_arch = "x86_64")]
        fn launch_failure_distinguishes_missing_elf_loader_from_workload_exit_125() {
            use std::os::fd::FromRawFd as _;
            use std::os::unix::fs::PermissionsExt as _;

            fn spawn(request: LinuxSandboxRequest) -> LinuxSandboxProcess {
                let mut pipe = [0; 2];
                assert_eq!(
                    unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
                    0
                );
                let pid = unsafe { libc::fork() };
                assert!(pid >= 0);
                if pid == 0 {
                    close_fd(pipe[0]);
                    // Bound a broken launcher fixture independently of the
                    // diagnostic reader, which deliberately waits for exit.
                    unsafe {
                        libc::signal(libc::SIGALRM, libc::SIG_DFL);
                        libc::alarm(5);
                    }
                    let error = exec_target(&request).unwrap_err();
                    let recorded = write_child_error(pipe[1], &error).is_ok();
                    unsafe { libc::_exit(if recorded { 125 } else { 126 }) };
                }
                close_fd(pipe[1]);
                LinuxSandboxProcess {
                    pid,
                    namespace_lifetime: None,
                    launch_failure: Some(unsafe { File::from_raw_fd(pipe[0]) }),
                    applied_launch: None,
                    observed_applied_launch: None,
                    mount_preparation: None,
                    termination_requested: false,
                }
            }

            let mut request = super::super::tests::minimal_request();
            request.cwd = PathBuf::from("/");
            request.executable = PathBuf::from("/bin/sh");
            request.argv0 = OsString::from("sh");
            request.arguments = vec!["-c".into(), "exit 125".into()];
            // This genuinely execs a workload returning 125. EOF is only the
            // absence of a failure record, not independent execution proof.
            assert_eq!(
                spawn(request.clone()).wait().unwrap(),
                LinuxSandboxExit::Code(125)
            );

            // Preserve the existing executable's ELF load segments, changing
            // only PT_INTERP. The kernel must report the missing loader rather
            // than misclassifying the launcher's private exit code as workload
            // completion. This fixture requires the GNU x86_64 test host.
            let mut executable = std::fs::read("/bin/true").unwrap();
            let interpreter = b"/lib64/ld-linux-x86-64.so.2\0";
            let offset = executable
                .windows(interpreter.len())
                .position(|window| window == interpreter)
                .expect("missing-loader regression requires GNU x86_64 /bin/true");
            let directory = tempfile::tempdir_in("/tmp").unwrap();
            let missing_loader = directory.path().join("ld");
            assert!(!missing_loader.exists());
            let missing = CString::new(missing_loader.as_os_str().as_encoded_bytes()).unwrap();
            let missing = missing.as_bytes_with_nul();
            assert!(missing.len() <= interpreter.len());
            executable[offset..offset + interpreter.len()].fill(0);
            executable[offset..offset + missing.len()].copy_from_slice(missing);
            let path = directory.path().join("missing-loader");
            std::fs::write(&path, executable).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();

            request.executable = path;
            request.argv0 = OsString::from("missing-loader");
            request.arguments.clear();
            let error = spawn(request).wait().unwrap_err();
            assert!(error.contains("target was not executed"), "{error}");
            assert!(error.contains("stage=exec errno=2"), "{error}");
        }

        #[test]
        fn source_backed_mount_targets_never_create_absent_overlay_children() {
            // Exercise the preparation seam used after mounting the CoW view,
            // without requiring namespace privileges. Any write here would be
            // a write to that view's upper in the real launch sequence.
            let backing = tempfile::tempdir_in(ROOT).unwrap();
            let destination = PathBuf::from("/").join(backing.path().strip_prefix(ROOT).unwrap());
            let retained = backing.path().join("retained");
            std::fs::write(&retained, b"candidate edits").unwrap();
            for (relative, kind) in [
                ("absent-file", DescriptorKind::Regular),
                ("absent-directory", DescriptorKind::Directory),
                ("missing-parent/file", DescriptorKind::Regular),
                ("missing-parent/directory", DescriptorKind::Directory),
            ] {
                assert!(
                    prepare_descriptor_mount_target(
                        &destination.join(relative),
                        kind,
                        Some(&destination),
                        &[],
                    )
                    .is_err()
                );
                assert!(!backing.path().join(relative).exists());
                assert_eq!(std::fs::read_dir(backing.path()).unwrap().count(), 1);
                assert_eq!(std::fs::read(&retained).unwrap(), b"candidate edits");
            }
        }

        #[test]
        fn source_backed_mount_targets_preserve_existing_overlay_targets() {
            let backing = tempfile::tempdir_in(ROOT).unwrap();
            let destination = PathBuf::from("/").join(backing.path().strip_prefix(ROOT).unwrap());
            std::fs::write(backing.path().join("file"), b"retained contents").unwrap();
            std::fs::create_dir(backing.path().join("directory")).unwrap();
            for (relative, kind) in [
                ("file", DescriptorKind::Regular),
                ("directory", DescriptorKind::Directory),
                ("", DescriptorKind::Directory),
            ] {
                prepare_descriptor_mount_target(
                    &destination.join(relative),
                    kind,
                    Some(&destination),
                    &[],
                )
                .unwrap();
            }
            assert_eq!(
                std::fs::read(backing.path().join("file")).unwrap(),
                b"retained contents"
            );
            assert_eq!(std::fs::read_dir(backing.path()).unwrap().count(), 2);
            for (relative, kind) in [
                ("file", DescriptorKind::Directory),
                ("directory", DescriptorKind::Regular),
            ] {
                assert!(
                    prepare_descriptor_mount_target(
                        &destination.join(relative),
                        kind,
                        Some(&destination),
                        &[],
                    )
                    .is_err()
                );
            }
            std::os::unix::fs::symlink("file", backing.path().join("link")).unwrap();
            assert!(
                prepare_descriptor_mount_target(
                    &destination.join("link"),
                    DescriptorKind::Regular,
                    Some(&destination),
                    &[],
                )
                .is_err()
            );
        }

        #[test]
        fn source_backed_mount_targets_retain_bind_source_rule_and_private_creation() {
            let backing = tempfile::tempdir_in(ROOT).unwrap();
            let destination = PathBuf::from("/").join(backing.path().strip_prefix(ROOT).unwrap());
            let preceding = [LinuxSandboxMount {
                // Target preparation does not inspect or consume source FDs.
                source_fd: 0,
                destination: destination.clone(),
                access: LinuxSandboxMountAccess::Writable,
                layer: 0,
            }];
            for kind in [DescriptorKind::Regular, DescriptorKind::Directory] {
                assert!(
                    prepare_descriptor_mount_target(
                        &destination.join("absent"),
                        kind,
                        None,
                        &preceding,
                    )
                    .is_err()
                );
                assert_eq!(std::fs::read_dir(backing.path()).unwrap().count(), 0);
            }
            // A lexical prefix is not a path ancestor. Independent namespace
            // paths still need placeholders for ordinary realization mounts.
            let unrelated = destination.join("private");
            let unrelated_bind = [LinuxSandboxMount {
                destination: unrelated.clone(),
                ..preceding[0].clone()
            }];
            prepare_descriptor_mount_target(
                &destination.join("private-other/file"),
                DescriptorKind::Regular,
                Some(&unrelated),
                &unrelated_bind,
            )
            .unwrap();
            prepare_descriptor_mount_target(
                &destination.join("private-other/directory"),
                DescriptorKind::Directory,
                Some(&unrelated),
                &unrelated_bind,
            )
            .unwrap();
            assert!(backing.path().join("private-other/file").is_file());
            assert!(backing.path().join("private-other/directory").is_dir());
        }

        #[test]
        fn target_channel_accepts_nonzero_coordinate_and_survives_exec() {
            use std::io::{Read as _, Seek as _, Write as _};

            let mut source = tempfile::tempfile().unwrap();
            source.write_all(b"exact-channel").unwrap();
            source.rewind().unwrap();
            let mut target = tempfile::tempfile().unwrap();
            assert!(target.as_raw_fd() > libc::STDERR_FILENO);
            place_target_channel(source.as_raw_fd(), target.as_raw_fd()).unwrap();
            assert_eq!(unsafe { libc::fcntl(target.as_raw_fd(), libc::F_GETFD) }, 0);
            let mut bytes = String::new();
            target.read_to_string(&mut bytes).unwrap();
            assert_eq!(bytes, "exact-channel");
        }

        #[test]
        fn target_channel_at_same_coordinate_clears_cloexec() {
            let channel = tempfile::tempfile().unwrap();
            let fd = channel.as_raw_fd();
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
            place_target_channel(fd, fd).unwrap();
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, 0);
        }

        #[test]
        fn target_channel_invalid_source_preserves_destination() {
            let target = tempfile::tempfile().unwrap();
            let flags = unsafe { libc::fcntl(target.as_raw_fd(), libc::F_GETFD) };
            assert!(place_target_channel(-1, target.as_raw_fd()).is_err());
            assert_eq!(
                unsafe { libc::fcntl(target.as_raw_fd(), libc::F_GETFD) },
                flags
            );
            assert!(place_target_channel(-1, -1).is_err());
        }

        #[test]
        fn filesystem_socket_uses_existing_mount_kind_and_exact_identity() {
            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("callback.sock");
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let original = crate::secure_fs::pin_canonical_mount_source(&path).unwrap();
            assert_eq!(
                descriptor_kind(original.inherited_descriptor().unwrap()).unwrap(),
                DescriptorKind::UnixSocket
            );
            assert!(reanchor_mount_source(original.file().as_raw_fd()).is_ok());
            std::fs::rename(&path, temporary.path().join("retained.sock")).unwrap();
            let _replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
            assert!(
                reanchor_mount_source_at(original.file().as_raw_fd(), &path)
                    .unwrap_err()
                    .contains("changed across namespace")
            );
        }

        #[test]
        fn anonymous_socket_is_not_filesystem_mount_authority() {
            let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
            assert!(reanchor_mount_source(socket.as_raw_fd()).is_err());
        }

        #[test]
        #[ignore = "requires the supported Linux user/mount/network namespace floor"]
        fn pinned_filesystem_socket_connects_across_namespace_mount() {
            use std::io::{Read as _, Write as _};

            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("callback.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let original = crate::secure_fs::pin_canonical_mount_source(&path).unwrap();
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let result = (|| {
                    enter_namespaces(LinuxSandboxNetwork::Isolated)?;
                    let source = reanchor_mount_source(original.file().as_raw_fd())?;
                    mount_private_root()?;
                    create_private_tmp()?;
                    let mount = LinuxSandboxMount {
                        source_fd: source.inherited_descriptor()?,
                        destination: PathBuf::from("/tmp/callback.sock"),
                        access: LinuxSandboxMountAccess::ReadOnly,
                        layer: 0,
                    };
                    let target = rooted(&mount.destination)?;
                    create_target(&target, descriptor_kind(mount.source_fd)?)?;
                    bind_descriptor_mount(&mount)?;
                    pivot_into_private_root()?;
                    verify_final_descriptor_mount(&mount)?;
                    let wrong_access = LinuxSandboxMount {
                        access: LinuxSandboxMountAccess::Writable,
                        ..mount.clone()
                    };
                    if verify_final_descriptor_mount(&wrong_access).is_ok() {
                        return Err("final target socket access drift was accepted".into());
                    }
                    std::os::unix::net::UnixStream::connect(&mount.destination)
                        .and_then(|mut stream| stream.write_all(b"exact socket"))
                        .map_err(|error| error.to_string())
                })();
                unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
            }
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            listener.set_nonblocking(true).unwrap();
            let (mut connection, _) = listener.accept().unwrap();
            let mut bytes = [0; 12];
            connection.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"exact socket");
        }

        #[test]
        fn namespace_reanchor_refuses_a_replacement_inode() {
            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("source");
            std::fs::write(&path, b"admitted").unwrap();
            let original = crate::secure_fs::pin_canonical_mount_source(&path).unwrap();
            assert!(reanchor_mount_source_at(original.file().as_raw_fd(), &path).is_ok());
            std::fs::rename(&path, temporary.path().join("retained")).unwrap();
            std::fs::write(&path, b"substitute").unwrap();
            assert!(
                reanchor_mount_source_at(original.file().as_raw_fd(), &path)
                    .unwrap_err()
                    .contains("changed across namespace")
            );
        }

        #[test]
        fn namespace_reanchor_refuses_symlink_substitution() {
            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("source");
            std::fs::create_dir(&path).unwrap();
            let original = crate::secure_fs::pin_canonical_mount_source(&path).unwrap();
            std::fs::rename(&path, temporary.path().join("retained")).unwrap();
            std::os::unix::fs::symlink("retained", &path).unwrap();
            assert!(reanchor_mount_source_at(original.file().as_raw_fd(), &path).is_err());
        }

        #[test]
        fn overlay_descendant_reanchor_requires_exact_relative_identity() {
            let temporary = tempfile::tempdir_in(ROOT).unwrap();
            let overlay_destination = PathBuf::from("/").join(
                temporary
                    .path()
                    .file_name()
                    .expect("temporary root has a name"),
            );
            std::fs::create_dir_all(temporary.path().join(".ai/cache/runtime-view")).unwrap();
            let mounted_root = crate::PinnedDirectory::open(temporary.path())
                .unwrap()
                .unwrap();
            let exact = mounted_root
                .open_child_directory(OsStr::new(".ai"))
                .unwrap()
                .unwrap()
                .open_child_directory(OsStr::new("cache"))
                .unwrap()
                .unwrap()
                .open_child_directory(OsStr::new("runtime-view"))
                .unwrap()
                .unwrap()
                .into_inherited_descriptor_path()
                .unwrap();
            let descendant = LinuxSandboxOverlayDescendantMount {
                source_fd: exact.inherited_descriptor().unwrap(),
                relative_path: PathBuf::from(".ai/cache/runtime-view"),
                destination: PathBuf::from("/ryeos/runtime-views/TMPDIR"),
            };
            let reopened =
                reanchor_overlay_descendant_source(&overlay_destination, &descendant).unwrap();
            assert!(exact.same_file_identity(&reopened).unwrap());

            std::fs::rename(
                temporary.path().join(".ai/cache/runtime-view"),
                temporary.path().join(".ai/cache/retained"),
            )
            .unwrap();
            std::fs::create_dir(temporary.path().join(".ai/cache/runtime-view")).unwrap();
            assert!(
                reanchor_overlay_descendant_source(&overlay_destination, &descendant)
                    .unwrap_err()
                    .contains("changed between retained authority")
            );
            std::fs::remove_dir(temporary.path().join(".ai/cache/runtime-view")).unwrap();
            std::os::unix::fs::symlink("retained", temporary.path().join(".ai/cache/runtime-view"))
                .unwrap();
            assert!(reanchor_overlay_descendant_source(&overlay_destination, &descendant).is_err());
        }

        #[test]
        fn overlay_descendant_relative_paths_are_strictly_canonical() {
            assert!(validate_relative_path(&PathBuf::from(".ai/cache/view"), "test").is_ok());
            for path in ["", "/absolute", ".", "a/../b", "a//b", "a/"] {
                assert!(
                    validate_relative_path(&PathBuf::from(path), "test").is_err(),
                    "accepted {path:?}"
                );
            }
        }

        #[test]
        fn private_mount_copy_refuses_unsealed_source() {
            let temporary = tempfile::tempfile().unwrap();
            let directory = tempfile::tempdir().unwrap();
            let staging = crate::PinnedDirectory::open(directory.path())
                .unwrap()
                .unwrap();
            assert!(!mount_source_is_sealed(temporary.as_raw_fd()).unwrap());
            assert!(
                materialize_sealed_mount_source(temporary.as_raw_fd(), &staging)
                    .unwrap_err()
                    .contains("requires a sealed source")
            );
        }

        #[test]
        fn sealed_source_cannot_gain_a_writable_second_alias() {
            let sealed = crate::sealed_memfd(c"mount-alias-test", b"immutable").unwrap();
            let mut request = super::super::tests::minimal_request();
            request.mounts = vec![
                LinuxSandboxMount {
                    source_fd: sealed.inherited_descriptor().unwrap(),
                    destination: PathBuf::from("/read-only"),
                    access: LinuxSandboxMountAccess::ReadOnly,
                    layer: 0,
                },
                LinuxSandboxMount {
                    source_fd: sealed.inherited_descriptor().unwrap(),
                    destination: PathBuf::from("/writable"),
                    access: LinuxSandboxMountAccess::Writable,
                    layer: 0,
                },
            ];
            assert!(
                reanchor_request_sources(&request)
                    .unwrap_err()
                    .contains("cannot grant writable")
            );
        }

        #[test]
        fn request_sources_cannot_recursively_capture_private_construction_mounts() {
            for path in [std::path::Path::new("/"), std::path::Path::new(ROOT)] {
                let source = crate::secure_fs::pin_canonical_mount_source(path).unwrap();
                let mut request = super::super::tests::minimal_request();
                request.mounts.push(LinuxSandboxMount {
                    source_fd: source.inherited_descriptor().unwrap(),
                    destination: PathBuf::from("/source"),
                    access: LinuxSandboxMountAccess::ReadOnly,
                    layer: 0,
                });
                assert!(
                    reanchor_request_sources(&request)
                        .unwrap_err()
                        .contains("contains the sandbox construction root")
                );
            }
            let directory = tempfile::tempdir_in(ROOT).unwrap();
            let source = crate::secure_fs::pin_canonical_mount_source(directory.path()).unwrap();
            let mut request = super::super::tests::minimal_request();
            request.mounts.push(LinuxSandboxMount {
                source_fd: source.inherited_descriptor().unwrap(),
                destination: PathBuf::from("/source"),
                access: LinuxSandboxMountAccess::ReadOnly,
                layer: 0,
            });
            assert!(reanchor_request_sources(&request).is_ok());
        }

        #[test]
        #[ignore = "requires the supported Linux user/mount/network namespace floor"]
        fn inherited_sources_cross_the_real_namespace_boundary() {
            inspect().unwrap();
        }

        #[test]
        #[ignore = "requires the supported unprivileged nested namespace floor"]
        fn nested_sandbox_generation_is_actually_qualified() {
            // An ordinary inspection may legitimately report nested support
            // unavailable. This test must not pass merely because that weaker
            // capability set was successfully inspected.
            assert!(inspect().unwrap().nested_sandbox);
        }

        // Invoked only by the isolated test-harness exec below. Ordinary test
        // runs do nothing here; no host executable discovery enters production.
        #[test]
        fn pid_proc_after_exec_target() {
            let Ok(stage) = std::env::var("LILLUX_PROC_EXEC_PROBE") else {
                return;
            };
            if let Some(code) = stage.strip_prefix("observe-code-") {
                assert_eq!(unsafe { libc::getpid() }, 1);
                unsafe { libc::_exit(code.parse::<i32>().unwrap()) };
            }
            if stage == "observe-pending" {
                std::fs::write("/work/writer-ready", b"ready").unwrap();
                loop {
                    unsafe {
                        libc::pause();
                    }
                }
            }
            if stage == "observe-descendant" {
                assert_eq!(unsafe { libc::getpid() }, 1);
                let pid = unsafe { libc::fork() };
                assert!(pid >= 0);
                if pid == 0 {
                    // Deliberately retain inherited stdout/stderr while writing.
                    // Parent target exit must end this namespace writer too.
                    for counter in 0_u64.. {
                        std::fs::write("/work/writer", counter.to_le_bytes()).unwrap();
                        std::fs::write("/work/writer-ready", b"ready").unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    unreachable!();
                }
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !std::path::Path::new("/work/writer-ready").exists() {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                unsafe { libc::_exit(0) };
            }
            if stage == "terminal-writer" {
                use std::io::{Read as _, Write as _};
                let mut input = [0_u8; 15];
                std::io::stdin().read_exact(&mut input).unwrap();
                assert_eq!(&input, b"candidate-input");
                std::io::stdout().write_all(b"candidate-output\n").unwrap();
                std::io::stdout().flush().unwrap();
                // The directory-realization case deliberately does not mount
                // /probe. Re-execute the exact path selected by this fixture,
                // including the read-only runtime-directory variant.
                let executable = std::env::var_os("LILLUX_PROBE_EXECUTABLE")
                    .expect("explicit qualification executable");
                let mut descendant = std::process::Command::new(executable)
                    .args([
                        "--exact",
                        "sandbox::imp::namespace_source_tests::pid_proc_after_exec_target",
                        "--nocapture",
                    ])
                    .env("LILLUX_PROC_EXEC_PROBE", "descendant-writer")
                    .spawn()
                    .unwrap();
                // Namespace termination must include this writer, not merely
                // the process which owns the returned launch handle.
                let status = descendant.wait().unwrap();
                panic!("descendant exited before namespace termination: {status}");
            }
            if stage == "descendant-writer" {
                // libtest retains its main thread even with one test selected.
                // Enter user namespaces only in a fresh single-threaded child.
                let helper = unsafe { libc::fork() };
                assert!(helper >= 0);
                if helper > 0 {
                    let mut status = 0;
                    assert_eq!(unsafe { libc::waitpid(helper, &mut status, 0) }, helper);
                    panic!("writer helper exited before namespace termination: {status}");
                }
                let result = std::panic::catch_unwind(|| {
                    if std::env::var_os("LILLUX_PROBE_NESTED").is_some() {
                        assert!(unsafe { libc::setsid() } > 0);
                        enter_mapped_user_namespace().unwrap();
                        syscall_zero(
                            unsafe { libc::unshare(libc::CLONE_NEWPID) },
                            "terminal test nested PID namespace",
                        )
                        .unwrap();
                        let child = unsafe { libc::fork() };
                        assert!(child >= 0);
                        if child > 0 {
                            let mut status = 0;
                            unsafe {
                                libc::waitpid(child, &mut status, 0);
                            }
                            panic!("nested namespace writer exited before outer termination");
                        }
                        assert_eq!(unsafe { libc::getpid() }, 1);
                    }
                    for counter in 0_u64.. {
                        std::fs::write("/work/writer", counter.to_le_bytes()).unwrap();
                        std::fs::write("/work/writer-ready", b"ready").unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    unreachable!();
                });
                unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
            }
            if stage == "child" {
                // Check before this target opens any files. The parent made
                // this exact directory descriptor inheritable deliberately;
                // ordinary descendant exec must still close it.
                let fd: RawFd = std::env::var("LILLUX_DESCENDANT_CLOSED_FD")
                    .unwrap()
                    .parse()
                    .unwrap();
                assert!(fd >= 300);
                assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EBADF)
                );
            }
            if stage == "alias-race" {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !std::path::Path::new("/work/host-mutated").exists() {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
            verify_target_namespace_capabilities().unwrap();
            assert_eq!(unsafe { libc::getuid() }, NAMESPACE_USER_ID);
            assert_eq!(
                std::fs::read("/mutable/config.toml").unwrap(),
                b"sealed configuration"
            );
            assert!(std::fs::write("/mutable/config.toml", b"mutated").is_err());
            assert!(std::fs::rename("/mutable/config.toml", "/mutable/replaced").is_err());
            // Exercise the device surface after the real private-root exec,
            // not merely the namespace probe's construction-time opens.
            {
                use std::io::{Read as _, Write as _};
                let mut zero = [1_u8];
                std::fs::File::open("/dev/zero")
                    .unwrap()
                    .read_exact(&mut zero)
                    .unwrap();
                assert_eq!(zero, [0]);
                let error = std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/full")
                    .unwrap()
                    .write_all(b"bounded full-device probe")
                    .unwrap_err();
                assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                if std::env::var_os("LILLUX_PROBE_NESTED").is_some() {
                    let error = std::fs::File::open("/dev/tty").unwrap_err();
                    assert_eq!(error.raw_os_error(), Some(libc::ENXIO));
                } else {
                    assert!(!std::path::Path::new("/dev/tty").exists());
                }
            }
            // Exec and same-UID transitions may not regain setup authority. This
            // runs in both the initial target and its fresh-exec descendant.
            assert_eq!(
                unsafe { libc::setresuid(NAMESPACE_USER_ID, NAMESPACE_USER_ID, NAMESPACE_USER_ID) },
                0
            );
            assert_eq!(unsafe { libc::setresuid(0, 0, 0) }, -1);
            verify_target_namespace_capabilities().unwrap();
            assert_eq!(
                unsafe { libc::prctl(libc::PR_SET_SECUREBITS, 0, 0, 0, 0) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            let executable = std::env::current_exe().unwrap();
            let expected_executable = PathBuf::from(
                std::env::var_os("LILLUX_PROBE_EXECUTABLE")
                    .expect("sandbox fixture supplies its exact executable path"),
            );
            assert_eq!(executable, expected_executable);
            assert!(std::fs::File::open(&executable).is_ok());
            assert!(!std::path::Path::new(&format!("/{SEALED_STAGING_NAME}")).exists());
            let write = std::fs::OpenOptions::new()
                .write(true)
                .open(&executable)
                .unwrap_err();
            // Linux may reject the executing inode (ETXTBSY) or its sealed
            // copy's mode (EACCES, now that DAC override is dropped) before
            // checking mount writeability. Independently require ST_RDONLY;
            // an arbitrary failed write alone is not confinement evidence.
            assert!(
                matches!(
                    write.raw_os_error(),
                    Some(libc::EROFS) | Some(libc::ETXTBSY) | Some(libc::EACCES)
                ),
                "unexpected executable write refusal: {write}"
            );
            let mut filesystem = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            assert_eq!(
                unsafe {
                    libc::statvfs(
                        std::ffi::CString::new(executable.as_os_str().as_bytes())
                            .unwrap()
                            .as_ptr(),
                        filesystem.as_mut_ptr(),
                    )
                },
                0
            );
            assert_ne!(
                unsafe { filesystem.assume_init() }.f_flag & libc::ST_RDONLY,
                0
            );
            // Namespace spelling is immutable independently of descriptor
            // inheritance, including after an ordinary child exec.
            let runtime = crate::secure_fs::PinnedDirectory::open(std::path::Path::new(
                "/ryeos/realizations/runtime",
            ))
            .unwrap()
            .unwrap();
            assert_eq!(
                runtime.verified_read_only_namespace_path().unwrap(),
                PathBuf::from("/ryeos/realizations/runtime")
            );
            let member = runtime
                .open_pinned_regular(std::ffi::OsStr::new("member"), false)
                .unwrap()
                .unwrap();
            assert_eq!(
                member.verified_read_only_namespace_path().unwrap(),
                PathBuf::from("/ryeos/realizations/runtime/member")
            );
            assert_eq!(
                std::fs::read(member.path()).unwrap(),
                b"exact runtime member"
            );
            for outcome in [
                std::fs::create_dir("/replacement"),
                std::fs::rename("/ryeos", "/replacement"),
                std::fs::rename("/ryeos/realizations", "/ryeos/replacement"),
            ] {
                assert_eq!(outcome.unwrap_err().raw_os_error(), Some(libc::EROFS));
            }
            // Absolute policy files require an immutable namespace spelling,
            // not just a read-only leaf or matching bytes at verification time.
            let configuration_path = PathBuf::from("/etc/qualification-runtime/policy");
            let configuration =
                crate::secure_fs::open_pinned_regular_file_no_follow(&configuration_path).unwrap();
            assert_eq!(
                configuration.verified_read_only_namespace_path().unwrap(),
                configuration_path
            );
            assert_eq!(configuration.read_bounded(64).unwrap(), b"policy=[]\n");
            assert!(std::fs::write(&configuration_path, b"policy=[1]\n").is_err());
            assert!(std::fs::remove_file(&configuration_path).is_err());
            assert_eq!(
                std::fs::rename("/etc/qualification-runtime", "/etc/replaced-runtime")
                    .unwrap_err()
                    .raw_os_error(),
                Some(libc::EROFS)
            );
            assert_eq!(std::fs::read(&configuration_path).unwrap(), b"policy=[]\n");

            // A read-only leaf below a writable ancestor is insufficient.
            let unsafe_leaf = crate::secure_fs::open_pinned_regular_file_no_follow(
                std::path::Path::new("/tmp/readonly-probe"),
            )
            .unwrap();
            assert!(unsafe_leaf.verified_read_only_namespace_path().is_err());
            for writable in ["/tmp", "/work"] {
                let path = PathBuf::from(writable).join(format!("write-{stage}"));
                std::fs::write(&path, b"writable child mount").unwrap();
                std::fs::rename(&path, path.with_extension("renamed")).unwrap();
                let directory =
                    crate::secure_fs::PinnedDirectory::open(std::path::Path::new(writable))
                        .unwrap()
                        .unwrap();
                assert!(directory.verified_read_only_namespace_path().is_err());
                assert_eq!(
                    directory
                        .verified_writable_mount_namespace_path(std::path::Path::new(writable),)
                        .unwrap(),
                    PathBuf::from(writable),
                );
            }
            let writable = crate::secure_fs::PinnedDirectory::open(std::path::Path::new("/work"))
                .unwrap()
                .unwrap();
            assert!(
                writable
                    .verified_writable_mount_namespace_path(runtime.path())
                    .is_err()
            );
            assert!(
                writable
                    .verified_writable_mount_namespace_path(std::path::Path::new(
                        "/ryeos/realizations/runtime/alias"
                    ),)
                    .is_err()
            );
            if stage == "root" || stage == "alias-race" {
                let source =
                    crate::secure_fs::PinnedDirectory::open(std::path::Path::new("/mutable/view"))
                        .unwrap()
                        .unwrap();
                assert!(
                    source
                        .verified_writable_mount_namespace_path(source.path())
                        .is_err()
                );
                assert_eq!(
                    source
                        .verified_writable_mount_namespace_path(std::path::Path::new("/work"))
                        .unwrap(),
                    PathBuf::from("/work"),
                );
                std::fs::rename("/mutable/view", "/mutable/prior").unwrap();
                std::fs::create_dir("/mutable/view").unwrap();
                // A writable source alias can move, but cannot retarget the
                // mounted directory seen by descendants.
                source
                    .verified_writable_mount_namespace_path(std::path::Path::new("/work"))
                    .unwrap();
                let replacement =
                    crate::secure_fs::PinnedDirectory::open(std::path::Path::new("/mutable/view"))
                        .unwrap()
                        .unwrap();
                assert!(
                    replacement
                        .verified_writable_mount_namespace_path(std::path::Path::new("/work"),)
                        .is_err()
                );
                std::fs::write("/work/exact-view", b"original inode").unwrap();
                assert!(!std::path::Path::new("/mutable/view/exact-view").exists());
                assert_eq!(
                    std::fs::read("/mutable/prior/exact-view").unwrap(),
                    b"original inode"
                );
            } else {
                assert_eq!(
                    std::fs::read("/work/exact-view").unwrap(),
                    b"original inode"
                );
            }
            assert!(!std::path::Path::new("/.lillux-old-root").exists());
            if std::env::var_os("LILLUX_PROBE_NESTED").is_some() {
                verify_nested_host_controls_unavailable().unwrap();
                assert!(
                    std::fs::read_to_string("/proc/sys/kernel/overflowuid")
                        .unwrap()
                        .trim()
                        .parse::<u32>()
                        .is_ok()
                );
            } else {
                assert!(!std::path::Path::new("/proc/sys").exists());
                assert!(!std::path::Path::new("/proc/meminfo").exists());
            }
            let host_loopback: std::net::SocketAddr = std::env::var("LILLUX_PROBE_HOST_LOOPBACK")
                .expect("native fixture supplies a live enclosing-network listener")
                .parse()
                .unwrap();
            let network_error = std::net::TcpStream::connect_timeout(
                &host_loopback,
                std::time::Duration::from_millis(250),
            )
            .expect_err("isolated target reached the enclosing host network");
            assert!(
                matches!(
                    network_error.raw_os_error(),
                    Some(libc::ECONNREFUSED | libc::ENETUNREACH | libc::EHOSTUNREACH | libc::ENETDOWN)
                ),
                "isolated target failed to connect for an unrelated reason: {network_error}"
            );
            let authority_fd: RawFd = std::env::var("LILLUX_PROBE_CLOSED_FD")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::fcntl(authority_fd, libc::F_GETFD) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
            assert_eq!(
                unsafe {
                    libc::mount(
                        c"proc".as_ptr(),
                        c"/proc".as_ptr(),
                        c"proc".as_ptr(),
                        0,
                        std::ptr::null(),
                    )
                },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            if stage == "root" || stage == "alias-race" {
                assert_eq!(unsafe { libc::getpid() }, 1);
                // The launcher is outside this fresh PID namespace: inherited
                // group/session leaders therefore appear as zero here. A
                // setup-time setsid would turn these into 1 and invalidate
                // the host's exact supervised-attachment identity proof.
                assert_eq!(unsafe { libc::getpgrp() }, 0);
                assert_eq!(unsafe { libc::getsid(0) }, 0);
                use std::io::Read as _;
                let channel_fd: RawFd = std::env::var("LILLUX_PROBE_CHANNEL_FD")
                    .unwrap()
                    .parse()
                    .unwrap();
                // This exact test-only channel survived the native target
                // remapping and exec; unrelated inherited authority did not.
                assert_eq!(unsafe { libc::fcntl(channel_fd, libc::F_GETFD) }, 0);
                let channel = unsafe { File::from_raw_fd(channel_fd) };
                let mut bytes = String::new();
                channel.take(64).read_to_string(&mut bytes).unwrap();
                assert_eq!(bytes, "native-target-channel");
                use std::os::unix::process::CommandExt as _;
                if std::env::var_os("LILLUX_PROBE_NESTED").is_none() {
                    let error = std::process::Command::new(&executable)
                        .process_group(0)
                        .spawn()
                        .unwrap_err();
                    assert_eq!(error.raw_os_error(), Some(libc::EPERM));
                }
                let directory = File::open("/work").unwrap();
                let fd = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD, 300) };
                assert!(fd >= 300);
                let sentinel = unsafe { File::from_raw_fd(fd) };
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                    0
                );
                for command in [executable.as_os_str(), OsStr::new("probe")] {
                    let mut child = std::process::Command::new(command);
                    child
                        .args([
                            "--exact",
                            "sandbox::imp::namespace_source_tests::pid_proc_after_exec_target",
                            "--nocapture",
                        ])
                        .env(
                            "PATH",
                            expected_executable
                                .parent()
                                .expect("absolute sandbox executable has a parent"),
                        )
                        .env("LILLUX_PROC_EXEC_PROBE", "child")
                        .env("LILLUX_DESCENDANT_CLOSED_FD", fd.to_string());
                    if std::env::var_os("LILLUX_PROBE_NESTED").is_some() {
                        // A nested worker may launch a child in its own
                        // process group without crossing the outer scope.
                        child.process_group(0);
                    }
                    unsafe {
                        child.pre_exec(|| {
                            // Linux CLOSE_RANGE_CLOEXEC: preserve Command's
                            // error pipe until exec, but close every nonstdio
                            // descriptor in the successfully executed child.
                            // No pass_fds or descriptor-holder fallback is
                            // needed by either namespace path form.
                            const CLOSE_RANGE_CLOEXEC: u32 = 1 << 2;
                            if libc::syscall(
                                libc::SYS_close_range,
                                3_u32,
                                u32::MAX,
                                CLOSE_RANGE_CLOEXEC,
                            ) != 0
                            {
                                return Err(std::io::Error::last_os_error());
                            }
                            Ok(())
                        });
                    }
                    assert!(child.status().unwrap().success(), "command={command:?}");
                    // Child-only closure must not revoke the parent's owner.
                    assert_eq!(
                        unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_GETFD) }
                            & libc::FD_CLOEXEC,
                        0
                    );
                }
            } else {
                assert_eq!(stage, "child");
                assert!(unsafe { libc::getpid() } > 1);
                if std::env::var_os("LILLUX_PROBE_NESTED").is_some() {
                    // Namespace transitions run in a fresh single-threaded
                    // descendant, not in the multithreaded Rust test harness.
                    let pid = unsafe { libc::fork() };
                    assert!(pid >= 0);
                    if pid == 0 {
                        let result = std::panic::catch_unwind(check_exec_nested_sandbox);
                        unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
                    }
                    let mut status = 0;
                    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
                    assert!(libc::WIFEXITED(status));
                    assert_eq!(libc::WEXITSTATUS(status), 0);
                    if let Ok(arguments) = std::env::var("LILLUX_PROBE_GUEST_ARGUMENTS") {
                        let arguments: Vec<String> = serde_json::from_str(&arguments).unwrap();
                        assert!(
                            std::process::Command::new("/guest")
                                .args(arguments)
                                .status()
                                .unwrap()
                                .success(),
                            "exact external sandbox guest failed"
                        );
                        assert_eq!(
                            std::fs::read("/tmp/guest-result").unwrap(),
                            b"guest wrote private workspace"
                        );
                    }
                } else {
                    assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWUSER) }, -1);
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::EPERM)
                    );
                }
            }
        }

        // Optional third-party guest for this generic native fixture. Caller
        // supplies exact bytes/digest/arguments; no provider dependency enters
        // Lillux or the ordinary test run.
        #[test]
        fn guest_after_exec_target() {
            if std::env::var_os("LILLUX_PROBE_GUEST_ARGUMENTS").is_none() {
                return;
            }
            assert!(!std::path::Path::new("/sys/fs/cgroup").exists());
            verify_nested_host_controls_unavailable().unwrap();
            assert!(!std::path::Path::new("/.lillux-old-root").exists());
            let executable = std::env::var("LILLUX_PROBE_EXECUTABLE")
                .expect("sandbox fixture supplies its exact executable path");
            let executable = std::ffi::CString::new(executable).unwrap();
            let mut filesystem = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            assert_eq!(
                unsafe { libc::statvfs(executable.as_ptr(), filesystem.as_mut_ptr()) },
                0
            );
            assert_ne!(
                unsafe { filesystem.assume_init() }.f_flag & libc::ST_RDONLY,
                0
            );
            std::fs::write("/tmp/guest-result", b"guest wrote private workspace").unwrap();
        }

        fn check_exec_nested_sandbox() {
            assert_eq!(unsafe { libc::setsid() } < 0, false);
            enter_mapped_user_namespace().unwrap();
            syscall_zero(
                unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID) },
                "nested mount/PID namespaces",
            )
            .unwrap();
            assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWCGROUP) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            assert!(!std::path::Path::new("/sys/fs/cgroup").exists());
            let readonly_mount = std::env::var("LILLUX_PROBE_READONLY_MOUNT")
                .expect("sandbox fixture supplies its exact read-only mount path");
            // The new namespace has its own capabilities, but inherited
            // read-only mounts cannot be promoted to writable aliases.
            assert_eq!(
                mount_raw(
                    None,
                    &readonly_mount,
                    None,
                    libc::MS_REMOUNT | libc::MS_BIND,
                    None,
                )
                .unwrap_err()
                .raw_os_error(),
                Some(libc::EPERM)
            );
            // The absolute-path and PATH-search descendants share private
            // /tmp. Give each probe a fresh mountpoint, not a stale fixed name.
            let nested_proc = tempfile::tempdir_in("/tmp").unwrap();
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let result = std::panic::catch_unwind(|| {
                    assert_eq!(unsafe { libc::getpid() }, 1);
                    mount_raw(
                        Some("proc"),
                        nested_proc.path().to_str().unwrap(),
                        Some("proc"),
                        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                        Some("subset=pid"),
                    )
                    .unwrap();
                    assert_eq!(
                        std::fs::read_link(nested_proc.path().join("self")).unwrap(),
                        PathBuf::from("1")
                    );
                    assert!(!nested_proc.path().join("sys").exists());
                    assert!(!nested_proc.path().join("meminfo").exists());
                    mount_raw(
                        Some("tmpfs"),
                        "/tmp",
                        Some("tmpfs"),
                        libc::MS_NOSUID | libc::MS_NODEV,
                        Some("mode=0700"),
                    )
                    .unwrap();
                    std::fs::write("/tmp/private", b"nested writable scratch").unwrap();
                });
                unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
            }
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        }

        #[test]
        #[ignore = "executes the test harness in real Linux namespaces"]
        fn pid_proc_supports_exact_realized_and_sealed_executable_after_exec() {
            exercise_native_proc(false, false, false);
        }

        #[test]
        #[ignore = "requires real native sandbox namespaces and host-alias mutation"]
        fn native_nested_sealed_file_survives_host_mutation_and_refuses_replacement() {
            exercise_native_proc(false, false, true);
        }

        #[test]
        #[ignore = "requires real native sandbox namespaces and pidfd support"]
        fn native_namespace_terminal_export_excludes_descendant_writers() {
            exercise_native_proc(true, true, false);
        }

        #[test]
        #[ignore = "requires real native sandbox namespaces and pidfd support"]
        fn native_namespace_terminal_export_cancels_before_release() {
            exercise_native_proc(true, false, false);
        }

        #[derive(Clone, Copy, Debug)]
        enum TargetObservationCase {
            Code(i32),
            Signal,
            ExecFailure,
            PendingThenCleanup,
            Descendant,
        }

        #[test]
        #[ignore = "requires real native sandbox namespaces and pidfd support; capability refusal is not a pass"]
        fn native_held_target_exit_observation_is_distinct_from_cleanup() {
            for case in [
                TargetObservationCase::Code(0),
                TargetObservationCase::Code(7),
                TargetObservationCase::Code(125),
                TargetObservationCase::Signal,
                TargetObservationCase::ExecFailure,
                TargetObservationCase::PendingThenCleanup,
                TargetObservationCase::Descendant,
            ] {
                exercise_native_proc_with_observation(true, true, Some(case), false);
            }
        }

        fn exercise_native_proc(terminal_export: bool, release_target: bool, alias_race: bool) {
            exercise_native_proc_with_observation(terminal_export, release_target, None, alias_race);
        }

        fn exercise_native_proc_with_observation(
            terminal_export: bool,
            release_target: bool,
            observation: Option<TargetObservationCase>,
            alias_race: bool,
        ) {
            let executable = std::env::current_exe().unwrap();
            // Test-only inventory of this harness's exact loader/library
            // mappings. No host directory or PATH is exposed to the target.
            let mut libraries = std::fs::read_to_string("/proc/self/maps")
                .unwrap()
                .lines()
                .filter_map(|line| line.split_whitespace().nth(5))
                .filter(|path| path.starts_with('/') && std::path::Path::new(path) != executable)
                .map(PathBuf::from)
                .collect::<BTreeSet<_>>();
            let library_path = std::env::join_paths(
                libraries
                    .iter()
                    .filter_map(|path| path.parent())
                    .collect::<BTreeSet<_>>(),
            )
            .unwrap();
            // The test harness uses the platform ELF interpreter alias,
            // unlike the produced runtime artifact's explicit loader path.
            #[cfg(target_arch = "x86_64")]
            libraries.insert(PathBuf::from("/lib64/ld-linux-x86-64.so.2"));
            #[cfg(target_arch = "aarch64")]
            libraries.insert(PathBuf::from("/lib/ld-linux-aarch64.so.1"));
            for (sealed, nested, directory_executable) in [
                (false, false, false),
                (true, false, false),
                (false, true, false),
                (false, true, true),
            ] {
                if (observation.is_some() || alias_race)
                    && (sealed || nested || directory_executable)
                {
                    continue;
                }
                let host_loopback = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let host_loopback_address = host_loopback.local_addr().unwrap();
                let host_connection = std::net::TcpStream::connect(host_loopback_address).unwrap();
                let (accepted, _) = host_loopback.accept().unwrap();
                drop((host_connection, accepted));
                let runtime_fixture = tempfile::tempdir().unwrap();
                std::os::unix::fs::symlink("/work", runtime_fixture.path().join("alias")).unwrap();
                std::fs::write(
                    runtime_fixture.path().join("member"),
                    b"exact runtime member",
                )
                .unwrap();
                std::fs::create_dir(runtime_fixture.path().join("bin")).unwrap();
                let directory_probe = runtime_fixture.path().join("bin/probe");
                std::fs::copy(&executable, &directory_probe).unwrap();
                std::fs::set_permissions(
                    &directory_probe,
                    std::os::unix::fs::PermissionsExt::from_mode(0o700),
                )
                .unwrap();
                let runtime_source =
                    crate::secure_fs::pin_canonical_mount_source(runtime_fixture.path()).unwrap();
                let writable_fixture = tempfile::tempdir().unwrap();
                std::fs::create_dir(writable_fixture.path().join("view")).unwrap();
                std::fs::write(writable_fixture.path().join("config.toml"), b"writable origin")
                    .unwrap();
                let host_alias_view = crate::secure_fs::PinnedDirectory::open(writable_fixture.path())
                    .unwrap()
                    .unwrap();
                let host_signal_view = host_alias_view
                    .open_child_directory(std::ffi::OsStr::new("view"))
                    .unwrap()
                    .unwrap();
                let writable_parent =
                    crate::secure_fs::pin_canonical_mount_source(writable_fixture.path()).unwrap();
                let writable_source = crate::secure_fs::pin_canonical_mount_source(
                    &writable_fixture.path().join("view"),
                )
                .unwrap();
                let writer_view = crate::secure_fs::PinnedDirectory::from_open_directory(
                    writable_fixture.path().join("view"),
                    writable_source.file().try_clone().unwrap(),
                )
                .unwrap();
                let entry = if sealed {
                    crate::sealed_executable_memfd(
                        c"proc-exec-test",
                        &std::fs::read(&executable).unwrap(),
                    )
                    .unwrap()
                } else {
                    crate::secure_fs::pin_canonical_mount_source(&executable).unwrap()
                };
                let high =
                    unsafe { libc::fcntl(entry.file().as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
                assert!(high >= 200);
                let sentinel = unsafe { File::from_raw_fd(high) };
                let channel =
                    crate::sealed_memfd(c"native-channel-probe", b"native-target-channel").unwrap();
                let configuration =
                    crate::sealed_memfd(c"native-configuration-probe", b"policy=[]\n").unwrap();
                let nested_configuration =
                    crate::sealed_memfd(c"native-nested-configuration", b"sealed configuration")
                        .unwrap();
                // A test coordinate above the unrelated sentinel, not a
                // production workload-client descriptor allocation rule.
                let channel_target = u32::try_from(high + 1).unwrap();
                let retained = libraries
                    .iter()
                    .map(|path| {
                        crate::secure_fs::pin_canonical_mount_source(
                            &std::fs::canonicalize(path).unwrap(),
                        )
                        .unwrap()
                    })
                    .collect::<Vec<_>>();
                let mut request = super::super::tests::minimal_request();
                let guest = if nested {
                    std::env::var_os("LILLUX_PROBE_GUEST_FILE").map(|path| {
                        let bytes = std::fs::read(path).unwrap();
                        assert_eq!(
                            crate::sha256_hex(&bytes),
                            std::env::var("LILLUX_PROBE_GUEST_SHA256").unwrap()
                        );
                        crate::sealed_executable_memfd(c"exact-native-guest", &bytes).unwrap()
                    })
                } else {
                    None
                };
                request.executable = if directory_executable {
                    PathBuf::from("/ryeos/realizations/runtime/bin/probe")
                } else {
                    PathBuf::from("/probe")
                };
                request.argv0 = OsString::from("probe");
                request.cwd = PathBuf::from("/");
                request.proc_filesystem = if nested {
                    LinuxSandboxProcFilesystem::PidNamespaceNested
                } else {
                    LinuxSandboxProcFilesystem::PidNamespace
                };
                request.contain_process_group = !nested;
                request.nested_sandbox = nested;
                request.arguments = [
                    "--exact",
                    "sandbox::imp::namespace_source_tests::pid_proc_after_exec_target",
                    "--nocapture",
                ]
                .into_iter()
                .map(OsString::from)
                .collect();
                request.environment = [
                    (OsString::from("LD_LIBRARY_PATH"), library_path.clone()),
                    (
                        OsString::from("LILLUX_PROC_EXEC_PROBE"),
                        OsString::from("root"),
                    ),
                    (
                        OsString::from("LILLUX_PROBE_CLOSED_FD"),
                        OsString::from(sentinel.as_raw_fd().to_string()),
                    ),
                    (
                        OsString::from("LILLUX_PROBE_CHANNEL_FD"),
                        OsString::from(channel_target.to_string()),
                    ),
                    (
                        OsString::from("LILLUX_PROBE_EXECUTABLE"),
                        request.executable.clone().into_os_string(),
                    ),
                    (
                        OsString::from("LILLUX_PROBE_READONLY_MOUNT"),
                        OsString::from(if directory_executable {
                            "/ryeos/realizations/runtime"
                        } else {
                            "/probe"
                        }),
                    ),
                    (
                        OsString::from("LILLUX_PROBE_HOST_LOOPBACK"),
                        OsString::from(host_loopback_address.to_string()),
                    ),
                ]
                .into();
                if nested {
                    request
                        .environment
                        .insert(OsString::from("LILLUX_PROBE_NESTED"), OsString::from("1"));
                }
                request.target_channels =
                    vec![(channel.inherited_descriptor().unwrap(), channel_target)];
                request.mounts = libraries
                    .iter()
                    .zip(&retained)
                    .map(|(path, file)| LinuxSandboxMount {
                        source_fd: file.inherited_descriptor().unwrap(),
                        destination: path.clone(),
                        access: LinuxSandboxMountAccess::ReadOnly,
                        layer: 0,
                    })
                    .collect();
                if !directory_executable {
                    request.mounts.push(LinuxSandboxMount {
                        source_fd: entry.inherited_descriptor().unwrap(),
                        destination: PathBuf::from("/probe"),
                        access: LinuxSandboxMountAccess::ReadOnly,
                        layer: 0,
                    });
                }
                if let Some(guest) = &guest {
                    request.mounts.push(LinuxSandboxMount {
                        source_fd: guest.inherited_descriptor().unwrap(),
                        destination: PathBuf::from("/guest"),
                        access: LinuxSandboxMountAccess::ReadOnly,
                        layer: 0,
                    });
                    request.environment.insert(
                        OsString::from("LILLUX_PROBE_GUEST_ARGUMENTS"),
                        std::env::var_os("LILLUX_PROBE_GUEST_ARGUMENTS")
                            .expect("explicit guest arguments"),
                    );
                }
                request.mounts.extend([
                    LinuxSandboxMount {
                        source_fd: configuration.inherited_descriptor().unwrap(),
                        destination: PathBuf::from("/etc/qualification-runtime/policy"),
                        access: LinuxSandboxMountAccess::ReadOnly,
                        layer: 30,
                    },
                    LinuxSandboxMount {
                        source_fd: writable_parent.inherited_descriptor().unwrap(),
                        destination: PathBuf::from("/mutable"),
                        access: LinuxSandboxMountAccess::Writable,
                        layer: 0,
                    },
                    LinuxSandboxMount {
                        source_fd: nested_configuration.inherited_descriptor().unwrap(),
                        destination: PathBuf::from("/mutable/config.toml"),
                        access: LinuxSandboxMountAccess::ReadOnly,
                        layer: 20,
                    },
                    LinuxSandboxMount {
                        source_fd: runtime_source.inherited_descriptor().unwrap(),
                        destination: PathBuf::from("/ryeos/realizations/runtime"),
                        access: LinuxSandboxMountAccess::ReadOnly,
                        layer: 0,
                    },
                    LinuxSandboxMount {
                        source_fd: writable_source.inherited_descriptor().unwrap(),
                        destination: PathBuf::from("/work"),
                        access: LinuxSandboxMountAccess::Writable,
                        layer: 0,
                    },
                    LinuxSandboxMount {
                        source_fd: entry.inherited_descriptor().unwrap(),
                        destination: PathBuf::from("/tmp/readonly-probe"),
                        access: LinuxSandboxMountAccess::ReadOnly,
                        layer: 0,
                    },
                ]);
                let pid = unsafe { libc::fork() };
                assert!(pid >= 0);
                if pid == 0 {
                    if let Some(case) = observation {
                        let stage = match case {
                            TargetObservationCase::Code(code) => format!("observe-code-{code}"),
                            TargetObservationCase::Signal => "observe-pending".into(),
                            TargetObservationCase::ExecFailure => {
                                // A present regular non-executable file passes
                                // setup path checks, then fails actual exec.
                                request.executable =
                                    PathBuf::from("/ryeos/realizations/runtime/member");
                                "observe-code-0".into()
                            }
                            TargetObservationCase::PendingThenCleanup => "observe-pending".into(),
                            TargetObservationCase::Descendant => "observe-descendant".into(),
                        };
                        request
                            .environment
                            .insert(OsString::from("LILLUX_PROC_EXEC_PROBE"), stage.into());
                    } else if terminal_export {
                        request.environment.insert(
                            OsString::from("LILLUX_PROC_EXEC_PROBE"),
                            OsString::from("terminal-writer"),
                        );
                    } else if alias_race {
                        request.environment.insert(
                            OsString::from("LILLUX_PROC_EXEC_PROBE"),
                            OsString::from("alias-race"),
                        );
                    }
                    let result = (|| {
                        if !terminal_export {
                            let expected_launch = request.clone();
                            let mut process = launch(request)?;
                            if alias_race {
                                let mut original = host_alias_view
                                    .open_regular(std::ffi::OsStr::new("config.toml"), true)
                                    .map_err(|error| format!("open host alias: {error}"))?
                                    .ok_or("host config alias disappeared")?;
                                let replacement = host_alias_view.rename_regular_child_noreplace_atomic(
                                    std::ffi::OsStr::new("config.toml"),
                                    std::ffi::OsStr::new("prior-config.toml"),
                                    &original,
                                );
                                if replacement.is_ok() {
                                    return Err("host replacement unexpectedly displaced the mounted source".into());
                                }
                                original.set_len(0).map_err(|error| format!("truncate host alias: {error}"))?;
                                use std::io::Write as _;
                                original.write_all(b"host mutated origin")
                                    .map_err(|error| format!("write host alias: {error}"))?;
                                host_signal_view.atomic_create_regular(
                                    std::ffi::OsStr::new("host-mutated"),
                                    b"ready",
                                    0o600,
                                ).map_err(|error| format!("signal host mutation: {error}"))?
                                    .ok_or("host mutation signal already exists")?;
                            }
                            let preparation = process.mount_preparation_receipt()?;
                            if preparation.schema != 1
                                || preparation.mount_count == 0
                                || preparation.owned_child_pid != process.child_pid()
                                || !preparation.matches_request(&expected_launch)?
                            {
                                return Err("native mount preparation differs from target".into());
                            }
                            let mut tampered = preparation.clone();
                            tampered.destination_access_sha256[0] ^= 1;
                            if tampered.matches_request(&expected_launch)? {
                                return Err("changed mount preparation was accepted".into());
                            }
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_secs(5);
                            loop {
                                if let Some(applied) = process.try_observe_applied_launch()? {
                                    if applied.owned_child_pid != process.child_pid()
                                        || !applied.matches_request(&expected_launch)?
                                        || !applied.matches_post_release_mounts(
                                            &imp::expected_mount_preparation(&expected_launch)?,
                                        )
                                    {
                                        return Err(
                                            "native applied launch differs from exact request"
                                                .into(),
                                        );
                                    }
                                    break;
                                }
                                if std::time::Instant::now() >= deadline {
                                    return Err(
                                        "native target did not reach applied-launch boundary"
                                            .into(),
                                    );
                                }
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            }
                            return process.wait();
                        }
                        // Missing ambient stdio must not let internal release
                        // or readiness authority be allocated at fds 0..2.
                        if sealed {
                            unsafe {
                                libc::close(0);
                                libc::close(1);
                                libc::close(2);
                            }
                        }
                        request.target_channels.clear();
                        let expected_launch = request.clone();
                        let (mut process, mut pipes) = prepare_linux_sandbox_piped(request)?;
                        let preparation = process.process.mount_preparation_receipt()?;
                        if preparation.schema != 1
                            || preparation.mount_count == 0
                            || preparation.owned_child_pid != process.process.child_pid()
                            || !preparation.matches_request(&expected_launch)?
                        {
                            return Err("held mount preparation differs from target".into());
                        }
                        if process.try_observe_target_exit().is_ok() {
                            return Err("unreleased target yielded an execution observation".into());
                        }
                        use std::io::{Read as _, Write as _};
                        pipes
                            .stdin
                            .write_all(b"candidate-input")
                            .map_err(|error| error.to_string())?;
                        // Native setup mounts its private root over /tmp in
                        // this launcher namespace. The original temp pathname
                        // no longer names our view; retain descriptor authority.
                        let writer_ready = || {
                            writer_view
                                .open_regular(std::ffi::OsStr::new("writer-ready"), false)
                                .map(|file| file.is_some())
                                .map_err(|error| error.to_string())
                        };
                        let writer_bytes = || {
                            writer_view
                                .open_pinned_regular(std::ffi::OsStr::new("writer"), false)
                                .and_then(|file| {
                                    file.ok_or_else(|| anyhow::anyhow!("writer absent"))
                                })
                                .and_then(|file| file.read_bounded(8))
                                .map_err(|error| error.to_string())
                        };
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        if writer_ready()? {
                            return Err("held native target executed before release".into());
                        }
                        if !release_target {
                            // Exercise a valid but exhausted observation budget,
                            // not merely validation of Duration::ZERO. Signalling
                            // may already have killed init; without observing and
                            // exclusively reaping it no proof may escape, and
                            // the exact lifetime must remain owned for retry.
                            let owned_pid = process.process.child_pid();
                            let timeout_error = process
                                .terminate_namespace_for_export(std::time::Duration::from_nanos(1))
                                .err()
                                .ok_or("expired observation budget produced terminal proof")?;
                            if !timeout_error.contains("unproved at deadline")
                                || process.process.child_pid() != owned_pid
                                || process.process.namespace_lifetime.is_none()
                            {
                                return Err(
                                    "termination timeout discarded exact lifetime authority".into(),
                                );
                            }
                            process.terminate_namespace_for_export(
                                std::time::Duration::from_secs(5),
                            )?;
                            if writer_ready()? || process.release_once().is_ok() {
                                return Err(
                                    "cancelled held target retained release authority".into()
                                );
                            }
                            return Ok(LinuxSandboxExit::Code(0));
                        }
                        if process
                            .terminate_namespace_for_export(std::time::Duration::ZERO)
                            .is_ok()
                        {
                            return Err("invalid deadline produced terminal proof".into());
                        }
                        process.release_once()?;
                        if process.release_once().is_ok() {
                            return Err("native release was not single use".into());
                        }
                        let applied_deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(5);
                        let applied = loop {
                            if let Some(receipt) = process.try_observe_applied_launch()? {
                                break receipt;
                            }
                            if std::time::Instant::now() >= applied_deadline {
                                return Err(
                                    "native target did not reach applied-launch boundary".into()
                                );
                            }
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        };
                        if applied.owned_child_pid != process.process.child_pid()
                            || !applied.matches_request(&expected_launch)?
                            || !applied.matches_post_release_mounts(
                                &imp::expected_mount_preparation(&expected_launch)?,
                            )
                        {
                            return Err("native applied launch differs from exact request".into());
                        }
                        if let Some(case) = observation {
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_secs(5);
                            if matches!(case, TargetObservationCase::Signal) {
                                while !writer_ready()? {
                                    if process.try_observe_target_exit()?.is_some()
                                        || std::time::Instant::now() >= deadline
                                    {
                                        return Err(
                                            "signal fixture target did not become ready".into()
                                        );
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(2));
                                }
                                // Test-only external signal to the exact target;
                                // do not route this through the cleanup operation.
                                let fd = process
                                    .process
                                    .namespace_lifetime
                                    .as_ref()
                                    .unwrap()
                                    .as_raw_fd();
                                if unsafe {
                                    libc::syscall(
                                        libc::SYS_pidfd_send_signal,
                                        fd,
                                        libc::SIGKILL,
                                        std::ptr::null::<libc::siginfo_t>(),
                                        0u32,
                                    )
                                } != 0
                                {
                                    return Err(
                                        "signal fixture could not signal its exact target".into()
                                    );
                                }
                            }
                            if matches!(case, TargetObservationCase::PendingThenCleanup) {
                                while !writer_ready()? {
                                    if process.try_observe_target_exit()?.is_some()
                                        || std::time::Instant::now() >= deadline
                                    {
                                        return Err(
                                            "pending target did not remain live through readiness"
                                                .into(),
                                        );
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(2));
                                }
                                if process.try_observe_target_exit()?.is_some() {
                                    return Err("live target yielded terminal completion".into());
                                }
                                let error = process
                                    .terminate_namespace_for_export(
                                        std::time::Duration::from_nanos(1),
                                    )
                                    .err()
                                    .ok_or("exhausted cleanup budget returned proof")?;
                                if !error.contains("unproved at deadline")
                                    || !process
                                        .try_observe_target_exit()
                                        .unwrap_err()
                                        .contains("termination request")
                                {
                                    return Err("cleanup timeout became target completion".into());
                                }
                                let cleanup = process.terminate_namespace_for_export(
                                    std::time::Duration::from_secs(5),
                                )?;
                                if cleanup.exit() != LinuxSandboxExit::Signal(libc::SIGKILL)
                                    || process.try_observe_target_exit().is_ok()
                                {
                                    return Err("cleanup status became target completion".into());
                                }
                                return Ok(LinuxSandboxExit::Code(0));
                            }
                            let observed = loop {
                                if let Some(observed) = process.try_observe_target_exit()? {
                                    break observed;
                                }
                                if std::time::Instant::now() >= deadline {
                                    return Err(format!("target observation deadline: {case:?}"));
                                }
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            };
                            let expected = match case {
                                TargetObservationCase::Code(code) => LinuxSandboxExit::Code(code),
                                TargetObservationCase::Signal => {
                                    LinuxSandboxExit::Signal(libc::SIGKILL)
                                }
                                TargetObservationCase::ExecFailure => LinuxSandboxExit::Code(125),
                                TargetObservationCase::Descendant => LinuxSandboxExit::Code(0),
                                TargetObservationCase::PendingThenCleanup => unreachable!(),
                            };
                            if observed.exit() != expected
                                || observed.launch_failure().is_some()
                                    != matches!(case, TargetObservationCase::ExecFailure)
                            {
                                return Err(format!(
                                    "wrong actual target observation for {case:?}: {observed:?}"
                                ));
                            }
                            let proof = observed.into_termination();
                            if proof.exit() != expected
                                || process.try_observe_target_exit().is_ok()
                                || process
                                    .terminate_namespace_for_export(std::time::Duration::from_secs(
                                        1,
                                    ))
                                    .is_ok()
                            {
                                return Err("natural target reap was not single-use".into());
                            }
                            // Draining protocol bytes is explicit and independent
                            // of target exit; descendant-held ends must now be EOF.
                            let mut stdout = Vec::new();
                            pipes
                                .stdout
                                .read_to_end(&mut stdout)
                                .map_err(|error| error.to_string())?;
                            let mut stderr = Vec::new();
                            pipes
                                .stderr
                                .read_to_end(&mut stderr)
                                .map_err(|error| error.to_string())?;
                            if matches!(case, TargetObservationCase::Descendant) {
                                let before = writer_bytes()?;
                                std::thread::sleep(std::time::Duration::from_millis(20));
                                if before != writer_bytes()? {
                                    return Err(
                                        "natural PID-1 exit left a descendant writer".into()
                                    );
                                }
                            }
                            return Ok(LinuxSandboxExit::Code(0));
                        }
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(5);
                        while !writer_ready()? {
                            if std::time::Instant::now() >= deadline {
                                process.terminate_namespace_for_export(
                                    std::time::Duration::from_secs(5),
                                )?;
                                let mut diagnostic = String::new();
                                pipes
                                    .stderr
                                    .take(8192)
                                    .read_to_string(&mut diagnostic)
                                    .map_err(|error| error.to_string())?;
                                return Err(format!(
                                    "descendant writer did not become ready: {diagnostic}"
                                ));
                            }
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        let proof = process
                            .terminate_namespace_for_export(std::time::Duration::from_secs(5))?;
                        let mut output = String::new();
                        pipes
                            .stdout
                            .read_to_string(&mut output)
                            .map_err(|error| error.to_string())?;
                        if !output.contains("candidate-output") {
                            return Err("candidate output escaped its exact protocol pipe".into());
                        }
                        if proof.exit() != LinuxSandboxExit::Signal(libc::SIGKILL) {
                            return Err("namespace init did not terminate by exact signal".into());
                        }
                        let before = writer_bytes()?;
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let after = writer_bytes()?;
                        if before != after
                            || process
                                .terminate_namespace_for_export(std::time::Duration::from_secs(1))
                                .is_ok()
                        {
                            return Err("terminal writer exclusion was not single-use".into());
                        }
                        Ok(LinuxSandboxExit::Code(0))
                    })();
                    if let Err(error) = &result {
                        eprintln!("proc exec qualification: {error}");
                        if alias_race {
                            let _ = writer_view.atomic_create_regular(
                                std::ffi::OsStr::new("alias-error"),
                                error.as_bytes(),
                                0o600,
                            );
                        }
                    }
                    unsafe {
                        libc::_exit(if result == Ok(LinuxSandboxExit::Code(0)) {
                            0
                        } else {
                            125
                        })
                    };
                }
                let mut status = 0;
                assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
                assert!(libc::WIFEXITED(status));
                assert_eq!(
                    libc::WEXITSTATUS(status),
                    0,
                    "sealed={sealed}, nested={nested}, directory_executable={directory_executable}, alias_error={:?}",
                    std::fs::read_to_string(writable_fixture.path().join("view/alias-error"))
                );
            }
        }

        #[test]
        #[ignore = "qualifies kernel mount locks in disposable nested user/mount namespaces"]
        fn nested_mounts_cannot_reacquire_outer_setup_authority() {
            // Like the exec fixture above, namespace setup runs in a fresh
            // single-threaded child, never a thread of the Rust test harness.
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_str().unwrap().to_owned();
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let result = std::panic::catch_unwind(|| check_nested_mount_privileges(&root));
                unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
            }
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            // Every mount was private to the disposable child. The host
            // directory is still empty, with no test mount to clean up.
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }

        fn check_nested_mount_privileges(root: &str) {
            enter_mapped_user_namespace().unwrap();
            syscall_zero(
                unsafe { libc::unshare(libc::CLONE_NEWNS) },
                "test mount namespace",
            )
            .unwrap();
            mount_raw(None, "/", None, libc::MS_PRIVATE | libc::MS_REC, None).unwrap();
            mount_raw(
                Some("tmpfs"),
                root,
                Some("tmpfs"),
                libc::MS_NOSUID | libc::MS_NODEV,
                Some("mode=0755"),
            )
            .unwrap();
            let protected = format!("{root}/protected");
            let alias = format!("{root}/alias");
            let masked = format!("{root}/masked");
            let scratch = format!("{root}/scratch");
            for path in [&protected, &alias, &masked, &scratch] {
                std::fs::create_dir(path).unwrap();
            }
            std::fs::write(format!("{protected}/baseline"), b"exact baseline").unwrap();
            std::fs::write(format!("{masked}/hidden"), b"must remain hidden").unwrap();
            mount_raw(Some(&protected), &protected, None, libc::MS_BIND, None).unwrap();
            mount_raw(
                None,
                &protected,
                None,
                libc::MS_REMOUNT
                    | libc::MS_BIND
                    | libc::MS_RDONLY
                    | libc::MS_NOSUID
                    | libc::MS_NODEV,
                None,
            )
            .unwrap();
            mount_raw(
                Some("tmpfs"),
                &masked,
                Some("tmpfs"),
                libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                None,
            )
            .unwrap();
            syscall_zero(
                unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
                "test NNP",
            )
            .unwrap();
            drop_target_namespace_capabilities().unwrap();
            // This fixture intentionally installs NO seccomp filter. Refusal
            // must come from lost outer capability and inherited mount locks,
            // not our still-strict production syscall rules or host masking.
            assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNS) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            let remount = || {
                mount_raw(
                    None,
                    &protected,
                    None,
                    libc::MS_REMOUNT | libc::MS_BIND,
                    None,
                )
                .unwrap_err()
            };
            assert_eq!(remount().raw_os_error(), Some(libc::EPERM));

            // Nonzero outer identity permits a child mapping without retaining
            // CAP_SETFCAP. The child gains privileges only in its NEW namespace.
            enter_mapped_user_namespace().unwrap();
            syscall_zero(
                unsafe { libc::unshare(libc::CLONE_NEWNS) },
                "test nested mount namespace",
            )
            .unwrap();
            assert_eq!(remount().raw_os_error(), Some(libc::EPERM));
            mount_raw(Some(&protected), &alias, None, libc::MS_BIND, None).unwrap();
            assert_eq!(
                mount_raw(None, &alias, None, libc::MS_REMOUNT | libc::MS_BIND, None)
                    .unwrap_err()
                    .raw_os_error(),
                Some(libc::EPERM)
            );
            for directory in [&protected, &alias] {
                assert_eq!(
                    std::fs::write(format!("{directory}/baseline"), b"changed")
                        .unwrap_err()
                        .raw_os_error(),
                    Some(libc::EROFS)
                );
                assert_eq!(
                    std::fs::read(format!("{directory}/baseline")).unwrap(),
                    b"exact baseline"
                );
            }
            let masked_c = CString::new(masked.clone()).unwrap();
            for flags in [0, libc::MNT_DETACH] {
                assert_eq!(unsafe { libc::umount2(masked_c.as_ptr(), flags) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EINVAL)
                );
            }
            assert!(!std::path::Path::new(&format!("{masked}/hidden")).exists());
            let options = format!("mode=0700,uid={NAMESPACE_USER_ID},gid={NAMESPACE_GROUP_ID}");
            mount_raw(
                Some("tmpfs"),
                &scratch,
                Some("tmpfs"),
                libc::MS_NOSUID | libc::MS_NODEV,
                Some(&options),
            )
            .unwrap();
            std::fs::write(format!("{scratch}/private"), b"nested scratch works").unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    use std::os::fd::AsRawFd as _;

    #[test]
    #[cfg(target_os = "linux")]
    fn sealed_descriptor_reads_are_independent_of_shared_cursor_state() {
        let sealed = crate::sealed_memfd(c"repeatable-sealed-read", b"exact bytes").unwrap();
        let descriptor = sealed.inherited_descriptor().unwrap();
        assert_eq!(
            read_sealed_inherited_descriptor(descriptor, 64).unwrap(),
            b"exact bytes"
        );
        assert_eq!(
            read_sealed_inherited_descriptor(descriptor, 64).unwrap(),
            b"exact bytes"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn target_observation_requires_release_owned_lifetime_and_no_cleanup() {
        let mut held = HeldLinuxSandboxProcess {
            process: LinuxSandboxProcess {
                pid: 0,
                namespace_lifetime: None,
                launch_failure: None,
                applied_launch: None,
                observed_applied_launch: None,
                mount_preparation: None,
                termination_requested: false,
            },
            release: None,
            release_succeeded: false,
        };
        assert!(
            held.try_observe_target_exit()
                .unwrap_err()
                .contains("not been successfully released")
        );
        assert!(
            held.try_observe_applied_launch()
                .unwrap_err()
                .contains("not been successfully released")
        );
        held.release_succeeded = true;
        assert!(
            held.try_observe_target_exit()
                .unwrap_err()
                .contains("namespace-init authority")
        );
        held.process.termination_requested = true;
        assert!(
            held.try_observe_target_exit()
                .unwrap_err()
                .contains("termination request")
        );
        assert!(
            held.process
                .reap_ready_namespace()
                .unwrap_err()
                .contains("namespace-init authority")
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn applied_launch_channel_rejects_missing_partial_and_invalid_records() {
        use std::os::fd::FromRawFd as _;
        for record in [Vec::new(), vec![0xA8], vec![0xA7; 189], vec![0xFF; 188]] {
            let mut descriptors = [0; 2];
            assert_eq!(
                unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
            let writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
            if !record.is_empty() {
                assert_eq!(
                    unsafe {
                        libc::write(writer.as_raw_fd(), record.as_ptr().cast(), record.len())
                    },
                    record.len() as isize
                );
            }
            drop(writer);
            assert!(imp::read_applied_launch(reader.as_raw_fd()).is_err());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn failed_release_never_authorizes_target_observation_or_retry() {
        let mut held = HeldLinuxSandboxProcess {
            process: LinuxSandboxProcess {
                pid: 0,
                namespace_lifetime: None,
                launch_failure: None,
                applied_launch: None,
                observed_applied_launch: None,
                mount_preparation: None,
                termination_requested: false,
            },
            // Deliberately wrong access mode: a failed private release write.
            release: Some(std::fs::File::open("/dev/null").unwrap()),
            release_succeeded: false,
        };
        assert!(
            held.release_once()
                .unwrap_err()
                .contains("release held native target")
        );
        assert!(
            held.try_observe_target_exit()
                .unwrap_err()
                .contains("not been successfully released")
        );
        assert!(
            held.release_once()
                .unwrap_err()
                .contains("already consumed")
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn terminal_export_rejects_unowned_handles_and_invalid_deadlines() {
        let mut process = LinuxSandboxProcess {
            pid: 0,
            namespace_lifetime: None,
            launch_failure: None,
            applied_launch: None,
            observed_applied_launch: None,
            mount_preparation: None,
            termination_requested: false,
        };
        for timeout in [
            std::time::Duration::ZERO,
            std::time::Duration::from_secs(61),
        ] {
            assert!(
                process
                    .terminate_namespace_for_export(timeout)
                    .unwrap_err()
                    .contains("timeout")
            );
        }
        assert!(
            process
                .terminate_namespace_for_export(std::time::Duration::from_secs(1))
                .unwrap_err()
                .contains("namespace-init authority")
        );
        // waitpid(0, ...) would wait for unrelated process-group children.
        assert!(process.wait().unwrap_err().contains("already been reaped"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn launch_failure_channel_is_exact_bounded_and_single_record() {
        use std::io::Write as _;
        use std::os::fd::FromRawFd as _;

        fn channel(bytes: &[u8]) -> std::fs::File {
            let mut descriptors = [0; 2];
            assert_eq!(
                unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
            let mut writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
            writer.write_all(bytes).unwrap();
            drop(writer);
            reader
        }

        assert!(
            imp::read_launch_failure(Some(channel(&[])))
                .unwrap()
                .is_none()
        );

        let mut exact = vec![imp::CHILD_ERROR];
        exact.extend_from_slice(&5_u32.to_ne_bytes());
        exact.extend_from_slice(b"exact");
        assert_eq!(
            imp::read_launch_failure(Some(channel(&exact)))
                .unwrap()
                .unwrap()
                .diagnostic(),
            "exact"
        );

        assert!(imp::read_launch_failure(Some(channel(&[0xff]))).is_err());

        let mut oversized = vec![imp::CHILD_ERROR];
        oversized.extend_from_slice(&((imp::MAX_LAUNCH_FAILURE_BYTES + 1) as u32).to_ne_bytes());
        assert!(imp::read_launch_failure(Some(channel(&oversized))).is_err());

        let mut repeated = exact.clone();
        repeated.extend_from_slice(&exact);
        assert!(imp::read_launch_failure(Some(channel(&repeated))).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn launch_failure_record_fits_without_a_reader_before_child_exit() {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};

        fn small_pipe() -> (std::fs::File, std::fs::File) {
            let mut descriptors = [0; 2];
            assert_eq!(
                unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
            let writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
            assert!(
                unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETPIPE_SZ, libc::PIPE_BUF) }
                    >= libc::PIPE_BUF as i32
            );
            (reader, writer)
        }

        let (failure_reader, failure_writer) = small_pipe();
        let (ready_reader, ready_writer) = small_pipe();
        let diagnostic = "x".repeat(imp::MAX_CHILD_ERROR_BYTES * 2);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Bound a regression: an oversized record would otherwise block
            // before _exit while the parent intentionally waits without reading.
            unsafe {
                libc::signal(libc::SIGALRM, libc::SIG_DFL);
                libc::alarm(5);
                libc::close(failure_reader.as_raw_fd());
                libc::close(ready_reader.as_raw_fd());
            }
            let result = imp::write_child_error(failure_writer.as_raw_fd(), &diagnostic)
                .and_then(|()| imp::write_child_error(ready_writer.as_raw_fd(), &diagnostic));
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 1 }) };
        }
        drop(failure_writer);
        drop(ready_writer);
        let mut status = 0;
        loop {
            if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
                break;
            }
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EINTR)
            );
        }
        assert!(
            libc::WIFEXITED(status),
            "failure writer did not finish: {status}"
        );
        assert_eq!(libc::WEXITSTATUS(status), 0);
        for reader in [failure_reader, ready_reader] {
            let failure = imp::read_launch_failure(Some(reader)).unwrap().unwrap();
            assert_eq!(failure.diagnostic().len(), imp::MAX_LAUNCH_FAILURE_BYTES);
            assert!(failure.diagnostic().bytes().all(|byte| byte == b'x'));
        }
    }

    pub(super) fn minimal_request() -> LinuxSandboxRequest {
        LinuxSandboxRequest {
            executable: PathBuf::from("/bin/tool"),
            argv0: OsString::from("tool"),
            arguments: Vec::new(),
            cwd: PathBuf::from("/workspace"),
            environment: BTreeMap::new(),
            mounts: Vec::new(),
            fixed_parent_views: Vec::new(),
            overlay: None,
            network: LinuxSandboxNetwork::Isolated,
            private_tmp: true,
            proc_filesystem: LinuxSandboxProcFilesystem::Empty,
            minimal_devices: true,
            character_devices: Vec::new(),
            target_channels: Vec::new(),
            lifecycle: LinuxSandboxLifecycle::Run,
            contain_process_group: true,
            nested_sandbox: false,
            aggregate_limits: None,
        }
    }

    #[test]
    fn isolated_loopback_launch_refuses_unheld_or_host_network_targets_before_contact() {
        let deadline = crate::time::MonotonicDeadline::after(crate::time::Duration::from_secs(1));
        let address = "127.0.0.1:7411".parse().unwrap();
        let error = launch_linux_sandbox_with_loopback_ingress(
            minimal_request(),
            address,
            9,
            b"attempt",
            deadline,
        )
        .unwrap_err();
        assert!(error.contains("held isolated target"));
        let mut request = minimal_request();
        request.network = LinuxSandboxNetwork::Host;
        request.lifecycle = LinuxSandboxLifecycle::AwaitRelease {
            release_fd: 10,
            release_keepalive_fd: 11,
        };
        let error =
            launch_linux_sandbox_with_loopback_ingress(request, address, 9, b"attempt", deadline)
                .unwrap_err();
        assert!(error.contains("held isolated target"));
    }

    #[test]
    fn aggregate_limits_are_not_misrepresented_as_rlimits() {
        let mut request = minimal_request();
        request.aggregate_limits = Some(LinuxSandboxAggregateLimits {
            max_processes: Some(32),
            max_memory_bytes: None,
            max_cpu_micros_per_second: None,
        });
        let error = launch_linux_sandbox(request).unwrap_err();
        assert!(error.contains("delegated cgroup-v2 authority"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn character_device_grant_rejects_wrong_identity_and_substitution() {
        let device = std::fs::File::open("/dev/null").unwrap();
        let mut request = minimal_request();
        request.character_devices = vec![LinuxSandboxCharacterDevice {
            source_fd: u32::try_from(device.as_raw_fd()).unwrap(),
            destination: PathBuf::from("/dev/selected"),
            access: crate::CharacterDeviceAccess::ReadWrite,
            major: 1,
            minor: 5,
        }];
        let error = launch_linux_sandbox(request).unwrap_err();
        assert!(error.contains("changed identity"), "{error}");

        let substitute = tempfile::tempfile().unwrap();
        let mut request = minimal_request();
        request.character_devices = vec![LinuxSandboxCharacterDevice {
            source_fd: u32::try_from(substitute.as_raw_fd()).unwrap(),
            destination: PathBuf::from("/dev/selected"),
            access: crate::CharacterDeviceAccess::ReadWrite,
            major: 1,
            minor: 3,
        }];
        let error = launch_linux_sandbox(request).unwrap_err();
        assert!(error.contains("changed identity"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_paths_reject_parent_components_before_namespace_entry() {
        let mut request = minimal_request();
        request.executable = PathBuf::from("/bin/../secret");
        let error = launch_linux_sandbox(request).unwrap_err();
        assert!(error.contains("canonical lexical spelling"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_mount_aliases_refuse_before_namespace_entry() {
        use std::os::fd::AsRawFd as _;

        let source = tempfile::tempfile().unwrap();
        for alias in ["/tool//member", "/tool/./member", "/tool/member/"] {
            let mut request = minimal_request();
            request.mounts = vec![LinuxSandboxMount {
                source_fd: source.as_raw_fd() as u32,
                destination: PathBuf::from(alias),
                access: LinuxSandboxMountAccess::ReadOnly,
                layer: 1,
            }];
            let error = launch_linux_sandbox(request).unwrap_err();
            assert!(
                error.contains("canonical lexical spelling"),
                "{alias}: {error}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn duplicate_mount_destinations_refuse_before_namespace_entry() {
        let mut request = minimal_request();
        request.mounts = vec![
            LinuxSandboxMount {
                source_fd: 999_999,
                destination: PathBuf::from("/tool"),
                access: LinuxSandboxMountAccess::ReadOnly,
                layer: 1,
            },
            LinuxSandboxMount {
                source_fd: 999_999,
                destination: PathBuf::from("/tool"),
                access: LinuxSandboxMountAccess::ReadOnly,
                layer: 2,
            },
        ];
        let error = launch_linux_sandbox(request).unwrap_err();
        assert!(error.contains("live inherited descriptor") || error.contains("unique"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn overlay_observation_retains_the_exact_mutation_content_root() {
        let temporary = tempfile::tempdir().unwrap();
        let project_path = temporary.path().join("project");
        let state_path = temporary.path().join("state");
        std::fs::create_dir_all(&project_path).unwrap();
        std::fs::create_dir_all(state_path.join("upper")).unwrap();
        std::fs::create_dir_all(state_path.join("work")).unwrap();
        std::fs::write(state_path.join("upper/result.txt"), b"retained bytes").unwrap();
        let project = crate::PinnedDirectory::open(&project_path)
            .unwrap()
            .unwrap();
        let state = crate::PinnedDirectory::open(&state_path).unwrap().unwrap();
        let project_fd = project.try_clone_descriptor().unwrap();
        let state_fd = state.try_clone_descriptor().unwrap();
        let observation = operate_linux_overlay_workspace(
            project_fd.as_raw_fd() as u32,
            state_fd.as_raw_fd() as u32,
            LinuxOverlayWorkspaceOperation::Observe,
            16,
        )
        .unwrap();
        assert_eq!(observation.mutation_content_root.as_deref(), Some("upper"));
        assert_eq!(observation.mutations.len(), 1);
        assert_eq!(
            std::fs::read(state_path.join("upper/result.txt")).unwrap(),
            b"retained bytes"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn overlay_observation_preserves_relative_links_without_following_them() {
        use std::ffi::OsStr;

        let temporary = tempfile::tempdir().unwrap();
        let project_path = temporary.path().join("project");
        let state_path = temporary.path().join("state");
        std::fs::create_dir_all(&project_path).unwrap();
        std::fs::create_dir_all(state_path.join("upper/bin")).unwrap();
        std::fs::create_dir_all(state_path.join("upper/empty")).unwrap();
        std::fs::create_dir_all(state_path.join("work")).unwrap();
        let bin = crate::PinnedDirectory::open(&state_path.join("upper/bin"))
            .unwrap()
            .unwrap();
        // No referent exists. Observation retains the target, never dereferences
        // it or interprets product-subtree containment as a kernel concern.
        bin.create_symlink(OsStr::new("program"), b"../lib/missing")
            .unwrap();
        let project = crate::PinnedDirectory::open(&project_path)
            .unwrap()
            .unwrap();
        let state = crate::PinnedDirectory::open(&state_path).unwrap().unwrap();
        let project_fd = project.try_clone_descriptor().unwrap();
        let state_fd = state.try_clone_descriptor().unwrap();
        let observe = || {
            operate_linux_overlay_workspace(
                project_fd.as_raw_fd() as u32,
                state_fd.as_raw_fd() as u32,
                LinuxOverlayWorkspaceOperation::Observe,
                8,
            )
        };
        let observed = observe().unwrap();
        assert!(observed.mutations.contains(&LinuxOverlayMutation {
            path: "bin/program".into(),
            kind: LinuxOverlayMutationKind::UpsertSymlink {
                target: "../lib/missing".into()
            },
        }));
        assert!(observed.mutations.contains(&LinuxOverlayMutation {
            path: "empty".into(),
            kind: LinuxOverlayMutationKind::EnsureDirectory,
        }));
        assert!(
            operate_linux_overlay_workspace(
                project_fd.as_raw_fd() as u32,
                state_fd.as_raw_fd() as u32,
                LinuxOverlayWorkspaceOperation::Observe,
                1
            )
            .is_err()
        );
        bin.create_symlink(OsStr::new("absolute"), b"/outside")
            .unwrap();
        assert!(observe().unwrap_err().contains("relative"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn overlay_destroy_removes_exact_upper_and_work_children() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let project_path = temporary.path().join("project");
        let state_path = temporary.path().join("state");
        std::fs::create_dir_all(&project_path).unwrap();
        std::fs::create_dir_all(state_path.join("upper/nested")).unwrap();
        std::fs::create_dir_all(state_path.join("work/work")).unwrap();
        std::fs::set_permissions(
            state_path.join("work/work"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        std::fs::write(state_path.join("upper/nested/result.txt"), b"bytes").unwrap();
        let project = crate::PinnedDirectory::open(&project_path)
            .unwrap()
            .unwrap();
        let state = crate::PinnedDirectory::open(&state_path).unwrap().unwrap();
        let project_fd = project.try_clone_descriptor().unwrap();
        let state_fd = state.try_clone_descriptor().unwrap();
        operate_linux_overlay_workspace(
            project_fd.as_raw_fd() as u32,
            state_fd.as_raw_fd() as u32,
            LinuxOverlayWorkspaceOperation::Destroy,
            16,
        )
        .unwrap();
        assert!(state.entries_no_follow().unwrap().is_empty());
        // The same retained state authority remains valid after a lost
        // success response, with neither child recreated by a retry.
        operate_linux_overlay_workspace(
            project_fd.as_raw_fd() as u32,
            state_fd.as_raw_fd() as u32,
            LinuxOverlayWorkspaceOperation::Destroy,
            16,
        )
        .unwrap();
        assert!(state.entries_no_follow().unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn overlay_destroy_resumes_partial_removal_but_refuses_foreign_layout() {
        for remaining in ["upper", "work"] {
            let temporary = tempfile::tempdir().unwrap();
            let project_path = temporary.path().join("project");
            let state_path = temporary.path().join("state");
            std::fs::create_dir(&project_path).unwrap();
            std::fs::create_dir_all(state_path.join(remaining)).unwrap();
            std::fs::write(state_path.join(remaining).join("kept"), b"owned").unwrap();
            std::fs::write(state_path.join("unexpected"), b"foreign").unwrap();
            let project = crate::PinnedDirectory::open(&project_path)
                .unwrap()
                .unwrap();
            let state = crate::PinnedDirectory::open(&state_path).unwrap().unwrap();
            let project_fd = project.try_clone_descriptor().unwrap();
            let state_fd = state.try_clone_descriptor().unwrap();
            let destroy = || {
                operate_linux_overlay_workspace(
                    project_fd.as_raw_fd() as u32,
                    state_fd.as_raw_fd() as u32,
                    LinuxOverlayWorkspaceOperation::Destroy,
                    16,
                )
            };
            assert!(destroy().unwrap_err().contains("invalid layout"));
            assert_eq!(
                std::fs::read(state_path.join(remaining).join("kept")).unwrap(),
                b"owned"
            );
            std::fs::remove_file(state_path.join("unexpected")).unwrap();
            destroy().unwrap();
            assert!(state.entries_no_follow().unwrap().is_empty());
        }
    }
}
