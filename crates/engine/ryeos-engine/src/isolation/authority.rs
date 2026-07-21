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
    pub verified_command: Option<&'a IsolationVerifiedCode>,
    pub item_ref: &'a str,
    pub thread_id: &'a str,
}
