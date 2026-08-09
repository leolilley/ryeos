use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use anyhow::Context as _;

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
        let (socket_type, socket_state) = inspect_proc_unix_socket(fd)?;
        if socket_type != libc::SOCK_STREAM {
            anyhow::bail!("target-channel source is not a SOCK_STREAM socket");
        }
        if socket_state != 3 {
            anyhow::bail!("target-channel source is not connected");
        }
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

#[cfg(target_os = "linux")]
fn inspect_proc_unix_socket(fd: std::os::fd::RawFd) -> anyhow::Result<(libc::c_int, u8)> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a valid writable buffer and the caller retains `fd`.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        anyhow::bail!("target-channel source is not an AF_UNIX socket");
    }
    let table = std::fs::read_to_string("/proc/net/unix")
        .context("inspect Linux Unix-socket table for target channel")?;
    for line in table.lines().skip(1) {
        let columns = line.split_whitespace().collect::<Vec<_>>();
        if columns.len() < 7 || columns[6].parse::<u64>().ok() != Some(stat.st_ino) {
            continue;
        }
        let socket_type = libc::c_int::from_str_radix(columns[4], 16)
            .context("decode target-channel Unix socket type")?;
        let state = u8::from_str_radix(columns[5], 16)
            .context("decode target-channel Unix socket state")?;
        return Ok((socket_type, state));
    }
    anyhow::bail!("target-channel source is not present in the Linux AF_UNIX socket table")
}

/// Per-launch facts used to resolve policy placeholders and record provenance.
#[derive(Debug, Clone, Copy)]
pub struct IsolationLaunchContext<'a> {
    pub project_path: &'a Path,
    pub project_authority: IsolationProjectAuthority,
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
