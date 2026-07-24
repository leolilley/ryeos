use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    pub item_ref: &'a str,
    pub thread_id: &'a str,
}
