//! Linux backend for protected guest-side candidate ownership.
//!
//! This component runs in a dedicated trusted launcher process: native Lillux
//! setup changes its caller's namespaces. A transport adapter must not call it
//! on a node's async executor. No credentials or channel keys are mounted into
//! the endpoint. The caller supplies already-admitted runtime descriptors and
//! a protected parent directory. This component creates fresh candidate inodes
//! from CAS; it never makes a shared materialization writable.
//! Linux request translation is confined here. Lillux implements all native
//! operations and refuses unsupported host capabilities; this backend does
//! not promise equivalent containment on another platform.

use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use lillux::time::Duration;
use ryeos_external_execution_contract::{
    ExternalExecutionMode, ExternalGuestInputProjection, ExternalTargetExit, GuestMountAccess,
    GuestMountKind,
};
use ryeos_state::external_execution::{
    AuthenticatedExecutionFrame, ChannelDirection, ExecutionChannelBinding,
    ExecutionChannelPayload, MAX_CANDIDATE_CONTENT_BYTES, NativeNamespaceExit,
    NativeWriterExclusionMechanism, NativeWriterExclusionObservation,
};
use ryeos_state::project_materialization::VerifiedProjectSnapshotClosure;
use ryeos_state::{
    CasMutationGuard, DurableCasPublicationKey, DurableCasUploadStage,
    PinnedProjectMaterialization, PinnedStateAuthority,
};

use crate::launcher::ExternalCandidateLauncherSpec;

/// Translate the admitted launcher contract to the explicitly selected native
/// backend. Descriptor owners remain with the caller through preparation.
pub(crate) fn prepare_candidate(
    spec: &ExternalCandidateLauncherSpec,
    authority: &PinnedStateAuthority,
    private_parent: &lillux::PinnedDirectory,
    mount_authorities: &[lillux::InheritedDescriptorAuthority],
) -> Result<(NativeExternalCandidate, NativeCandidateOutput)> {
    use ryeos_state::external_execution::admission::ExternalCandidateProcFilesystem;

    spec.validate_native_target()?;
    ensure!(
        mount_authorities.len() == spec.runtime_mounts.len(),
        "native candidate mount authority count changed"
    );
    let mounts = spec
        .runtime_mounts
        .iter()
        .zip(mount_authorities)
        .map(|(mount, authority)| {
            ensure!(
                authority.mount_entry_kind()? != lillux::OpenMountEntryKind::UnixSocket,
                "external runtime mount is not file content"
            );
            Ok(lillux::LinuxSandboxMount {
                source_fd: authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?,
                destination: PathBuf::from(&mount.destination),
                access: match mount.access {
                    GuestMountAccess::ReadOnly => lillux::LinuxSandboxMountAccess::ReadOnly,
                    GuestMountAccess::PrivateWritable => lillux::LinuxSandboxMountAccess::Writable,
                },
                layer: mount.layer,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let request = lillux::LinuxSandboxRequest {
        executable: PathBuf::from(&spec.executable),
        argv0: spec.argv0.clone().into(),
        arguments: spec.arguments.iter().cloned().map(Into::into).collect(),
        cwd: PathBuf::from(&spec.cwd),
        environment: spec
            .environment
            .iter()
            .map(|(name, value)| (name.clone().into(), value.clone().into()))
            .collect(),
        mounts,
        fixed_parent_views: vec![],
        overlay: None,
        network: lillux::LinuxSandboxNetwork::Isolated,
        private_tmp: true,
        proc_filesystem: match spec.proc_filesystem {
            ExternalCandidateProcFilesystem::Empty => lillux::LinuxSandboxProcFilesystem::Empty,
            ExternalCandidateProcFilesystem::PidNamespace => {
                lillux::LinuxSandboxProcFilesystem::PidNamespace
            }
            ExternalCandidateProcFilesystem::PidNamespaceNested => {
                lillux::LinuxSandboxProcFilesystem::PidNamespaceNested
            }
        },
        minimal_devices: true,
        character_devices: vec![],
        target_channels: vec![],
        lifecycle: lillux::LinuxSandboxLifecycle::Run,
        contain_process_group: spec.contain_process_group,
        nested_sandbox: spec.nested_sandbox,
        aggregate_limits: None,
    };
    NativeExternalCandidate::prepare(
        spec.binding.clone(),
        authority,
        private_parent,
        request,
        &spec.guest_inputs,
        spec.stdin.as_ref(),
    )
}

/// Only admitted Project inputs can acquire targets inside the private root.
/// Runtime/configuration mounts outside /workspace never become exclusions.
fn workspace_input_targets(
    inputs: &ExternalGuestInputProjection,
) -> Result<Vec<(String, GuestMountKind)>> {
    let mut targets = Vec::new();
    for input in &inputs.inputs {
        if let Some(relative) = input.destination.strip_prefix("/workspace/") {
            ryeos_state::objects::validate_canonical_project_relative_path(relative)?;
            ensure!(
                input.access == GuestMountAccess::ReadOnly,
                "workspace input shadow must be read-only"
            );
            targets.push((relative.to_owned(), input.kind));
        }
    }
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    ryeos_project_capture::validate_operational_exclusions(
        &targets
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>(),
    )?;
    Ok(targets)
}

struct WorkspaceInputAnchor {
    parent: lillux::PinnedDirectory,
    entry: lillux::PinnedDirectoryEntryMetadata,
    // Keep the inode alive so a deleted entry cannot pass through inode reuse.
    // This descriptor is never passed to the candidate.
    _lifeline: lillux::InheritedDescriptorAuthority,
}

fn retain_workspace_input_anchor(
    anchors: &mut std::collections::BTreeMap<PathBuf, WorkspaceInputAnchor>,
    relative: PathBuf,
    parent: &lillux::PinnedDirectory,
    lifeline: lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    if let std::collections::btree_map::Entry::Vacant(slot) = anchors.entry(relative.clone()) {
        let name = relative
            .file_name()
            .context("workspace anchor has no name")?;
        let entry = parent
            .entry_no_follow(name)?
            .context("workspace anchor disappeared")?;
        slot.insert(WorkspaceInputAnchor {
            parent: parent.try_clone()?,
            entry,
            _lifeline: lifeline,
        });
    }
    Ok(())
}

fn verify_workspace_input_anchors(anchors: &[WorkspaceInputAnchor]) -> Result<()> {
    // Ancestors are retained in root-first path order. Every comparison is
    // descriptor-relative: the original root pathname may be masked after
    // native namespace preparation. No directory/file metadata is interpreted
    // above Lillux's observation API.
    for anchor in anchors {
        anchor
            .parent
            .ensure_entry_observation(&anchor.entry)
            .context("workspace input target or ancestor moved or changed")?;
    }
    Ok(())
}

fn prepare_workspace_input_targets(
    root: &lillux::PinnedDirectory,
    targets: &[(String, GuestMountKind)],
    deadline: lillux::time::MonotonicDeadline,
) -> Result<Vec<WorkspaceInputAnchor>> {
    let mut anchors = std::collections::BTreeMap::new();
    for (relative, kind) in targets {
        ensure!(
            !deadline.has_elapsed(),
            "workspace input preparation expired"
        );
        ryeos_state::objects::validate_canonical_project_relative_path(relative)?;
        let path = std::path::Path::new(relative);
        let mut parent = root.try_clone()?;
        let mut prefix = PathBuf::new();
        if let Some(ancestors) = path.parent() {
            for component in ancestors.components() {
                let child = parent.open_or_create_child(component.as_os_str(), 0o755)?;
                prefix.push(component.as_os_str());
                retain_workspace_input_anchor(
                    &mut anchors,
                    prefix.clone(),
                    &parent,
                    child.inherited_descriptor_authority()?,
                )?;
                parent = child;
            }
        }
        let name = path
            .file_name()
            .context("workspace input has no target name")?;
        let lifeline = match kind {
            GuestMountKind::Directory => parent
                .open_or_create_child(name, 0o755)?
                .inherited_descriptor_authority()?,
            GuestMountKind::RegularFile => {
                // Reuse a real existing file without truncation. Wrong kinds
                // and symlinks refuse; absent targets are exclusively created.
                let file = match parent.open_pinned_regular(name, false)? {
                    Some(file) => file,
                    None => parent.open_pinned_regular_create(name, false, true, 0o600)?,
                };
                file.inherited_descriptor_authority()?
            }
        };
        retain_workspace_input_anchor(&mut anchors, path.to_owned(), &parent, lifeline)?;
    }
    Ok(anchors.into_values().collect())
}

fn validate_workspace_output_separation(
    output_paths: impl IntoIterator<Item = impl AsRef<str>>,
    input_shadows: &[String],
) -> Result<()> {
    for output in output_paths {
        let output = std::path::Path::new(output.as_ref());
        ensure!(
            !input_shadows.iter().any(|input| {
                let input = std::path::Path::new(input);
                output.starts_with(input) || input.starts_with(output)
            }),
            "external workspace output overlaps an admitted input shadow"
        );
    }
    Ok(())
}

fn load_workspace_output_authority(
    inputs: &ExternalGuestInputProjection,
    input_shadows: &[String],
) -> Result<Option<ryeos_state::objects::WorkspaceOutputAuthority>> {
    inputs
        .workspace_outputs
        .as_ref()
        .map(|input| {
            let bytes = lillux::read_sealed_inherited_descriptor(
                input.descriptor,
                ryeos_state::objects::MAX_WORKSPACE_OUTPUT_PARTITION_BYTES,
            )
            .map_err(anyhow::Error::msg)?;
            ensure!(
                bytes.len() as u64 == input.bytes
                    && lillux::sha256_hex(&bytes) == input.authority_hash,
                "external workspace-output authority changed after admission"
            );
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(
                lillux::canonical_json(&value)?.as_bytes() == bytes,
                "external workspace-output authority is noncanonical"
            );
            let authority: ryeos_state::objects::WorkspaceOutputAuthority =
                serde_json::from_value(value)?;
            authority.validate()?;
            ensure!(
                authority.partition.roots.iter().all(|root| root.storage
                    == ryeos_state::external_content::products::ProductStorage::Content),
                "external workspace outputs require the transferable content storage tier"
            );
            validate_workspace_output_separation(
                authority
                    .partition
                    .roots
                    .iter()
                    .map(|root| root.path.as_str()),
                input_shadows,
            )?;
            Ok(authority)
        })
        .transpose()
}

pub struct NativeExternalCandidate {
    binding: ExecutionChannelBinding,
    guest_inputs: ExternalGuestInputProjection,
    process: lillux::HeldLinuxSandboxProcess,
    /// Pinned from the child-origin held-mount receipt before release. Lillux
    /// clears the live process handle PID after reap, but a cached applied
    /// receipt must still compare to this exact original child.
    held_child_pid: u32,
    expected_applied_launch: lillux::LinuxSandboxAppliedLaunchCommitments,
    expected_mount_preparation: lillux::LinuxSandboxMountPreparationCommitments,
    root: lillux::PinnedDirectory,
    workspace_input_shadows: Vec<String>,
    workspace_input_anchors: Vec<WorkspaceInputAnchor>,
    workspace_output_authority: Option<ryeos_state::objects::WorkspaceOutputAuthority>,
    deadline: lillux::time::MonotonicDeadline,
    channel_deadline: lillux::time::MonotonicDeadline,
    released: bool,
    terminal: bool,
    input: Option<std::fs::File>,
    protocol: CandidateProtocolInput,
    direct_input: Option<DirectCommandInput>,
    /// Lillux's once-reaped observation remains local to the native owner. A
    /// cleanup proof alone is never promoted into an actual target exit.
    native_termination: Option<lillux::LinuxSandboxTermination>,
    target_exit: Option<ExternalTargetExit>,
    /// The native side effect crossed its boundary but the protected journal
    /// has not yet acknowledged its durable `applied` transition. No later
    /// action may start while this fence is present.
    application_fence: ApplicationFence,
}

/// Output streams are untrusted bytes; no writable input descriptor escapes
/// the serialized candidate owner. The supervisor applies independent output
/// budgets and cannot derive completion from either stream.
pub struct NativeCandidateOutput {
    pub stdout: std::fs::File,
    pub stderr: std::fs::File,
}

fn require_applied_candidate_runtime(
    receipt: &lillux::LinuxSandboxAppliedLaunchReceipt,
    expected_target: &lillux::LinuxSandboxAppliedLaunchCommitments,
    expected_mounts: &lillux::LinuxSandboxMountPreparationCommitments,
    held_pid: u32,
) -> Result<()> {
    let target_matches = receipt.matches_commitments(expected_target);
    let mounts_match = receipt.matches_post_release_mounts(expected_mounts);
    let child_matches = receipt.owned_child_pid == held_pid;
    ensure!(
        target_matches && mounts_match && child_matches,
        "external candidate applied runtime differs from admitted held target: target_matches={target_matches}, mounts_match={mounts_match}, child_matches={child_matches}, namespace_pid_one={}, uid_one={}, gid_one={}, no_new_privs={}, seccomp_filter={}, executable_matches={}, argv_matches={}, environment_matches={}, cwd_matches={}",
        receipt.namespace_pid == 1,
        receipt.effective_uid == 1,
        receipt.effective_gid == 1,
        receipt.no_new_privs,
        receipt.seccomp_mode == 2,
        receipt.executable_sha256 == expected_target.executable_sha256,
        receipt.argv_sha256 == expected_target.argv_sha256,
        receipt.environment_sha256 == expected_target.environment_sha256,
        receipt.cwd_sha256 == expected_target.cwd_sha256,
    );
    Ok(())
}

/// The admitted command mode determines the execution view. A deterministic
/// evaluator consumes the frozen generation as data; it cannot turn that view
/// into a writable candidate or publish workspace-output capture authority.
/// Scratch remains an independently admitted mount outside this generation.
fn execution_workspace_access(
    mode: ExternalExecutionMode,
    has_workspace_outputs: bool,
) -> Result<lillux::LinuxSandboxMountAccess> {
    mode.validate()?;
    match mode {
        ExternalExecutionMode::StructuredSession {} => {
            Ok(lillux::LinuxSandboxMountAccess::Writable)
        }
        ExternalExecutionMode::DirectCommand { .. } => {
            ensure!(
                !has_workspace_outputs,
                "external direct command cannot capture or mutate its frozen execution generation"
            );
            Ok(lillux::LinuxSandboxMountAccess::ReadOnly)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CandidateProtocolProgress {
    pub frame_digest: String,
    pub written_bytes: usize,
    /// All bytes reached the pipe, not endpoint processing or command success.
    pub complete: bool,
}

#[derive(Default)]
struct CandidateProtocolInput {
    frontier: u64,
    revoked: bool,
    pending: Option<(String, Vec<u8>, usize)>,
}

/// Progress of the one sealed stdin buffer. Completion means all admitted
/// bytes reached the pipe and its writer closed, not workload success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectCommandInputProgress {
    Fenced,
    Pending {
        written_bytes: usize,
        total_bytes: usize,
    },
    Complete {
        written_bytes: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectInputPhase {
    AwaitingRelease,
    AwaitingReleaseFinish,
    Delivering,
    Complete,
    Faulted,
}

/// In-memory execution state only. It cannot be deserialized/reconstructed to
/// resume uncertain writes in a replacement launcher. The original release
/// claim and its exact durable finish acknowledgement authorize this buffer.
struct DirectCommandInput {
    bytes: Vec<u8>,
    offset: usize,
    release_digest: Option<String>,
    phase: DirectInputPhase,
}

impl DirectCommandInput {
    fn from_sealed(
        input: &ryeos_state::external_execution::admission::ExternalDirectSealedInput,
    ) -> Result<Self> {
        Ok(Self {
            bytes: input.decoded_bytes()?,
            offset: 0,
            release_digest: None,
            phase: DirectInputPhase::AwaitingRelease,
        })
    }

    fn begin_release(&mut self, digest: &str) -> Result<()> {
        ensure!(
            self.phase == DirectInputPhase::AwaitingRelease,
            "external direct stdin release was already consumed"
        );
        self.release_digest = Some(digest.to_owned());
        self.phase = DirectInputPhase::AwaitingReleaseFinish;
        Ok(())
    }

    fn acknowledge_release_finish(&mut self, digest: &str) {
        if self.phase == DirectInputPhase::AwaitingReleaseFinish
            && self.release_digest.as_deref() == Some(digest)
        {
            self.phase = if self.bytes.is_empty() {
                DirectInputPhase::Complete
            } else {
                DirectInputPhase::Delivering
            };
        }
    }

    fn flush(&mut self, writer: &mut impl std::io::Write) -> Result<DirectCommandInputProgress> {
        match self.phase {
            DirectInputPhase::AwaitingRelease | DirectInputPhase::AwaitingReleaseFinish => {
                return Ok(DirectCommandInputProgress::Fenced);
            }
            DirectInputPhase::Complete => {
                return Ok(DirectCommandInputProgress::Complete {
                    written_bytes: self.offset,
                });
            }
            DirectInputPhase::Faulted => {
                anyhow::bail!("external direct stdin is revoked or failed")
            }
            DirectInputPhase::Delivering => {}
        }
        let remaining = &self.bytes[self.offset..];
        let result = match writer.write(remaining) {
            Ok(count) if count > 0 && count <= remaining.len() => {
                self.offset += count;
                Ok(())
            }
            Ok(_) => Err(anyhow::anyhow!(
                "external direct stdin pipe failed before complete application"
            )),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(anyhow::Error::new(error).context("write external direct stdin")),
        };
        if let Err(error) = result {
            self.revoke();
            return Err(error);
        }
        if self.offset == self.bytes.len() {
            self.phase = DirectInputPhase::Complete;
            self.bytes.clear();
            Ok(DirectCommandInputProgress::Complete {
                written_bytes: self.offset,
            })
        } else {
            Ok(DirectCommandInputProgress::Pending {
                written_bytes: self.offset,
                total_bytes: self.bytes.len(),
            })
        }
    }

    fn is_complete(&self) -> bool {
        self.phase == DirectInputPhase::Complete
    }

    /// Own only the finite delivery state over a Lillux-supplied nonblocking
    /// stream. Dropping the writer closes EOF; no flush/wait is requested.
    fn pump(
        &mut self,
        writer: &mut Option<impl std::io::Write>,
    ) -> Result<DirectCommandInputProgress> {
        if self.is_complete() {
            *writer = None;
            return Ok(DirectCommandInputProgress::Complete {
                written_bytes: self.offset,
            });
        }
        if matches!(
            self.phase,
            DirectInputPhase::AwaitingRelease | DirectInputPhase::AwaitingReleaseFinish
        ) {
            return Ok(DirectCommandInputProgress::Fenced);
        }
        let result = match writer.as_mut() {
            Some(writer) => self.flush(writer),
            None => Err(anyhow::anyhow!(
                "external direct stdin writer closed before completion"
            )),
        };
        match &result {
            Ok(DirectCommandInputProgress::Complete { .. }) => *writer = None,
            Err(_) => {
                self.revoke();
                *writer = None;
            }
            _ => {}
        }
        result
    }

    fn revoke(&mut self) {
        // Completed delivery is an immutable byte-count fact, not permission
        // for overall success after cancellation, output failure or timeout.
        if !self.is_complete() {
            self.phase = DirectInputPhase::Faulted;
        }
        self.bytes.clear();
    }
}

#[derive(Default)]
struct ApplicationFence {
    awaiting_finish: Option<String>,
    last_acknowledged: Option<String>,
}

impl ApplicationFence {
    fn begin(&mut self, digest: &str) -> Result<()> {
        ensure!(
            self.awaiting_finish.is_none(),
            "candidate already awaits durable finish"
        );
        self.awaiting_finish = Some(digest.to_owned());
        Ok(())
    }

    fn acknowledge(&mut self, digest: &str) -> Result<()> {
        if self.awaiting_finish.is_none() && self.last_acknowledged.as_deref() == Some(digest) {
            return Ok(());
        }
        ensure!(
            self.awaiting_finish.as_deref() == Some(digest),
            "candidate finish acknowledgement changed its exact application"
        );
        self.awaiting_finish = None;
        self.last_acknowledged = Some(digest.to_owned());
        Ok(())
    }

    /// Terminal cancellation may overtake an uncertain input claim. The
    /// interrupted input stays durably claimed and is never retried; the live
    /// launcher now waits only for the cancellation finish acknowledgement.
    fn begin_terminal(&mut self, digest: &str) {
        self.awaiting_finish = Some(digest.to_owned());
    }

    fn is_clear(&self) -> bool {
        self.awaiting_finish.is_none()
    }
}

impl CandidateProtocolInput {
    fn begin(&mut self, sequence: u64, digest: &str, bytes: Vec<u8>) -> Result<()> {
        ensure!(
            !self.revoked && self.pending.is_none() && sequence > self.frontier,
            "candidate protocol is revoked, busy or already started"
        );
        ensure!(
            !bytes.is_empty() && bytes.len() <= ryeos_state::external_execution::MAX_CHUNK_BYTES,
            "candidate protocol chunk exceeds its bound"
        );
        self.frontier = sequence;
        self.pending = Some((digest.to_owned(), bytes, 0));
        Ok(())
    }

    fn flush(&mut self, writer: &mut impl std::io::Write) -> Result<CandidateProtocolProgress> {
        ensure!(!self.revoked, "candidate protocol input is revoked");
        let (digest, bytes, offset) = self
            .pending
            .as_mut()
            .context("candidate has no pending protocol input")?;
        match writer.write(&bytes[*offset..]) {
            Ok(0) => anyhow::bail!("candidate protocol pipe closed before complete application"),
            Ok(count) => *offset += count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
        let progress = CandidateProtocolProgress {
            frame_digest: digest.clone(),
            written_bytes: *offset,
            complete: *offset == bytes.len(),
        };
        if progress.complete {
            self.pending = None;
        }
        Ok(progress)
    }

    fn revoke(&mut self) {
        self.revoked = true;
        self.pending = None;
    }
}

/// This value can only be returned after exact native namespace termination
/// and complete project capture. It is local testimony, not cloud cleanup.
pub struct NativeCandidateExport {
    binding_digest: String,
    completion_request_digest: String,
    occurrence_digest: String,
    durable_stage_id: String,
    snapshot_hash: String,
    output_capture_hash: Option<String>,
    writer_exclusion_blob_hash: String,
}

impl NativeCandidateExport {
    pub fn snapshot_hash(&self) -> &str {
        &self.snapshot_hash
    }
    pub fn writer_exclusion_blob_hash(&self) -> &str {
        &self.writer_exclusion_blob_hash
    }
    pub fn output_capture_hash(&self) -> Option<&str> {
        self.output_capture_hash.as_deref()
    }
    pub fn binding_digest(&self) -> &str {
        &self.binding_digest
    }
    pub fn completion_request_digest(&self) -> &str {
        &self.completion_request_digest
    }
    pub fn occurrence_digest(&self) -> &str {
        &self.occurrence_digest
    }
    pub fn durable_stage_id(&self) -> &str {
        &self.durable_stage_id
    }
}

impl NativeExternalCandidate {
    fn prepare(
        binding: ExecutionChannelBinding,
        authority: &PinnedStateAuthority,
        private_parent: &lillux::PinnedDirectory,
        mut request: lillux::LinuxSandboxRequest,
        guest_inputs: &ExternalGuestInputProjection,
        stdin: Option<&ryeos_state::external_execution::admission::ExternalDirectSealedInput>,
    ) -> Result<(Self, NativeCandidateOutput)> {
        binding.validate()?;
        guest_inputs.validate()?;
        let workspace_targets = workspace_input_targets(guest_inputs)?;
        let workspace_input_shadows = workspace_targets
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let workspace_output_authority =
            load_workspace_output_authority(guest_inputs, &workspace_input_shadows)?;
        let direct_input = match (binding.execution_mode, stdin) {
            (ExternalExecutionMode::StructuredSession {}, None) => None,
            (ExternalExecutionMode::DirectCommand { .. }, Some(input)) => {
                Some(DirectCommandInput::from_sealed(input)?)
            }
            _ => anyhow::bail!("native candidate input contradicts its admitted execution mode"),
        };
        let workspace_access = execution_workspace_access(
            binding.execution_mode,
            guest_inputs.workspace_outputs.is_some(),
        )?;
        ensure!(
            guest_inputs.base_snapshot.snapshot_hash == binding.base_snapshot_hash
                && request.mounts.len() == guest_inputs.inputs.len(),
            "external candidate guest inputs changed after admission"
        );
        ensure!(
            request.network == lillux::LinuxSandboxNetwork::Isolated
                && request.private_tmp
                && request.minimal_devices
                && request.character_devices.is_empty()
                && request.target_channels.is_empty()
                && request.overlay.is_none()
                && request.fixed_parent_views.is_empty()
                && request.aggregate_limits.is_none()
                && request.lifecycle == lillux::LinuxSandboxLifecycle::Run,
            "external candidate requires the exact native candidate-only boundary"
        );
        let workspace = PathBuf::from("/workspace");
        ensure!(
            request.cwd.starts_with(&workspace),
            "external candidate cwd is outside its workspace"
        );
        for (mount, input) in request.mounts.iter().zip(&guest_inputs.inputs) {
            let expected_access = match input.access {
                GuestMountAccess::ReadOnly => lillux::LinuxSandboxMountAccess::ReadOnly,
                GuestMountAccess::PrivateWritable => lillux::LinuxSandboxMountAccess::Writable,
            };
            let destination = PathBuf::from(&input.destination);
            ensure!(
                mount.destination == destination
                    && mount.access == expected_access
                    && mount.layer == u32::from(mount.destination.starts_with(&workspace))
                    && mount.destination != workspace
                    && !workspace.starts_with(&mount.destination)
                    && (!mount.destination.starts_with(&workspace)
                        || mount.access == lillux::LinuxSandboxMountAccess::ReadOnly),
                "external candidate mount changed its admitted guest authority"
            );
        }
        let now = lillux::time::timestamp_millis();
        ensure!(
            now >= binding.issued_at_ms && now < binding.execution_deadline_ms,
            "external candidate preparation is outside its execution window"
        );
        let deadline = lillux::time::MonotonicDeadline::after(Duration::from_millis(
            u64::try_from(binding.execution_deadline_ms - now)?,
        ));
        let channel_deadline = lillux::time::MonotonicDeadline::after(Duration::from_millis(
            u64::try_from(binding.expires_at_ms - now)?,
        ));
        // Hold the CAS guard only during immutable input validation/copy. It
        // must be gone before native launch forks the isolated target.
        let base = {
            let guard = authority.acquire_shared_guard()?;
            let cas = authority.cas_store()?;
            let closure = VerifiedProjectSnapshotClosure::load(&cas, &binding.base_snapshot_hash)?;
            let mut total = 0_u64;
            for file in closure.tree().files().values() {
                total = total
                    .checked_add(file.size)
                    .context("external base byte count overflow")?;
                ensure!(
                    total <= MAX_CANDIDATE_CONTENT_BYTES,
                    "external base exceeds first-generation private-copy bound"
                );
            }
            let (_, root) = private_parent.create_unique_child("external-candidate-", 0o700)?;
            for (path, file) in closure.tree().files() {
                ensure!(
                    !deadline.has_elapsed(),
                    "external base copy deadline expired"
                );
                let relative = std::path::Path::new(path);
                let mut parent = root.try_clone()?;
                if let Some(ancestors) = relative.parent() {
                    for component in ancestors.components() {
                        parent = parent.open_or_create_child(component.as_os_str(), 0o755)?;
                    }
                }
                let size = cas.materialize_blob_to_new_regular(
                    &file.blob_hash,
                    &parent,
                    relative
                        .file_name()
                        .context("external base file has no name")?,
                    file.normalized_mode,
                )?;
                ensure!(size == file.size, "external base size changed during copy");
            }
            PinnedProjectMaterialization::verify_pinned_from_closure(
                authority, &guard, &closure, root,
            )?
        };
        ensure!(
            !deadline.has_elapsed(),
            "external candidate preparation exhausted its execution window"
        );
        // The exact private base has been verified. From here this inode is an
        // execution assembly: admitted input mounts may need empty targets.
        // Do not export a verified-base descriptor after augmenting it, or
        // mutate CAS/shared materializations to satisfy namespace setup.
        let root = base.try_clone_root()?;
        drop(base);
        let workspace_input_anchors =
            prepare_workspace_input_targets(&root, &workspace_targets, deadline)?;
        let candidate_mount = root.inherited_descriptor_authority()?;
        request.mounts.push(lillux::LinuxSandboxMount {
            source_fd: candidate_mount
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            destination: workspace,
            access: workspace_access,
            layer: 0,
        });
        let expected_applied_launch = lillux::LinuxSandboxAppliedLaunchCommitments::from_target(
            lillux::LinuxSandboxAppliedLaunchTarget {
                executable: &request.executable,
                argv0: &request.argv0,
                arguments: &request.arguments,
                cwd: &request.cwd,
                environment: &request.environment,
            },
        )
        .map_err(anyhow::Error::msg)?;
        let expected_mount_preparation =
            lillux::LinuxSandboxMountPreparationCommitments::from_admitted_mounts(
                &request.mounts,
                &[],
            )
            .map_err(anyhow::Error::msg)?;
        let (process, pipes) =
            lillux::prepare_linux_sandbox_piped(request).map_err(anyhow::Error::msg)?;
        let held_mount_preparation = process
            .mount_preparation_receipt()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            held_mount_preparation.matches_commitments(&expected_mount_preparation)
                && held_mount_preparation.owned_child_pid == process.child_pid(),
            "external candidate held mounts differ from admitted inputs"
        );
        Ok((
            Self {
                binding,
                guest_inputs: guest_inputs.clone(),
                process,
                held_child_pid: held_mount_preparation.owned_child_pid,
                expected_applied_launch,
                expected_mount_preparation,
                root,
                workspace_input_shadows,
                workspace_input_anchors,
                workspace_output_authority,
                deadline,
                channel_deadline,
                released: false,
                terminal: false,
                input: Some(pipes.stdin),
                protocol: CandidateProtocolInput::default(),
                direct_input,
                native_termination: None,
                target_exit: None,
                application_fence: ApplicationFence::default(),
            },
            NativeCandidateOutput {
                stdout: pipes.stdout,
                stderr: pipes.stderr,
            },
        ))
    }

    pub fn release(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()> {
        self.require_owner(frame)?;
        ensure!(
            matches!(frame.frame().payload, ExecutionChannelPayload::Release),
            "not an external release command"
        );
        ensure!(
            !self.terminal
                && !self.released
                && self.application_fence.is_clear()
                && !self.deadline.has_elapsed(),
            "external candidate is terminal, released, awaiting finish or expired"
        );
        // A release error may have crossed the boundary. Never retry it.
        self.released = true;
        self.protocol.frontier = frame.frame().sequence;
        self.application_fence.begin(frame.digest())?;
        if let Some(input) = &mut self.direct_input {
            input.begin_release(frame.digest())?;
        }
        if let Err(error) = self.process.release_once() {
            self.close_input();
            return Err(anyhow::Error::msg(error));
        }
        Ok(())
    }

    /// Point-read the child-owned pre-exec receipt for the sole released
    /// target. Pending is not success; a mismatch refuses without turning a
    /// launcher assertion into qualification testimony. The supervisor must
    /// durably join this fact to its exact occurrence before any claim uses it.
    pub fn try_observe_applied_launch(
        &mut self,
    ) -> Result<Option<lillux::LinuxSandboxAppliedLaunchReceipt>> {
        ensure!(self.released, "external candidate has not been released");
        let Some(receipt) = self
            .process
            .try_observe_applied_launch()
            .map_err(anyhow::Error::msg)?
        else {
            return Ok(None);
        };
        require_applied_candidate_runtime(
            &receipt,
            &self.expected_applied_launch,
            &self.expected_mount_preparation,
            self.held_child_pid,
        )?;
        Ok(Some(receipt))
    }

    /// Called only after the supervisor's exact durable application claim and
    /// sticky-revocation check. This object cannot be reconstructed to replay
    /// an uncertain command. Its mutable owner serializes writes with stop.
    pub fn begin_protocol_input(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()> {
        self.require_owner(frame)?;
        ensure!(
            self.binding.execution_mode == ExternalExecutionMode::StructuredSession {},
            "external direct command has no structured-session input authority"
        );
        ensure!(
            self.released
                && !self.terminal
                && self.application_fence.is_clear()
                && !self.execution_expired(),
            "candidate is not executable or awaits durable finish"
        );
        self.protocol.begin(
            frame.frame().sequence,
            frame.digest(),
            frame.protocol_bytes()?,
        )
    }

    /// At most one nonblocking write. Partial progress remains the same claimed
    /// frame; another begin, retry from offset zero or input after stop fails.
    pub fn flush_protocol_input(&mut self) -> Result<CandidateProtocolProgress> {
        ensure!(
            self.released && !self.terminal && !self.execution_expired(),
            "candidate is not executable"
        );
        let result = self
            .protocol
            .flush(self.input.as_mut().context("candidate input is closed")?);
        if let Ok(progress) = &result
            && progress.complete
        {
            self.application_fence.begin(&progress.frame_digest)?;
        }
        if result.is_err() {
            self.close_input();
        }
        result
    }

    /// Apply one journal-gated prefix of an authenticated protocol frame. The
    /// first call binds the native pending buffer; later calls must present the
    /// same frame and continue from its retained offset.
    pub fn apply_protocol_chunk(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<usize> {
        self.require_owner(frame)?;
        if self.protocol.pending.is_none() {
            self.begin_protocol_input(frame)?;
        } else {
            let (digest, _, _) = self
                .protocol
                .pending
                .as_ref()
                .context("candidate protocol pending state disappeared")?;
            ensure!(
                digest == frame.digest(),
                "candidate protocol chunk changed its claimed frame"
            );
        }
        let before = self
            .protocol
            .pending
            .as_ref()
            .map(|(_, _, offset)| *offset)
            .context("candidate protocol input was not established")?;
        let progress = self.flush_protocol_input()?;
        ensure!(
            progress.written_bytes > before,
            "candidate protocol writer made no bounded progress"
        );
        Ok(progress.written_bytes - before)
    }

    /// Release the native action fence only after the protected supervisor has
    /// committed `finish_application` for this exact frame. A lost response or
    /// acknowledgement leaves the launcher fenced rather than permitting a
    /// second action after an uncertain effect.
    pub fn acknowledge_application_finish(&mut self, frame_digest: &str) -> Result<()> {
        self.application_fence.acknowledge(frame_digest)?;
        if let Some(input) = &mut self.direct_input {
            input.acknowledge_release_finish(frame_digest);
            // Empty sealed input closes only after the exact release finish.
            // This is stdin EOF, not command completion or candidate terminal.
            if input.is_complete() {
                self.input = None;
            }
        }
        Ok(())
    }

    /// At most one nonblocking write of the admitted one-shot input. The finite
    /// launcher loop must interleave this with output, cancellation and exact
    /// target-status observation; no host writer thread or blocking flush is
    /// created here. A lost release acknowledgement leaves input fenced.
    pub fn pump_direct_input(&mut self) -> Result<DirectCommandInputProgress> {
        ensure!(
            self.direct_input.is_some(),
            "structured session has no one-shot stdin"
        );
        if self.terminal || self.execution_expired() {
            self.close_input();
            anyhow::bail!("external direct stdin is terminal or expired");
        }
        if !self.released || !self.application_fence.is_clear() {
            return Ok(DirectCommandInputProgress::Fenced);
        }
        let input = self
            .direct_input
            .as_mut()
            .expect("direct input checked above");
        let result = input.pump(&mut self.input);
        if result.is_err() {
            self.close_input();
        }
        result
    }

    /// This is required in addition to actual exit/output completion. A target
    /// exiting zero before complete stdin delivery is not success. Delivery to
    /// the pipe is not testimony that the target processed those bytes.
    pub fn require_direct_input_complete(&self) -> Result<()> {
        ensure!(
            self.direct_input
                .as_ref()
                .is_some_and(DirectCommandInput::is_complete),
            "external direct stdin was not completely applied"
        );
        Ok(())
    }

    /// Observe and retain only Lillux's actual released-target status. EOF and
    /// cleanup-init status never substitute for this observation. Once reaped,
    /// later callers receive the same cached fact, not a second native wait.
    pub fn try_observe_target_exit(&mut self) -> Result<Option<ExternalTargetExit>> {
        ensure!(
            self.direct_input.is_some(),
            "structured session has no direct target-exit authority"
        );
        if let Some(exit) = self.target_exit {
            return Ok(Some(exit));
        }
        if !self.released || !self.application_fence.is_clear() {
            return Ok(None);
        }
        ensure!(
            self.native_termination.is_none(),
            "actual target status is unavailable after native cleanup or launch failure"
        );
        // A fast direct command may exit before the supervisor polls the
        // applied-launch channel. Reaping it first destroys the exact owned
        // child identity required by Lillux to authenticate that receipt.
        // Keep the target unreaped until its pre-exec evidence is retained;
        // pending evidence is not an exit observation.
        if self.try_observe_applied_launch()?.is_none() {
            return Ok(None);
        }
        let observation = self.process.try_observe_target_exit();
        let Some(observed) = (match observation {
            Ok(observed) => observed,
            Err(error) => {
                self.close_input();
                return Err(anyhow::Error::msg(error));
            }
        }) else {
            return Ok(None);
        };
        let exit = observed.exit();
        self.native_termination = Some(observed.into_termination());
        let failure = self
            .native_termination
            .as_ref()
            .and_then(lillux::LinuxSandboxTermination::launch_failure)
            .map(|failure| failure.diagnostic().to_owned());
        if let Some(failure) = failure {
            self.close_input();
            anyhow::bail!("external direct executable failed before workload launch: {failure}");
        }
        let exit = match exit {
            lillux::LinuxSandboxExit::Code(code) => ExternalTargetExit::Code(code),
            lillux::LinuxSandboxExit::Signal(signal) => ExternalTargetExit::Signal(signal),
        };
        self.target_exit = Some(exit);
        self.input = None;
        self.terminal = true;
        self.protocol.revoke();
        if let Some(input) = &mut self.direct_input {
            if !input.is_complete() {
                input.revoke();
            }
        }
        Ok(Some(exit))
    }

    pub fn cancel(
        &mut self,
        frame: &AuthenticatedExecutionFrame,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        self.require_owner(frame)?;
        ensure!(
            matches!(frame.frame().payload, ExecutionChannelPayload::Cancel),
            "not a cancellation command"
        );
        // A terminal revocation does not wait for a missing data predecessor.
        // The supervisor persists its sticky record before calling this method.
        self.application_fence.begin_terminal(frame.digest());
        self.stop(deadline)
    }

    fn close_input(&mut self) {
        self.terminal = true;
        self.protocol.revoke();
        if let Some(input) = &mut self.direct_input {
            input.revoke();
        }
        self.input = None;
    }

    pub fn execution_expired(&self) -> bool {
        self.deadline.has_elapsed()
    }

    /// Actual Lillux namespace settlement, not input closure or a stop intent.
    pub fn has_native_settlement(&self) -> bool {
        self.native_termination.is_some()
    }

    /// A blocked control-channel write cannot extend a live target's execution
    /// window. Only retained native settlement permits the separate bounded
    /// post-execution drain window; input closure/EOF alone is not settlement.
    pub fn control_io_deadline(
        &self,
        channel_deadline: lillux::time::MonotonicDeadline,
    ) -> lillux::time::MonotonicDeadline {
        let channel_deadline = channel_deadline.min(self.channel_deadline);
        if self.has_native_settlement() {
            channel_deadline
        } else {
            self.deadline.min(channel_deadline)
        }
    }

    /// Call from the finite supervisor control loop on cancellation, deadline,
    /// transport loss or endpoint failure. This settles local writers only.
    pub fn stop(&mut self, deadline: lillux::time::MonotonicDeadline) -> Result<()> {
        self.close_input();
        if self.native_termination.is_none() {
            self.native_termination = Some(
                self.process
                    .terminate_namespace_for_export_until(deadline)
                    .map_err(anyhow::Error::msg)?,
            );
        }
        Ok(())
    }

    pub fn capture(
        &mut self,
        frame: &AuthenticatedExecutionFrame,
        authority: &PinnedStateAuthority,
        guard: &CasMutationGuard,
        occurrence_stage: &mut DurableCasUploadStage,
        occurrence_digest: &str,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<NativeCandidateExport> {
        self.require_owner(frame)?;
        ensure!(
            self.binding.execution_mode == ExternalExecutionMode::StructuredSession {},
            "external direct command cannot produce a candidate export"
        );
        let completion = match &frame.frame().payload {
            ExecutionChannelPayload::Quiesce {
                completion_request_digest,
            } => completion_request_digest,
            _ => anyhow::bail!("external capture requires authenticated quiescence"),
        };
        let binding_digest = self.binding.digest()?;
        let occurrence_key = DurableCasPublicationKey::external_candidate_occurrence(
            &binding_digest,
            occurrence_digest,
        )?;
        occurrence_stage.ensure_publication_contract(&occurrence_key, None)?;
        ensure!(
            self.released
                && !self.terminal
                && self.protocol.pending.is_none()
                && self.application_fence.is_clear()
                && frame.frame().sequence > self.protocol.frontier,
            "external candidate is not capturable"
        );
        // Namespace termination and capture are one irreversible native
        // application. Fence subsequent actions before crossing that boundary.
        self.application_fence.begin(frame.digest())?;
        self.close_input();
        self.native_termination = Some(
            self.process
                .terminate_namespace_for_export_until(deadline.min(self.channel_deadline))
                .map_err(anyhow::Error::msg)?,
        );
        // Native settlement is true even if subsequent source capture fails.
        // Retain the exact proof for post-execution control I/O and one reap.
        let proof = self
            .native_termination
            .as_ref()
            .expect("native settlement retained above");
        ensure!(
            proof.launch_failure().is_none(),
            "external candidate never reached its workload: {}",
            proof
                .launch_failure()
                .map(|failure| failure.diagnostic())
                .unwrap_or("unknown pre-exec failure")
        );
        authority.ensure_guard(guard)?;
        // A writable candidate may rename an input mount's ancestor. Refuse
        // before any capture/publication instead of ingesting a relocated
        // backing placeholder under a path outside its admitted exclusion.
        verify_workspace_input_anchors(&self.workspace_input_anchors)?;
        // The live owner retains the exact admitted inode. Native launch may
        // mask its original pathname in the dedicated launcher's namespace;
        // all capture operations remain descriptor-relative. Reopening that
        // path is neither needed nor authorized recovery of this live owner.
        let cas = authority.cas_store()?;
        let base = VerifiedProjectSnapshotClosure::load(&cas, &self.binding.base_snapshot_hash)?;
        let output_authority = self.workspace_output_authority.clone();
        let mut retained_bytes = 0_u64;
        self.root.visit_regular_files_bounded(
            lillux::DirectoryTraversalBudget::new(
                ryeos_state::project_sync::MAX_PROJECT_TREE_ENTRIES,
                ryeos_state::project_sync::MAX_PROJECT_TREE_DEPTH,
            ),
            |_, _| {
                ensure!(
                    !self.channel_deadline.has_elapsed(),
                    "external candidate traversal expired"
                );
                Ok(false)
            },
            |_, file| {
                ensure!(
                    !self.channel_deadline.has_elapsed(),
                    "external candidate capture window expired"
                );
                retained_bytes = retained_bytes
                    .checked_add(lillux::observe_open_regular_file(&file)?.size())
                    .context("external candidate byte count overflow")?;
                ensure!(
                    retained_bytes <= MAX_CANDIDATE_CONTENT_BYTES,
                    "external candidate exceeds capture byte bound"
                );
                Ok(())
            },
        )?;
        // Project input mounts shadow this private backing tree. Their target
        // placeholders are namespace assembly, not candidate edits; preserve
        // any original base files hidden beneath those exact admitted roots.
        let capture_deadline = self.channel_deadline;
        ensure!(
            !capture_deadline.has_elapsed(),
            "external capture deadline expired"
        );
        let output_states = output_authority
            .as_ref()
            .map(|outputs| {
                ryeos_project_capture::capture_native_workspace_outputs(
                    authority,
                    guard,
                    occurrence_stage,
                    &self.root,
                    &outputs.partition,
                    base.tree().policy(),
                )
            })
            .transpose()?;
        let mut source_exclusions = output_authority
            .as_ref()
            .map(|outputs| {
                outputs
                    .partition
                    .roots
                    .iter()
                    .map(|root| root.path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        source_exclusions.extend(self.workspace_input_shadows.iter().cloned());
        source_exclusions.sort();
        source_exclusions.dedup();
        let mut tree = ryeos_project_capture::ingest_project_tree_bounded_with_exclusions(
            authority,
            guard,
            &self.root,
            base.tree().policy(),
            &source_exclusions,
            ryeos_project_capture::ProjectCaptureBudget {
                max_bytes: MAX_CANDIDATE_CONTENT_BYTES,
                deadline: capture_deadline,
            },
        )?;
        ryeos_project_capture::restore_operational_shadow_files(
            &mut tree,
            base.tree().tree(),
            &self.workspace_input_shadows,
        )?;
        ryeos_state::project_sync::validate_captured_policy_source(
            &cas,
            &tree,
            base.tree().policy(),
        )?;
        let snapshot = ryeos_state::objects::ProjectSnapshot {
            project_tree_hash: cas.store_object(&tree.to_value())?,
            effective_policy_hash: base.snapshot().effective_policy_hash.clone(),
            parent_hashes: vec![self.binding.base_snapshot_hash.clone()],
            message: None,
            source: "external_candidate_terminal_capture".into(),
            created_at: lillux::time::iso8601_now(),
        };
        let snapshot_hash = cas.store_object(&snapshot.to_value())?;
        let output_capture_hash = match (output_authority, output_states) {
            (None, None) => None,
            (Some(outputs), Some(states)) => {
                let input = self
                    .guest_inputs
                    .workspace_outputs
                    .as_ref()
                    .context("external workspace-output producer authority disappeared")?;
                let capture = ryeos_state::objects::WorkspaceOutputCapture {
                    schema: ryeos_state::objects::WORKSPACE_OUTPUT_CAPTURE_SCHEMA.to_owned(),
                    kind: ryeos_state::objects::WORKSPACE_OUTPUT_CAPTURE_KIND.to_owned(),
                    producer_chain_root_id: input.producer_chain_root_id.clone(),
                    producer_thread_id: input.producer_thread_id.clone(),
                    admitted_launch_capsule_hash: input.admitted_launch_capsule_hash.clone(),
                    base_project_snapshot_hash: self.binding.base_snapshot_hash.clone(),
                    result_project_snapshot_hash: snapshot_hash.clone(),
                    partition: outputs.partition,
                    outputs: states,
                };
                Some(occurrence_stage.store_object(guard, &cas, &capture.to_value()?)?)
            }
            _ => anyhow::bail!("external workspace output capture is incomplete"),
        };
        let observation = NativeWriterExclusionObservation {
            schema: 1,
            binding_digest: self.binding.digest()?,
            base_snapshot_hash: self.binding.base_snapshot_hash.clone(),
            completion_request_digest: completion.clone(),
            mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
            exit: match proof.exit() {
                lillux::LinuxSandboxExit::Code(n) => NativeNamespaceExit::Code(n),
                lillux::LinuxSandboxExit::Signal(n) => NativeNamespaceExit::Signal(n),
            },
        };
        observation.validate(&self.binding, completion)?;
        let writer_exclusion_blob_hash = cas
            .store_blob(lillux::canonical_json(&serde_json::to_value(observation)?)?.as_bytes())?;
        let captured = VerifiedProjectSnapshotClosure::load(&cas, &snapshot_hash)?;
        let mut object_hashes = std::collections::BTreeSet::from([
            snapshot_hash.clone(),
            captured.snapshot().project_tree_hash.clone(),
            captured.snapshot().effective_policy_hash.clone(),
        ]);
        object_hashes.extend(output_capture_hash.iter().cloned());
        let mut blob_hashes =
            std::collections::BTreeSet::from([writer_exclusion_blob_hash.clone()]);
        for (path, file) in captured.tree().files() {
            object_hashes.insert(captured.tree().tree().files[path].clone());
            blob_hashes.insert(file.blob_hash.clone());
        }
        occurrence_stage.protect_cas_closure(
            guard,
            object_hashes.iter().map(String::as_str),
            blob_hashes.iter().map(String::as_str),
        )?;
        ensure!(
            !capture_deadline.has_elapsed(),
            "external capture expired before completion"
        );
        // The caller must retain both outputs before releasing its guard.
        Ok(NativeCandidateExport {
            binding_digest,
            completion_request_digest: completion.clone(),
            occurrence_digest: occurrence_digest.to_owned(),
            durable_stage_id: occurrence_stage.staging_id().to_owned(),
            snapshot_hash,
            output_capture_hash,
            writer_exclusion_blob_hash,
        })
    }

    fn require_owner(&self, frame: &AuthenticatedExecutionFrame) -> Result<()> {
        ensure!(
            frame.frame().binding_digest == self.binding.digest()?
                && frame.frame().direction == ChannelDirection::OwnerToSupervisor,
            "external candidate command changed channel authority"
        );
        ensure!(
            !self.channel_deadline.has_elapsed()
                && lillux::time::timestamp_millis() < self.binding.expires_at_ms,
            "external candidate command channel expired"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applied_candidate_runtime_requires_exact_target_mounts_and_held_child() {
        let expected_target = lillux::LinuxSandboxAppliedLaunchCommitments {
            executable_sha256: [1; 32],
            argv_sha256: [2; 32],
            environment_sha256: [3; 32],
            cwd_sha256: [4; 32],
        };
        let expected_mounts = lillux::LinuxSandboxMountPreparationCommitments {
            schema: 1,
            mount_count: 2,
            destination_access_sha256: [5; 32],
        };
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 42,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: expected_target.executable_sha256,
            argv_sha256: expected_target.argv_sha256,
            environment_sha256: expected_target.environment_sha256,
            cwd_sha256: expected_target.cwd_sha256,
            post_release_mount_view: expected_mounts.clone(),
        };
        require_applied_candidate_runtime(&receipt, &expected_target, &expected_mounts, 42)
            .unwrap();
        assert!(
            require_applied_candidate_runtime(&receipt, &expected_target, &expected_mounts, 43)
                .is_err()
        );
        let mut wrong_target = receipt.clone();
        wrong_target.argv_sha256[0] ^= 1;
        assert!(
            require_applied_candidate_runtime(
                &wrong_target,
                &expected_target,
                &expected_mounts,
                42
            )
            .is_err()
        );
        let mut wrong_mounts = receipt.clone();
        wrong_mounts
            .post_release_mount_view
            .destination_access_sha256[0] ^= 1;
        assert!(
            require_applied_candidate_runtime(
                &wrong_mounts,
                &expected_target,
                &expected_mounts,
                42
            )
            .is_err()
        );
    }

    fn preparation_deadline() -> lillux::time::MonotonicDeadline {
        lillux::time::MonotonicDeadline::after(Duration::from_secs(10))
    }

    #[test]
    fn workspace_targets_prepare_absent_and_existing_typed_entries_without_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        std::fs::write(temp.path().join("existing.json"), b"retained base").unwrap();
        let targets = vec![
            ("vendor/runtime".into(), GuestMountKind::Directory),
            ("inputs/model.bin".into(), GuestMountKind::RegularFile),
            ("existing.json".into(), GuestMountKind::RegularFile),
        ];
        prepare_workspace_input_targets(&root, &targets, preparation_deadline()).unwrap();
        prepare_workspace_input_targets(&root, &targets, preparation_deadline()).unwrap();
        assert!(temp.path().join("vendor/runtime").is_dir());
        assert_eq!(
            std::fs::read(temp.path().join("inputs/model.bin")).unwrap(),
            b""
        );
        assert_eq!(
            std::fs::read(temp.path().join("existing.json")).unwrap(),
            b"retained base"
        );
        for (path, kind) in [
            ("vendor/runtime", GuestMountKind::RegularFile),
            ("existing.json", GuestMountKind::Directory),
            ("existing.json/child", GuestMountKind::Directory),
            ("../escape", GuestMountKind::Directory),
            ("/absolute", GuestMountKind::RegularFile),
        ] {
            assert!(
                prepare_workspace_input_targets(
                    &root,
                    &[(path.into(), kind)],
                    preparation_deadline(),
                )
                .is_err(),
                "accepted {path}"
            );
        }
        assert!(
            prepare_workspace_input_targets(
                &root,
                &[("too-late".into(), GuestMountKind::Directory)],
                lillux::time::MonotonicDeadline::after(Duration::ZERO),
            )
            .is_err()
        );
        assert!(!temp.path().join("too-late").exists());
    }

    #[test]
    fn workspace_targets_refuse_symlink_targets_and_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        std::fs::create_dir(temp.path().join("real")).unwrap();
        std::fs::write(temp.path().join("real/file"), b"original").unwrap();
        std::os::unix::fs::symlink("real", temp.path().join("alias")).unwrap();
        std::os::unix::fs::symlink("real/file", temp.path().join("link")).unwrap();
        for (path, kind) in [
            ("alias", GuestMountKind::Directory),
            ("alias/new", GuestMountKind::RegularFile),
            ("link", GuestMountKind::RegularFile),
        ] {
            assert!(
                prepare_workspace_input_targets(
                    &root,
                    &[(path.into(), kind)],
                    preparation_deadline(),
                )
                .is_err()
            );
        }
        assert!(!temp.path().join("real/new").exists());
        assert_eq!(
            std::fs::read(temp.path().join("real/file")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn workspace_outputs_cannot_overlap_input_shadows_in_either_direction() {
        let shadows = vec!["vendor/runtime".into(), "evidence/input.json".into()];
        for output in [
            "vendor",
            "vendor/runtime",
            "vendor/runtime/new",
            "evidence/input.json",
        ] {
            assert!(validate_workspace_output_separation([output], &shadows).is_err());
        }
        validate_workspace_output_separation(
            ["products", "vendor/runtime-extra", "evidence/output.json"],
            &shadows,
        )
        .unwrap();
    }

    #[test]
    fn workspace_input_anchors_refuse_renamed_ancestors_even_after_recreation() {
        let temp = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let anchors = prepare_workspace_input_targets(
            &root,
            &[("evidence/input.json".into(), GuestMountKind::RegularFile)],
            preparation_deadline(),
        )
        .unwrap();
        verify_workspace_input_anchors(&anchors).unwrap();
        std::fs::write(temp.path().join("evidence/unrelated.json"), b"valid edit").unwrap();
        verify_workspace_input_anchors(&anchors).unwrap();
        std::fs::rename(temp.path().join("evidence"), temp.path().join("moved")).unwrap();
        assert!(verify_workspace_input_anchors(&anchors).is_err());
        std::fs::create_dir(temp.path().join("evidence")).unwrap();
        std::fs::write(temp.path().join("evidence/input.json"), b"").unwrap();
        assert!(verify_workspace_input_anchors(&anchors).is_err());
    }

    #[test]
    fn workspace_input_anchors_refuse_replaced_file_and_directory_targets() {
        for kind in [GuestMountKind::RegularFile, GuestMountKind::Directory] {
            let temp = tempfile::tempdir().unwrap();
            let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
            let anchors = prepare_workspace_input_targets(
                &root,
                &[("input".into(), kind)],
                preparation_deadline(),
            )
            .unwrap();
            std::fs::rename(temp.path().join("input"), temp.path().join("moved")).unwrap();
            prepare_workspace_input_targets(
                &root,
                &[("input".into(), kind)],
                preparation_deadline(),
            )
            .unwrap();
            assert!(verify_workspace_input_anchors(&anchors).is_err());
        }
    }

    #[test]
    fn workspace_capture_excludes_targets_and_restores_hidden_base_content() {
        let temp = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        std::fs::create_dir_all(temp.path().join("vendor/existing")).unwrap();
        std::fs::write(temp.path().join("vendor/existing/base.py"), b"hidden base").unwrap();
        std::fs::write(temp.path().join("settings.json"), b"base settings").unwrap();
        std::fs::write(temp.path().join("policy.py"), b"original policy").unwrap();
        let db = ryeos_state::StateDb::open(
            state.path(),
            std::sync::Arc::new(ryeos_state::TrustStore::new()),
        )
        .unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let policy = ryeos_state::objects::ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            Default::default(),
        )
        .unwrap();
        let base =
            ryeos_project_capture::ingest_project_tree(&authority, &guard, &root, &policy).unwrap();
        let base_bytes = lillux::canonical_json(&base.to_value()).unwrap();
        let targets: Vec<(String, GuestMountKind)> = vec![
            ("evidence/new.json".into(), GuestMountKind::RegularFile),
            ("settings.json".into(), GuestMountKind::RegularFile),
            ("vendor/existing".into(), GuestMountKind::Directory),
            ("vendor/runtime".into(), GuestMountKind::Directory),
        ];
        let shadows = targets
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let anchors =
            prepare_workspace_input_targets(&root, &targets, preparation_deadline()).unwrap();
        let capture = || {
            verify_workspace_input_anchors(&anchors).unwrap();
            let mut tree = ryeos_project_capture::ingest_project_tree_bounded_with_exclusions(
                &authority,
                &guard,
                &root,
                &policy,
                &shadows,
                ryeos_project_capture::ProjectCaptureBudget {
                    max_bytes: 1024,
                    deadline: preparation_deadline(),
                },
            )
            .unwrap();
            ryeos_project_capture::restore_operational_shadow_files(&mut tree, &base, &shadows)
                .unwrap();
            tree
        };
        assert_eq!(
            capture().files,
            base.files,
            "namespace targets changed no-op candidate"
        );
        std::fs::write(temp.path().join("policy.py"), b"new policy").unwrap();
        // Adversarial backing-tree changes simulate hidden input content: even
        // those bytes must never replace retained base files during fold-back.
        std::fs::write(temp.path().join("settings.json"), b"not candidate").unwrap();
        std::fs::write(temp.path().join("vendor/existing/new.py"), b"not candidate").unwrap();
        let edited = capture();
        assert_ne!(edited.files["policy.py"], base.files["policy.py"]);
        assert_eq!(edited.files["settings.json"], base.files["settings.json"]);
        assert_eq!(
            edited.files["vendor/existing/base.py"],
            base.files["vendor/existing/base.py"]
        );
        assert!(!edited.files.contains_key("vendor/existing/new.py"));
        assert!(!edited.files.contains_key("evidence/new.json"));
        assert_eq!(
            lillux::canonical_json(&base.to_value()).unwrap(),
            base_bytes
        );
    }

    fn direct_input(bytes: &[u8]) -> DirectCommandInput {
        DirectCommandInput::from_sealed(
            &ryeos_state::external_execution::admission::ExternalDirectSealedInput::from_bytes(
                bytes,
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[derive(Clone, Copy)]
    enum WriteStep {
        Prefix(usize),
        Blocked,
        Interrupted,
        Broken,
    }

    #[derive(Default)]
    struct WriteEvidence {
        bytes: Vec<u8>,
        calls: usize,
        closes: usize,
    }

    struct ScriptedWriter {
        steps: std::collections::VecDeque<WriteStep>,
        evidence: std::rc::Rc<std::cell::RefCell<WriteEvidence>>,
    }

    impl std::io::Write for ScriptedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut evidence = self.evidence.borrow_mut();
            evidence.calls += 1;
            match self
                .steps
                .pop_front()
                .expect("unexpected additional stdin write")
            {
                WriteStep::Prefix(limit) => {
                    let count = limit.min(bytes.len());
                    evidence.bytes.extend_from_slice(&bytes[..count]);
                    Ok(count)
                }
                WriteStep::Blocked => Err(std::io::ErrorKind::WouldBlock.into()),
                WriteStep::Interrupted => Err(std::io::ErrorKind::Interrupted.into()),
                WriteStep::Broken => Err(std::io::ErrorKind::BrokenPipe.into()),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            panic!("one-shot input must not block on flush")
        }
    }

    impl Drop for ScriptedWriter {
        fn drop(&mut self) {
            self.evidence.borrow_mut().closes += 1;
        }
    }

    fn scripted_writer(
        steps: impl IntoIterator<Item = WriteStep>,
    ) -> (
        Option<ScriptedWriter>,
        std::rc::Rc<std::cell::RefCell<WriteEvidence>>,
    ) {
        let evidence = std::rc::Rc::new(std::cell::RefCell::new(WriteEvidence::default()));
        (
            Some(ScriptedWriter {
                steps: steps.into_iter().collect(),
                evidence: evidence.clone(),
            }),
            evidence,
        )
    }

    #[test]
    fn direct_stdin_requires_release_and_exact_durable_finish_before_any_write() {
        let mut input = direct_input(b"abc");
        let (mut writer, evidence) = scripted_writer([WriteStep::Prefix(1), WriteStep::Prefix(2)]);
        assert_eq!(
            input.pump(&mut writer).unwrap(),
            DirectCommandInputProgress::Fenced
        );
        input.begin_release("release").unwrap();
        assert!(input.begin_release("replacement-release").is_err());
        assert_eq!(
            input.pump(&mut writer).unwrap(),
            DirectCommandInputProgress::Fenced
        );
        input.acknowledge_release_finish("other-finish");
        assert_eq!(
            input.pump(&mut writer).unwrap(),
            DirectCommandInputProgress::Fenced
        );
        assert_eq!(evidence.borrow().calls, 0);
        input.acknowledge_release_finish("release");
        assert_eq!(
            input.pump(&mut writer).unwrap(),
            DirectCommandInputProgress::Pending {
                written_bytes: 1,
                total_bytes: 3
            }
        );
        // An equivalent acknowledgement never restarts an already applied prefix.
        input.acknowledge_release_finish("release");
        assert_eq!(
            input.pump(&mut writer).unwrap(),
            DirectCommandInputProgress::Complete { written_bytes: 3 }
        );
        assert_eq!(evidence.borrow().bytes, b"abc");
        assert_eq!(evidence.borrow().closes, 1);
        assert!(writer.is_none());
        assert!(input.is_complete());
        assert_eq!(
            input.pump(&mut writer).unwrap(),
            DirectCommandInputProgress::Complete { written_bytes: 3 }
        );
        assert_eq!(evidence.borrow().calls, 2);
    }

    #[test]
    fn direct_stdin_backpressure_retains_exact_prefix_without_renewing_authority() {
        let mut input = direct_input(b"abcde");
        let (mut writer, evidence) = scripted_writer([
            WriteStep::Prefix(2),
            WriteStep::Blocked,
            WriteStep::Interrupted,
            WriteStep::Prefix(9),
        ]);
        input.begin_release("release").unwrap();
        input.acknowledge_release_finish("release");
        for _ in 0..3 {
            assert_eq!(
                input.pump(&mut writer).unwrap(),
                DirectCommandInputProgress::Pending {
                    written_bytes: 2,
                    total_bytes: 5
                }
            );
        }
        assert_eq!(
            input.pump(&mut writer).unwrap(),
            DirectCommandInputProgress::Complete { written_bytes: 5 }
        );
        assert_eq!(evidence.borrow().bytes, b"abcde");
        assert_eq!(evidence.borrow().closes, 1);
        assert!(input.bytes.is_empty());
    }

    #[test]
    fn empty_direct_stdin_closes_once_only_after_authorized_release_finish() {
        let mut input = direct_input(b"");
        let (mut writer, evidence) = scripted_writer([]);
        input.begin_release("release").unwrap();
        assert_eq!(
            input.pump(&mut writer).unwrap(),
            DirectCommandInputProgress::Fenced
        );
        assert_eq!(evidence.borrow().closes, 0);
        input.acknowledge_release_finish("release");
        assert_eq!(
            input.pump(&mut writer).unwrap(),
            DirectCommandInputProgress::Complete { written_bytes: 0 }
        );
        assert_eq!(evidence.borrow().calls, 0);
        assert_eq!(evidence.borrow().closes, 1);
        input.acknowledge_release_finish("release");
        input.pump(&mut writer).unwrap();
        assert_eq!(evidence.borrow().closes, 1);
    }

    #[test]
    fn direct_stdin_zero_write_and_broken_pipe_are_sticky_failure_not_success() {
        for failure in [WriteStep::Prefix(0), WriteStep::Broken] {
            let mut input = direct_input(b"abc");
            let (mut writer, evidence) = scripted_writer([WriteStep::Prefix(1), failure]);
            input.begin_release("release").unwrap();
            input.acknowledge_release_finish("release");
            input.pump(&mut writer).unwrap();
            assert!(input.pump(&mut writer).is_err());
            assert!(!input.is_complete());
            assert!(input.bytes.is_empty());
            assert!(writer.is_none());
            assert_eq!(evidence.borrow().bytes, b"a");
            assert_eq!(evidence.borrow().closes, 1);
            input.acknowledge_release_finish("release");
            assert!(input.begin_release("release").is_err());
            assert!(input.pump(&mut writer).is_err());
            assert_eq!(evidence.borrow().calls, 2);
        }
    }

    #[test]
    fn direct_stdin_closed_before_first_byte_cannot_finish_or_rearm() {
        for peer_closed in [false, true] {
            let mut input = direct_input(b"sealed-input");
            let (mut writer, evidence) = scripted_writer([WriteStep::Broken]);
            if !peer_closed {
                // The local writer was already removed; this is distinct from
                // a live descriptor reporting that its peer closed the pipe.
                writer = None;
            }
            input.begin_release("release").unwrap();
            input.acknowledge_release_finish("release");
            assert!(input.pump(&mut writer).is_err());
            assert!(!input.is_complete());
            assert_eq!(input.offset, 0);
            assert!(input.bytes.is_empty());
            assert!(writer.is_none());
            assert!(evidence.borrow().bytes.is_empty());
            assert_eq!(evidence.borrow().closes, 1);
            assert_eq!(evidence.borrow().calls, usize::from(peer_closed));
            input.acknowledge_release_finish("release");
            assert!(input.begin_release("release").is_err());
            assert!(input.pump(&mut writer).is_err());
            assert!(!input.is_complete());
            assert_eq!(evidence.borrow().calls, usize::from(peer_closed));
            assert_eq!(evidence.borrow().closes, 1);
        }
    }

    #[test]
    fn direct_stdin_revocation_cannot_resume_even_after_late_release_finish() {
        for partial in [false, true] {
            let mut input = direct_input(b"abc");
            let (mut writer, evidence) = scripted_writer([WriteStep::Prefix(1)]);
            input.begin_release("release").unwrap();
            if partial {
                input.acknowledge_release_finish("release");
                input.pump(&mut writer).unwrap();
            }
            input.revoke();
            input.acknowledge_release_finish("release");
            assert!(input.pump(&mut writer).is_err());
            assert!(!input.is_complete());
            assert!(writer.is_none());
            assert_eq!(evidence.borrow().closes, 1);
            assert_eq!(evidence.borrow().calls, usize::from(partial));
        }
    }

    #[test]
    fn direct_execution_view_is_read_only_and_has_no_candidate_capture_authority() {
        let direct = ExternalExecutionMode::DirectCommand {
            stdout_max_bytes: 1024,
            stderr_max_bytes: 1024,
        };
        assert_eq!(
            execution_workspace_access(direct, false).unwrap(),
            lillux::LinuxSandboxMountAccess::ReadOnly
        );
        assert!(execution_workspace_access(direct, true).is_err());
        assert_eq!(
            execution_workspace_access(ExternalExecutionMode::StructuredSession {}, true).unwrap(),
            lillux::LinuxSandboxMountAccess::Writable
        );
        assert!(
            execution_workspace_access(
                ExternalExecutionMode::DirectCommand {
                    stdout_max_bytes: 0,
                    stderr_max_bytes: 1024,
                },
                false,
            )
            .is_err()
        );
    }

    struct BoundedWriter {
        bytes: Vec<u8>,
        limit: usize,
        blocked: bool,
    }

    impl std::io::Write for BoundedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.blocked {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let count = self.limit.min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn candidate_protocol_partial_write_never_restarts_or_crosses_revocation() {
        let mut input = CandidateProtocolInput::default();
        input.frontier = 1;
        let mut writer = BoundedWriter {
            bytes: Vec::new(),
            limit: 1,
            blocked: false,
        };
        input.begin(2, "first", b"abc".to_vec()).unwrap();
        assert_eq!(
            input.flush(&mut writer).unwrap(),
            CandidateProtocolProgress {
                frame_digest: "first".into(),
                written_bytes: 1,
                complete: false,
            }
        );
        assert!(input.begin(3, "second", b"xyz".to_vec()).is_err());
        writer.blocked = true;
        assert_eq!(input.flush(&mut writer).unwrap().written_bytes, 1);
        writer.blocked = false;
        writer.limit = 10;
        assert!(input.flush(&mut writer).unwrap().complete);
        assert!(input.begin(2, "first", b"abc".to_vec()).is_err());
        input.begin(3, "second", b"xyz".to_vec()).unwrap();
        input.revoke();
        assert!(input.flush(&mut writer).is_err());
        assert!(input.begin(4, "third", b"later".to_vec()).is_err());
        assert_eq!(writer.bytes, b"abc");
    }

    #[test]
    fn candidate_protocol_refuses_oversized_or_empty_input_without_moving_frontier() {
        let mut input = CandidateProtocolInput::default();
        for bytes in [
            Vec::new(),
            vec![0; ryeos_state::external_execution::MAX_CHUNK_BYTES + 1],
        ] {
            assert!(input.begin(1, "invalid", bytes).is_err());
            assert_eq!(input.frontier, 0);
            assert!(input.pending.is_none());
        }
    }

    #[test]
    fn application_fence_requires_exact_durable_finish_acknowledgement() {
        let mut fence = ApplicationFence::default();
        fence.begin("release-frame").unwrap();
        assert!(!fence.is_clear());
        assert!(fence.begin("later-frame").is_err());
        assert!(fence.acknowledge("wrong-frame").is_err());
        assert!(!fence.is_clear());
        fence.acknowledge("release-frame").unwrap();
        assert!(fence.is_clear());
        fence.acknowledge("release-frame").unwrap();
        fence.begin("partial-input").unwrap();
        fence.begin_terminal("cancel");
        assert!(fence.acknowledge("partial-input").is_err());
        fence.acknowledge("cancel").unwrap();
    }
}
