//! Protected guest-side candidate ownership, independent of cloud mechanics.
//!
//! This component runs in a dedicated trusted launcher process: native Lillux
//! setup changes its caller's namespaces. A transport adapter must not call it
//! on a node's async executor. No credentials or channel keys are mounted into
//! the endpoint. The caller supplies already-admitted runtime descriptors and
//! a protected parent directory. This component creates fresh candidate inodes
//! from CAS; it never makes a shared materialization writable.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use ryeos_state::external_execution::{
    AuthenticatedExecutionFrame, ChannelDirection, ExecutionChannelBinding,
    ExecutionChannelPayload, NativeNamespaceExit, NativeWriterExclusionMechanism,
    NativeWriterExclusionObservation,
};
use ryeos_state::project_materialization::VerifiedProjectSnapshotClosure;
use ryeos_state::{CasMutationGuard, PinnedProjectMaterialization, PinnedStateAuthority};

pub struct NativeExternalCandidate {
    binding: ExecutionChannelBinding,
    process: lillux::HeldLinuxSandboxProcess,
    root: lillux::PinnedDirectory,
    deadline: Instant,
    channel_deadline: Instant,
    released: bool,
    terminal: bool,
}

/// This value can only be returned after exact native namespace termination
/// and complete project capture. It is local testimony, not cloud cleanup.
pub struct NativeCandidateExport {
    snapshot_hash: String,
    writer_exclusion_blob_hash: String,
}

impl NativeCandidateExport {
    pub fn snapshot_hash(&self) -> &str {
        &self.snapshot_hash
    }
    pub fn writer_exclusion_blob_hash(&self) -> &str {
        &self.writer_exclusion_blob_hash
    }
}

impl NativeExternalCandidate {
    pub fn prepare(
        binding: ExecutionChannelBinding,
        authority: &PinnedStateAuthority,
        private_parent: &lillux::PinnedDirectory,
        mut request: lillux::LinuxSandboxRequest,
    ) -> Result<(Self, lillux::LinuxSandboxPipes)> {
        binding.validate()?;
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
        for mount in &request.mounts {
            ensure!(
                mount.access == lillux::LinuxSandboxMountAccess::ReadOnly
                    && !mount.destination.starts_with(&workspace)
                    && !workspace.starts_with(&mount.destination),
                "external runtime mount shadows or adds writable candidate authority"
            );
        }
        let now = lillux::time::timestamp_millis();
        ensure!(
            now >= binding.issued_at_ms && now < binding.execution_deadline_ms,
            "external candidate preparation is outside its execution window"
        );
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(u64::try_from(
                binding.execution_deadline_ms - now,
            )?))
            .context("guest deadline overflow")?;
        let channel_deadline = Instant::now()
            .checked_add(Duration::from_millis(u64::try_from(
                binding.expires_at_ms - now,
            )?))
            .context("guest channel deadline overflow")?;
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
                    total <= 1024 * 1024 * 1024,
                    "external base exceeds first-generation private-copy bound"
                );
            }
            let (_, root) = private_parent.create_unique_child("external-candidate-", 0o700)?;
            for (path, file) in closure.tree().files() {
                ensure!(
                    Instant::now() < deadline,
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
            PinnedProjectMaterialization::verify_from_closure(
                authority,
                &guard,
                &closure,
                root.path(),
            )?
        };
        ensure!(
            Instant::now() < deadline,
            "external candidate preparation exhausted its execution window"
        );
        let candidate_mount = base.verified_mount_descriptor()?;
        let root = base.try_clone_root()?;
        request.mounts.push(lillux::LinuxSandboxMount {
            source_fd: candidate_mount
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            destination: workspace,
            access: lillux::LinuxSandboxMountAccess::Writable,
            layer: 0,
        });
        let (process, pipes) =
            lillux::prepare_linux_sandbox_piped(request).map_err(anyhow::Error::msg)?;
        Ok((
            Self {
                binding,
                process,
                root,
                deadline,
                channel_deadline,
                released: false,
                terminal: false,
            },
            pipes,
        ))
    }

    pub fn release(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()> {
        self.require_owner(frame)?;
        ensure!(
            matches!(frame.frame().payload, ExecutionChannelPayload::Release),
            "not an external release command"
        );
        ensure!(
            !self.terminal && !self.released && Instant::now() < self.deadline,
            "external candidate is terminal, released or expired"
        );
        // A release error may have crossed the boundary. Never retry it.
        self.released = true;
        self.process.release_once().map_err(anyhow::Error::msg)
    }

    pub fn execution_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Call from the finite supervisor control loop on cancellation, deadline,
    /// transport loss or endpoint failure. This settles local writers only.
    pub fn stop(&mut self, timeout: Duration) -> Result<()> {
        self.terminal = true;
        self.process
            .terminate_namespace_for_export(timeout)
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    pub fn capture(
        &mut self,
        frame: &AuthenticatedExecutionFrame,
        authority: &PinnedStateAuthority,
        guard: &CasMutationGuard,
        timeout: Duration,
    ) -> Result<NativeCandidateExport> {
        self.require_owner(frame)?;
        let completion = match &frame.frame().payload {
            ExecutionChannelPayload::Quiesce {
                completion_request_digest,
            } => completion_request_digest,
            _ => anyhow::bail!("external capture requires authenticated quiescence"),
        };
        ensure!(
            self.released && !self.terminal,
            "external candidate is not capturable"
        );
        self.terminal = true;
        let proof = self
            .process
            .terminate_namespace_for_export(timeout)
            .map_err(anyhow::Error::msg)?;
        authority.ensure_guard(guard)?;
        self.root.ensure_path_binding()?;
        let cas = authority.cas_store()?;
        let base = VerifiedProjectSnapshotClosure::load(&cas, &self.binding.base_snapshot_hash)?;
        let mut retained_bytes = 0_u64;
        self.root.visit_regular_files_bounded(
            lillux::DirectoryTraversalBudget::new(
                ryeos_state::project_sync::MAX_PROJECT_TREE_ENTRIES,
                ryeos_state::project_sync::MAX_PROJECT_TREE_DEPTH,
            ),
            |_, _| {
                ensure!(
                    Instant::now() < self.channel_deadline,
                    "external candidate traversal expired"
                );
                Ok(false)
            },
            |_, file| {
                ensure!(
                    Instant::now() < self.channel_deadline,
                    "external candidate capture window expired"
                );
                retained_bytes = retained_bytes
                    .checked_add(file.metadata()?.len())
                    .context("external candidate byte count overflow")?;
                ensure!(
                    retained_bytes <= 1024 * 1024 * 1024,
                    "external candidate exceeds capture byte bound"
                );
                Ok(())
            },
        )?;
        // Runtime mounts live outside /workspace. No profile, supervisor state
        // or separate input mount is enumerated as candidate content.
        let capture_deadline = self.channel_deadline;
        ensure!(
            Instant::now() < capture_deadline,
            "external capture deadline expired"
        );
        let tree = super::ingest::ingest_project_tree_bounded(
            authority,
            guard,
            &self.root,
            base.tree().policy(),
            super::ingest::ProjectCaptureBudget {
                max_bytes: 1024 * 1024 * 1024,
                deadline: capture_deadline,
            },
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
        ensure!(
            Instant::now() < capture_deadline,
            "external capture expired before completion"
        );
        // The caller must retain both outputs before releasing its guard.
        Ok(NativeCandidateExport {
            snapshot_hash,
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
            Instant::now() < self.channel_deadline
                && lillux::time::timestamp_millis() < self.binding.expires_at_ms,
            "external candidate command channel expired"
        );
        Ok(())
    }
}
