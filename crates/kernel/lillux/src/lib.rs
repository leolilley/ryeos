pub mod atomic_fs;
pub mod cas;
pub mod crypto;
pub mod exec;
pub mod host_service;
pub mod identity;
pub mod local_ipc;
pub mod locks;
pub mod process_control;
pub mod sandbox;
pub mod secure_fs;
pub mod signature;
pub mod time;
pub mod vault;

pub use exec::take_inherited_duplex_channel_from_env;
pub use exec::{
    AbortedProcess, AttachmentAbortError, AttachmentReleaseError, CooperativeChildTermination,
    DEFAULT_MAX_CAPTURE_BYTES, DeadlineDuplexStream, ForkSensitiveDescriptorLease,
    InheritedDescriptorAuthority, InheritedDescriptorMapping, InheritedDuplexChannel,
    InheritedDuplexChannelChildAuthority, OutputLimitExceeded, PendingCooperativeChildTermination,
    ProcessAwaitingAttachment, ProcessObservationError, ProcessStdoutReader, RunningProcess,
    SpawnResult, SubprocessLimits, SubprocessRequest, SubprocessResult,
    SupervisedLauncherAttachmentStatusPipe, SupervisedLauncherStatusPipe, SupervisedProcessStatus,
    configure_command_argv0, configure_command_piped_stdio,
    configure_inherited_descriptor_authorities, configure_inherited_fds,
    configure_owner_private_creation_mask, configure_subprocess_limits, disable_process_core_dumps,
    inherited_descriptor_coordinate, inherited_descriptor_path_for, inherited_duplex_channel_pair,
    protect_descriptor_from_exec, replace_current_process, sealed_executable_memfd, sealed_memfd,
    supervised_launcher_attachment_status_pipe, supervised_launcher_status_pipe,
    validate_subprocess_limits,
};
pub use exec::{retain_fork_sensitive_descriptors, retain_fork_sensitive_descriptors_until};

pub use atomic_fs::{
    AtomicMutationError, AtomicMutationResult, atomic_exchange_paths, atomic_write,
    atomic_write_private, atomic_write_with_mode, remove_dir_all_durable, remove_file_durable,
    rename_path_durable, rename_path_noreplace_durable, sync_tree_durable,
};
pub use cas::{
    CanonicalJsonError, CasPutOutcome, CasStore, StreamedBlobOutcome, atomic_write_batch,
    atomic_write_batch_in_pinned_root, canonical_json, sha256_hex, shard_path, valid_hash,
};
pub use host_service::{
    HostServiceController, HostServiceInstallation, HostServiceLaunch, discover_host_service,
    exec_install_transaction, provision_host_service, run_as_administrator,
    validate_install_transaction,
};
pub use local_ipc::{
    LocalDuplexStream, OwnerPrivateLocalDuplexListener, authenticated_unix_peer_from_stream,
};
pub use locks::{
    ExactExclusiveFileLock, ExclusiveFileLock, SharedFileLock, with_exclusive_file_lock,
};
pub use process_control::{
    ControllerAccount, ProcessHostLifetime, ProcessScope, ProcessScopeAllocation,
    ProcessScopeCapability, ProcessScopeConfiguration, ProcessScopeLaunchError,
    ProcessScopeProvider, ProcessScopeRecovery, QuiescedProcessScope, require_administrator,
};
pub use process_control::{
    ExactProcessIdentity, QuiescedProcessGroup, QuiescedProcesses, capture_exact_process_identity,
    diagnostic_process_is_live, diagnostic_process_matches_executable_name,
    prepare_process_group_controller, quiesce_exact_process_group,
};

#[cfg(target_os = "linux")]
pub use exec::{
    DescriptorTransferBounds, InheritedDescriptorTransferChildAuthority,
    InheritedDescriptorTransferReceiver, InheritedDescriptorTransferSender,
    ReceivedDescriptorAuthority, ReceivedDescriptorTransfer, inherited_descriptor_transfer_pair,
    take_inherited_descriptor_transfer_sender,
};
pub use secure_fs::{
    DirectoryTraversalBudget, FilesystemCapacity, NoFollowDirectoryTree, OpenFileIdentity,
    OpenMountEntryKind, OpenRegularFileObservation, PinnedDirectory, PinnedDirectoryEntry,
    PinnedDirectoryEntryMetadata, PinnedDirectoryIdentity, PinnedDirectoryLock, PinnedEntryType,
    PinnedRegularFile, ProcessScopedFlatDirectoryGeneration, canonicalize_existing_path,
    collect_directory_tree_no_follow, collect_pinned_regular_files_no_follow_bounded,
    collect_regular_files_no_follow, current_user_home, digest_open_regular_file_stable_exact,
    ensure_open_regular_file_unchanged, inspect_optional_entry_no_follow,
    matches_regular_file_identity, normalized_portable_regular_mode, observe_open_file_identity,
    observe_open_regular_file, open_mount_entry_kind, open_pinned_regular_file_no_follow,
    pin_canonical_mount_source, protected_system_write_roots, read_open_regular_file_bounded,
    read_open_regular_file_exact_bounded, read_open_regular_file_stable_bounded,
    read_optional_regular_file_bounded_no_follow, read_optional_regular_file_no_follow,
    read_regular_file_bounded_no_follow, read_regular_file_no_follow,
    read_regular_file_to_string_no_follow, require_effective_user_owned_executable,
    require_effective_user_owned_regular, same_open_file_identity, set_open_regular_file_mode,
    visit_regular_files_no_follow, visit_regular_files_no_follow_bounded,
};

pub use sandbox::{
    LinuxOverlayMutation, LinuxOverlayMutationKind, LinuxOverlayTemplate,
    LinuxOverlayWorkspaceObservation, LinuxOverlayWorkspaceOperation, LinuxSandboxAggregateLimits,
    LinuxSandboxExit, LinuxSandboxFixedParentView, LinuxSandboxInspection, LinuxSandboxLifecycle,
    LinuxSandboxMount, LinuxSandboxMountAccess, LinuxSandboxNetwork, LinuxSandboxOverlay,
    LinuxSandboxOverlayDescendantMount, LinuxSandboxProcFilesystem, LinuxSandboxProcess,
    LinuxSandboxRequest, create_linux_overlay_template, exit_with_linux_sandbox_status,
    inspect_linux_sandbox, launch_linux_sandbox, operate_linux_overlay_workspace,
    read_sealed_inherited_descriptor, validate_connected_unix_stream_descriptor,
    validate_current_executable_descriptor, write_inherited_descriptor,
};

pub use identity::envelope::{
    AadFields, Envelope, InspectResult, OpenResult, ValidateResult, inspect_envelope,
    open_envelope, seal_envelope, validate_envelope_env,
};

pub fn run(request: SubprocessRequest) -> SubprocessResult {
    exec::lib_run(request)
}

pub fn spawn(request: SubprocessRequest) -> Result<RunningProcess, SubprocessResult> {
    exec::lib_spawn(request)
}

pub fn spawn_awaiting_attachment(
    request: SubprocessRequest,
) -> Result<ProcessAwaitingAttachment, SubprocessResult> {
    exec::lib_spawn_awaiting_attachment(request)
}

pub fn run_inherited_stdio(request: SubprocessRequest) -> SubprocessResult {
    exec::lib_run_inherited_stdio(request)
}

pub fn spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[(String, String)],
) -> Result<SpawnResult, String> {
    exec::lib_spawn_detached(cmd, args, log, envs)
}

pub fn kill(pid: u32, grace: f64) -> Result<String, String> {
    exec::lib_kill(pid, grace)
}

pub fn is_alive(pid: u32) -> bool {
    exec::lib_is_alive(pid)
}
