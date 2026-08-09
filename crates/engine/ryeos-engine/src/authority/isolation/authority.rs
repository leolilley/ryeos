use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum IsolationLiveAccessAuthority {
    DescriptorRootedMasked {
        /// Exact live root retained from authority resolution through adapter
        /// spawn. Isolation mounts clone this descriptor; they never reopen the
        /// ambient project pathname after identity validation.
        root: Arc<lillux::PinnedDirectory>,
        root_device_id: u64,
        root_inode: u64,
        denied_control_paths: Vec<PathBuf>,
        authorized_write_namespaces: Vec<String>,
    },
    UnconfinedHost {
        authorized_write_namespaces: Vec<String>,
    },
}

impl IsolationLiveAccessAuthority {
    pub fn authorized_write_namespaces(&self) -> &[String] {
        match self {
            Self::DescriptorRootedMasked {
                authorized_write_namespaces,
                ..
            }
            | Self::UnconfinedHost {
                authorized_write_namespaces,
            } => authorized_write_namespaces,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationProjectAuthority {
    External,
    RuntimeWorkspace,
    /// Daemon-created, request-owned projectless scratch directory. This is
    /// writable but has no snapshot/fold-back semantics.
    EphemeralScratch,
    /// Pure node handler launch. The project path supplies a read-only cwd;
    /// no configured host writable mount is granted for this launch.
    ReadOnly,
}

/// Launch-owned ceiling over the node filesystem policy. Ordinary tools may
/// consume every node-policy mount they otherwise qualify for. Captured
/// execution is narrower: only its descriptor-bound verified command,
/// daemon-owned scratch workspace, and separately admitted realization mounts
/// may enter the namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationFilesystemAuthorityCeiling {
    NodePolicy,
    CapturedExecution,
}

/// Verified file identity for executable code used by one launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationVerifiedCode {
    pub source_path: PathBuf,
    pub content_hash: String,
}

/// Exact already-open executable authority carried through one isolation
/// launch. The descriptor, rather than `identity.source_path`, is the process
/// execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsolationDescriptorFileIdentity {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
    pub mode: u32,
    pub file_type: u32,
}

#[derive(Debug, Clone)]
pub struct IsolationDescriptorBoundCommand {
    identity: IsolationVerifiedCode,
    executable: Arc<std::fs::File>,
    file_identity: IsolationDescriptorFileIdentity,
}

impl IsolationDescriptorBoundCommand {
    pub fn new(
        identity: IsolationVerifiedCode,
        executable: Arc<std::fs::File>,
        file_identity: IsolationDescriptorFileIdentity,
    ) -> Self {
        Self {
            identity,
            executable,
            file_identity,
        }
    }

    pub fn identity(&self) -> &IsolationVerifiedCode {
        &self.identity
    }

    pub fn executable(&self) -> &Arc<std::fs::File> {
        &self.executable
    }

    pub fn file_identity(&self) -> IsolationDescriptorFileIdentity {
        self.file_identity
    }
}

/// Canonical command authority accepted by the isolation boundary.
///
/// Persisted/operator identities are revalidated and captured by isolation.
/// Native executors use `DescriptorBound`; they never fall back to a pathname
/// after their materialized inode has passed verification.
#[derive(Debug, Clone, Copy)]
pub enum IsolationCommandAuthorityRef<'a> {
    Revalidate(&'a IsolationVerifiedCode),
    DescriptorBound(&'a IsolationDescriptorBoundCommand),
}

impl<'a> IsolationCommandAuthorityRef<'a> {
    pub fn identity(self) -> &'a IsolationVerifiedCode {
        match self {
            Self::Revalidate(identity) => identity,
            Self::DescriptorBound(command) => command.identity(),
        }
    }
}

pub trait IsolationCommandAuthority: std::fmt::Debug + Send + Sync {
    fn authority(&self) -> IsolationCommandAuthorityRef<'_>;
}

/// One daemon-admitted immutable mount that is additional to node policy.
///
/// The opened descriptor is the authority; `source_path` is diagnostic only.
/// This is used for content-addressed realizations whose logical destination
/// is committed by the effective program.
#[derive(Debug, Clone)]
pub struct IsolationReadOnlyMountAuthority {
    source_path: PathBuf,
    destination: PathBuf,
    source: Arc<std::fs::File>,
}

impl IsolationReadOnlyMountAuthority {
    pub fn new(source_path: PathBuf, destination: PathBuf, source: std::fs::File) -> Self {
        Self {
            source_path,
            destination,
            source: Arc::new(source),
        }
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) fn source(&self) -> &Arc<std::fs::File> {
        &self.source
    }
}

impl IsolationCommandAuthority for IsolationVerifiedCode {
    fn authority(&self) -> IsolationCommandAuthorityRef<'_> {
        IsolationCommandAuthorityRef::Revalidate(self)
    }
}

impl IsolationCommandAuthority for IsolationDescriptorBoundCommand {
    fn authority(&self) -> IsolationCommandAuthorityRef<'_> {
        IsolationCommandAuthorityRef::DescriptorBound(self)
    }
}

/// One daemon-created, connected Unix stream that may be delivered to an
/// isolated target. Callers cannot construct this authority from a raw
/// descriptor, so arbitrary inherited files never acquire target-channel
/// meaning by assertion.
#[derive(Debug, Clone)]
pub struct IsolationTargetChannelAuthority {
    channel: Arc<std::fs::File>,
    env_name: String,
}

impl IsolationTargetChannelAuthority {
    #[cfg(unix)]
    pub fn new(
        channel: std::os::unix::net::UnixStream,
        env_name: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let env_name = env_name.into();
        if env_name.is_empty()
            || env_name.len() > 128
            || !env_name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_uppercase() || (index != 0 && byte.is_ascii_digit())
            })
        {
            anyhow::bail!("target-channel environment name is not canonical");
        }
        let fd = channel.as_raw_fd();
        if fd <= libc::STDERR_FILENO {
            anyhow::bail!("target-channel source descriptor overlaps stdio");
        }
        validate_connected_unix_stream(fd)?;
        let file = std::fs::File::from(OwnedFd::from(channel));
        Ok(Self {
            channel: Arc::new(file),
            env_name,
        })
    }

    pub(crate) fn channel(&self) -> &Arc<std::fs::File> {
        &self.channel
    }

    pub(crate) fn env_name(&self) -> &str {
        &self.env_name
    }
}

#[cfg(unix)]
fn validate_connected_unix_stream(fd: std::os::fd::RawFd) -> anyhow::Result<()> {
    let mut socket_type: libc::c_int = 0;
    let mut socket_type_len = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    // SAFETY: both output pointers name initialized writable storage, and the
    // caller retains the descriptor for the duration of the syscall.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut socket_type_len,
        )
    } != 0
    {
        anyhow::bail!(
            "target-channel source is not a socket: {}",
            std::io::Error::last_os_error()
        );
    }
    if socket_type != libc::SOCK_STREAM {
        anyhow::bail!("target-channel source is not a SOCK_STREAM socket");
    }

    let mut peer: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut peer_len = std::mem::size_of_val(&peer) as libc::socklen_t;
    // SAFETY: `peer` and `peer_len` are valid writable buffers.
    if unsafe {
        libc::getpeername(
            fd,
            (&mut peer as *mut libc::sockaddr_storage).cast(),
            &mut peer_len,
        )
    } != 0
    {
        anyhow::bail!(
            "target-channel source is not connected: {}",
            std::io::Error::last_os_error()
        );
    }
    if peer.ss_family as libc::c_int != libc::AF_UNIX {
        anyhow::bail!("target-channel source is not an AF_UNIX socket");
    }
    Ok(())
}

/// Per-launch facts used to resolve policy placeholders and record provenance.
#[derive(Debug, Clone, Copy)]
pub struct IsolationLaunchContext<'a> {
    pub project_path: &'a Path,
    pub project_authority: IsolationProjectAuthority,
    pub filesystem_authority_ceiling: IsolationFilesystemAuthorityCeiling,
    pub live_access: Option<&'a IsolationLiveAccessAuthority>,
    pub state_root: Option<&'a Path>,
    pub checkpoint_dir: Option<&'a Path>,
    pub daemon_socket_path: Option<&'a Path>,
    pub bundle_roots: &'a [PathBuf],
    pub node_trusted_keys_dir: Option<&'a Path>,
    pub verified_code: &'a [IsolationVerifiedCode],
    /// The one verified-code entry that must supply the process executable.
    /// Other entries may be imported tool/runtime files and cannot silently
    /// substitute for a changed command.
    pub verified_command: Option<&'a dyn IsolationCommandAuthority>,
    /// Exact read-only realization mounts admitted for this program. These
    /// are not ambient policy paths and may not be synthesized by runtimes.
    pub external_read_only_mounts: &'a [IsolationReadOnlyMountAuthority],
    pub target_channel: Option<&'a IsolationTargetChannelAuthority>,
    pub item_ref: &'a str,
    pub thread_id: &'a str,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::{FromRawFd as _, IntoRawFd as _};
    use std::os::unix::net::{UnixDatagram, UnixStream};

    #[test]
    fn target_channel_authority_requires_one_connected_unix_stream() {
        let (worker, _daemon) = UnixStream::pair().unwrap();
        IsolationTargetChannelAuthority::new(worker, "RYEOS_SESSION_FD").unwrap();

        let datagram = UnixDatagram::unbound().unwrap();
        // SAFETY: ownership of the socket descriptor moves exactly once. The
        // constructor validates the kernel socket type before retaining it.
        let forged = unsafe { UnixStream::from_raw_fd(datagram.into_raw_fd()) };
        assert!(
            IsolationTargetChannelAuthority::new(forged, "RYEOS_SESSION_FD")
                .unwrap_err()
                .to_string()
                .contains("SOCK_STREAM")
        );

        let regular = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: the owned descriptor moves exactly once. This deliberately
        // adversarial construction proves the authority validates the kernel
        // object instead of trusting the Rust wrapper's nominal type.
        let forged = unsafe { UnixStream::from_raw_fd(regular.into_raw_fd()) };
        assert!(
            IsolationTargetChannelAuthority::new(forged, "RYEOS_SESSION_FD")
                .unwrap_err()
                .to_string()
                .contains("not a socket")
        );

        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert!(raw > libc::STDERR_FILENO);
        // SAFETY: `raw` is a newly owned AF_UNIX stream descriptor.
        let unconnected = unsafe { UnixStream::from_raw_fd(raw) };
        assert!(
            IsolationTargetChannelAuthority::new(unconnected, "RYEOS_SESSION_FD")
                .unwrap_err()
                .to_string()
                .contains("not connected")
        );

        let (worker, _daemon) = UnixStream::pair().unwrap();
        assert!(
            IsolationTargetChannelAuthority::new(worker, "lowercase")
                .unwrap_err()
                .to_string()
                .contains("not canonical")
        );
    }
}
