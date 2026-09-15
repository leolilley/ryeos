use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum IsolationLiveAccessAuthority {
    DescriptorRootedFixedParents {
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
            Self::DescriptorRootedFixedParents {
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
/// daemon-owned workspace, separately admitted realization mounts, and an
/// explicitly granted exact daemon-private state root may enter the namespace.
/// That private state is independently bounded/pinned launch authority, never
/// an ambient mount inherited from the node's filesystem policy.
/// Explicit sealed node-network runtime files may also enter when the effective
/// network ceiling allows them. They carry separate node-generation provenance;
/// captured execution never inherits the general host filesystem as a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IsolationFilesystemAuthorityCeiling {
    NodePolicy,
    CapturedExecution,
}

impl IsolationFilesystemAuthorityCeiling {
    pub const REALIZATION_PROPERTY: &str = "isolation_filesystem_authority_ceiling";

    /// A child and its parent independently restrict the node's mount policy.
    /// Once either requires captured content, no later launch layer may restore
    /// ambient host mounts by choosing `node_policy`.
    pub fn intersect(self, other: Self) -> Self {
        if matches!(self, Self::CapturedExecution) || matches!(other, Self::CapturedExecution) {
            Self::CapturedExecution
        } else {
            Self::NodePolicy
        }
    }
}

/// Launch-owned narrowing of the node network ceiling. Ordinary execution
/// inherits the configured node mode. A captured local worker must remove host
/// networking even when the node permits it for unrelated admitted tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IsolationNetworkAuthorityCeiling {
    NodePolicy,
    Isolated,
}

impl IsolationNetworkAuthorityCeiling {
    pub const REALIZATION_PROPERTY: &str = "isolation_network_authority_ceiling";

    /// Irreversibly intersect two independently admitted network ceilings.
    /// `isolated` is absorbing: no later launch layer can widen an authored
    /// subject or parent execution back to node-policy networking.
    pub fn intersect(self, other: Self) -> Self {
        if matches!(self, Self::Isolated) || matches!(other, Self::Isolated) {
            Self::Isolated
        } else {
            Self::NodePolicy
        }
    }
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
    executable: lillux::InheritedDescriptorAuthority,
    file_identity: IsolationDescriptorFileIdentity,
}

/// Exact executable member of one already-admitted read-only realization
/// tree. The complete tree remains a separate mount authority; this value
/// only promotes the selected regular member to process-executable authority
/// at its realization-relative destination. Keeping both authorities is what
/// preserves sibling-relative runtime layouts without reopening a pathname.
#[derive(Debug, Clone)]
pub struct IsolationRealizationMemberCommand {
    command: IsolationDescriptorBoundCommand,
    realization_root: lillux::PinnedDirectoryIdentity,
    realization_destination: PathBuf,
}

impl IsolationRealizationMemberCommand {
    pub(crate) fn new(
        command: IsolationDescriptorBoundCommand,
        realization_root: lillux::PinnedDirectoryIdentity,
        realization_destination: PathBuf,
    ) -> Self {
        Self {
            command,
            realization_root,
            realization_destination,
        }
    }

    pub(crate) fn command(&self) -> &IsolationDescriptorBoundCommand {
        &self.command
    }

    pub(crate) fn realization_root(&self) -> lillux::PinnedDirectoryIdentity {
        self.realization_root
    }

    pub(crate) fn realization_destination(&self) -> &Path {
        &self.realization_destination
    }
}

/// Owned admitted command carried by an execution plan. This is deliberately
/// an OS-mechanical distinction only: item kinds still select commands through
/// signed runtime data, while isolation decides whether the exact descriptor
/// is a standalone executable or a member overlaid inside an admitted tree.
#[derive(Debug, Clone)]
pub enum IsolationAdmittedCommand {
    DescriptorBound(IsolationDescriptorBoundCommand),
    RealizationMember(IsolationRealizationMemberCommand),
}

impl From<IsolationDescriptorBoundCommand> for IsolationAdmittedCommand {
    fn from(command: IsolationDescriptorBoundCommand) -> Self {
        Self::DescriptorBound(command)
    }
}

impl From<IsolationRealizationMemberCommand> for IsolationAdmittedCommand {
    fn from(command: IsolationRealizationMemberCommand) -> Self {
        Self::RealizationMember(command)
    }
}

impl IsolationDescriptorBoundCommand {
    pub fn new(
        identity: IsolationVerifiedCode,
        executable: lillux::InheritedDescriptorAuthority,
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

    pub fn executable(&self) -> &lillux::InheritedDescriptorAuthority {
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
    RealizationMember(&'a IsolationRealizationMemberCommand),
}

impl<'a> IsolationCommandAuthorityRef<'a> {
    pub fn identity(self) -> &'a IsolationVerifiedCode {
        match self {
            Self::Revalidate(identity) => identity,
            Self::DescriptorBound(command) => command.identity(),
            Self::RealizationMember(command) => command.command().identity(),
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
    source: lillux::InheritedDescriptorAuthority,
    scope: IsolationReadOnlyMountScope,
}

/// One daemon-prepared writable directory for a retained session environment
/// variable.
///
/// The descriptor is the complete source authority. Callers supply only the
/// validated environment name; the namespace destination is derived by the
/// shared state owner and can never be redirected to an arbitrary path.
#[derive(Debug, Clone)]
pub struct IsolationWritableRuntimeViewMountAuthority {
    environment_name: String,
    destination: PathBuf,
    source: lillux::InheritedDescriptorAuthority,
    workspace_relative_path: Option<String>,
}

impl IsolationWritableRuntimeViewMountAuthority {
    /// Construct the ordinary direct writable-mount lane used only by a
    /// projectless scratch workspace, which has no retained workspace view.
    pub fn new(
        environment_name: String,
        source: lillux::InheritedDescriptorAuthority,
    ) -> anyhow::Result<Self> {
        Self::new_inner(environment_name, source, None)
    }

    /// Construct a directory borrowed from one retained workspace view. The
    /// child descriptor proves the exact directory now; the canonical
    /// workspace-relative coordinate lets the isolation adapter reopen that
    /// same descendant beneath its already-mounted workspace authority.
    pub fn new_workspace_descendant(
        environment_name: String,
        workspace_relative_path: String,
        source: lillux::InheritedDescriptorAuthority,
    ) -> anyhow::Result<Self> {
        ryeos_state::objects::validate_canonical_project_relative_path(&workspace_relative_path)
            .map_err(|error| anyhow::anyhow!("invalid runtime-view workspace path: {error}"))?;
        Self::new_inner(environment_name, source, Some(workspace_relative_path))
    }

    fn new_inner(
        environment_name: String,
        source: lillux::InheritedDescriptorAuthority,
        workspace_relative_path: Option<String>,
    ) -> anyhow::Result<Self> {
        let destination = ryeos_state::objects::runtime_view_mount_destination(&environment_name)?;
        source
            .directory_identity()
            .map_err(|error| anyhow::anyhow!("runtime-view source is not a directory: {error}"))?;
        Ok(Self {
            environment_name,
            destination,
            source,
            workspace_relative_path,
        })
    }

    pub fn environment_name(&self) -> &str {
        &self.environment_name
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) fn source(&self) -> &lillux::InheritedDescriptorAuthority {
        &self.source
    }

    pub(crate) fn workspace_relative_path(&self) -> Option<&str> {
        self.workspace_relative_path.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IsolationReadOnlyMountScope {
    ProjectRealization,
    ExecutionRuntimeRealization,
    StateOverlay,
}

impl IsolationReadOnlyMountAuthority {
    pub fn new_execution_runtime(
        source_path: PathBuf,
        destination: PathBuf,
        source: lillux::InheritedDescriptorAuthority,
    ) -> Self {
        Self {
            source_path,
            destination,
            source,
            scope: IsolationReadOnlyMountScope::ExecutionRuntimeRealization,
        }
    }

    pub fn new(
        source_path: PathBuf,
        destination: PathBuf,
        source: lillux::InheritedDescriptorAuthority,
    ) -> Self {
        Self {
            source_path,
            destination,
            source,
            scope: IsolationReadOnlyMountScope::ProjectRealization,
        }
    }

    pub fn new_state_overlay(
        source_path: PathBuf,
        destination: PathBuf,
        source: lillux::InheritedDescriptorAuthority,
    ) -> Self {
        Self {
            source_path,
            destination,
            source,
            scope: IsolationReadOnlyMountScope::StateOverlay,
        }
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) fn source(&self) -> &lillux::InheritedDescriptorAuthority {
        &self.source
    }

    pub(crate) fn scope(&self) -> IsolationReadOnlyMountScope {
        self.scope
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

impl IsolationCommandAuthority for IsolationRealizationMemberCommand {
    fn authority(&self) -> IsolationCommandAuthorityRef<'_> {
        IsolationCommandAuthorityRef::RealizationMember(self)
    }
}

impl IsolationCommandAuthority for IsolationAdmittedCommand {
    fn authority(&self) -> IsolationCommandAuthorityRef<'_> {
        match self {
            Self::DescriptorBound(command) => command.authority(),
            Self::RealizationMember(command) => command.authority(),
        }
    }
}

/// One daemon-created, connected Unix stream that may be delivered to an
/// isolated target. Callers cannot construct this authority from a raw
/// descriptor, so arbitrary inherited files never acquire target-channel
/// meaning by assertion.
#[derive(Debug, Clone)]
pub struct IsolationTargetChannelAuthority {
    channel: lillux::InheritedDuplexChannelChildAuthority,
    target_fd: u32,
    env_name: String,
}

impl IsolationTargetChannelAuthority {
    pub fn new(
        channel: lillux::InheritedDuplexChannelChildAuthority,
        target_fd: u32,
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
        if matches!(target_fd, 1 | 2) {
            anyhow::bail!("target channel cannot replace stdout or stderr");
        }
        channel.inherited_descriptor().map_err(anyhow::Error::msg)?;
        Ok(Self {
            channel,
            target_fd,
            env_name,
        })
    }

    pub(crate) fn inherited_descriptor(&self) -> anyhow::Result<u32> {
        self.channel
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)
    }

    /// Declared child descriptor slot, used to order typed launch channels.
    /// This exposes no source descriptor or OS operation authority.
    pub fn target_fd(&self) -> u32 {
        self.target_fd
    }

    pub(crate) fn env_name(&self) -> &str {
        &self.env_name
    }

    pub(crate) fn bind_to_subprocess_request(
        &self,
        request: &mut lillux::SubprocessRequest,
    ) -> anyhow::Result<()> {
        self.channel
            .bind_to_subprocess_request(request, &self.env_name, self.target_fd)
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn retain_for_child(
        &self,
        inherited_fds: &mut Vec<lillux::InheritedDescriptorAuthority>,
    ) {
        self.channel.retain_for_child(inherited_fds);
    }
}

/// Per-launch facts used to resolve policy placeholders and record provenance.
#[derive(Debug, Clone, Copy)]
pub struct IsolationLaunchContext<'a> {
    pub project_path: &'a Path,
    pub project_authority: IsolationProjectAuthority,
    /// State-issued proof for the actual immutable execution input, not the
    /// definition/subject generation. Never reconstruct it from a cache path.
    pub immutable_project: Option<&'a ryeos_state::PinnedProjectMaterialization>,
    /// Exact retained view from the admitted workspace owner's bound slot.
    /// Enforced RuntimeWorkspace launches require it. It is never rebuilt
    /// from lower/backend-state paths; nonworkspace and disabled launches
    /// must not carry one. The caller proves workspace/incarnation ownership
    /// before retrieving this descriptor, not by parsing its path.
    pub workspace_view: Option<&'a lillux::InheritedDescriptorAuthority>,
    pub filesystem_authority_ceiling: IsolationFilesystemAuthorityCeiling,
    pub network_authority_ceiling: IsolationNetworkAuthorityCeiling,
    pub live_access: Option<&'a IsolationLiveAccessAuthority>,
    pub state_root: Option<&'a Path>,
    pub checkpoint_dir: Option<&'a Path>,
    pub checkpoint_authority: Option<&'a lillux::PinnedDirectory>,
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
    /// Exact daemon-prepared writable runtime-view directories. These are
    /// descriptor authority, not external content and not node-policy paths.
    pub writable_runtime_view_mounts: &'a [IsolationWritableRuntimeViewMountAuthority],
    pub target_channels: &'a [IsolationTargetChannelAuthority],
    pub item_ref: &'a str,
    pub thread_id: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn writable_runtime_view_derives_a_flat_destination_from_a_directory_descriptor() {
        let source = tempfile::tempdir().unwrap();
        let source = lillux::PinnedDirectory::open(source.path())
            .unwrap()
            .unwrap();
        let authority = IsolationWritableRuntimeViewMountAuthority::new(
            "XDG_CACHE_HOME".to_string(),
            source.inherited_descriptor_authority().unwrap(),
        )
        .unwrap();
        assert_eq!(authority.environment_name(), "XDG_CACHE_HOME");
        assert_eq!(authority.workspace_relative_path(), None);
        assert_eq!(
            authority.destination(),
            Path::new(ryeos_state::objects::SESSION_RUNTIME_VIEWS_ROOT).join("XDG_CACHE_HOME")
        );

        let descendant = IsolationWritableRuntimeViewMountAuthority::new_workspace_descendant(
            "XDG_CACHE_HOME".to_string(),
            ".ai/cache/ryeos-runtime/cache".to_string(),
            source.inherited_descriptor_authority().unwrap(),
        )
        .unwrap();
        assert_eq!(
            descendant.workspace_relative_path(),
            Some(".ai/cache/ryeos-runtime/cache")
        );
        assert!(
            IsolationWritableRuntimeViewMountAuthority::new_workspace_descendant(
                "XDG_CACHE_HOME".to_string(),
                ".ai/cache/../escape".to_string(),
                source.inherited_descriptor_authority().unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("invalid runtime-view workspace path")
        );

        let invalid = IsolationWritableRuntimeViewMountAuthority::new(
            "PATH".to_string(),
            source.inherited_descriptor_authority().unwrap(),
        )
        .unwrap_err();
        assert!(invalid.to_string().contains("protected or invalid name"));

        let file_name = std::ffi::OsStr::new("not-a-directory");
        std::fs::write(source.path().join(file_name), b"file").unwrap();
        let file = source
            .open_inherited_regular(file_name, false)
            .unwrap()
            .unwrap();
        let invalid =
            IsolationWritableRuntimeViewMountAuthority::new("XDG_CACHE_HOME".to_string(), file)
                .unwrap_err();
        assert!(invalid.to_string().contains("not a directory"));
    }

    #[test]
    fn filesystem_ceiling_intersection_cannot_restore_node_mounts() {
        use IsolationFilesystemAuthorityCeiling::{CapturedExecution, NodePolicy};
        for (parent, child, expected) in [
            (NodePolicy, NodePolicy, NodePolicy),
            (NodePolicy, CapturedExecution, CapturedExecution),
            (CapturedExecution, NodePolicy, CapturedExecution),
            (CapturedExecution, CapturedExecution, CapturedExecution),
        ] {
            assert_eq!(parent.intersect(child), expected);
            assert_eq!(
                serde_json::from_value::<IsolationFilesystemAuthorityCeiling>(
                    serde_json::to_value(expected).unwrap()
                )
                .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn isolated_network_ceiling_is_absorbing() {
        assert_eq!(
            IsolationNetworkAuthorityCeiling::NodePolicy
                .intersect(IsolationNetworkAuthorityCeiling::NodePolicy),
            IsolationNetworkAuthorityCeiling::NodePolicy
        );
        assert_eq!(
            IsolationNetworkAuthorityCeiling::NodePolicy
                .intersect(IsolationNetworkAuthorityCeiling::Isolated),
            IsolationNetworkAuthorityCeiling::Isolated
        );
        assert_eq!(
            IsolationNetworkAuthorityCeiling::Isolated
                .intersect(IsolationNetworkAuthorityCeiling::NodePolicy),
            IsolationNetworkAuthorityCeiling::Isolated
        );
    }

    #[test]
    fn target_channel_authority_retains_lillux_minted_channel_and_exact_binding() {
        let (_daemon, worker) = lillux::inherited_duplex_channel_pair().unwrap();
        let authority =
            IsolationTargetChannelAuthority::new(worker, 0, "RYEOS_SESSION_FD").unwrap();
        assert_eq!(authority.target_fd(), 0);
        assert_eq!(authority.env_name(), "RYEOS_SESSION_FD");
        assert!(authority.inherited_descriptor().unwrap() > 2);
    }

    #[test]
    fn target_channel_authority_refuses_noncanonical_binding() {
        let (_daemon, worker) = lillux::inherited_duplex_channel_pair().unwrap();
        assert!(
            IsolationTargetChannelAuthority::new(worker, 4, "lowercase")
                .unwrap_err()
                .to_string()
                .contains("not canonical")
        );

        let (_daemon, worker) = lillux::inherited_duplex_channel_pair().unwrap();
        assert!(
            IsolationTargetChannelAuthority::new(worker, 2, "RYEOS_WORKLOAD_FD")
                .unwrap_err()
                .to_string()
                .contains("stdout or stderr")
        );
    }
}
