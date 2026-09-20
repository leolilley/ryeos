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
    ExecutionChannelPayload, MAX_CANDIDATE_CONTENT_BYTES, NativeNamespaceExit,
    NativeWriterExclusionMechanism, NativeWriterExclusionObservation,
};
use ryeos_state::project_materialization::VerifiedProjectSnapshotClosure;
use ryeos_state::{
    CasMutationGuard, DurableCasPublicationKey, DurableCasUploadStage,
    PinnedProjectMaterialization, PinnedStateAuthority,
};

pub struct NativeExternalCandidate {
    binding: ExecutionChannelBinding,
    process: lillux::HeldLinuxSandboxProcess,
    root: lillux::PinnedDirectory,
    deadline: Instant,
    channel_deadline: Instant,
    released: bool,
    terminal: bool,
    input: Option<std::fs::File>,
    protocol: CandidateProtocolInput,
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

#[derive(Default)]
struct ApplicationFence(Option<String>);

impl ApplicationFence {
    fn begin(&mut self, digest: &str) -> Result<()> {
        ensure!(self.0.is_none(), "candidate already awaits durable finish");
        self.0 = Some(digest.to_owned());
        Ok(())
    }

    fn acknowledge(&mut self, digest: &str) -> Result<()> {
        ensure!(
            self.0.as_deref() == Some(digest),
            "candidate finish acknowledgement changed its exact application"
        );
        self.0 = None;
        Ok(())
    }

    /// Terminal cancellation may overtake an uncertain input claim. The
    /// interrupted input stays durably claimed and is never retried; the live
    /// launcher now waits only for the cancellation finish acknowledgement.
    fn begin_terminal(&mut self, digest: &str) {
        self.0 = Some(digest.to_owned());
    }

    fn is_clear(&self) -> bool {
        self.0.is_none()
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
    writer_exclusion_blob_hash: String,
}

impl NativeCandidateExport {
    pub fn snapshot_hash(&self) -> &str {
        &self.snapshot_hash
    }
    pub fn writer_exclusion_blob_hash(&self) -> &str {
        &self.writer_exclusion_blob_hash
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
    pub fn prepare(
        binding: ExecutionChannelBinding,
        authority: &PinnedStateAuthority,
        private_parent: &lillux::PinnedDirectory,
        mut request: lillux::LinuxSandboxRequest,
    ) -> Result<(Self, NativeCandidateOutput)> {
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
                    total <= MAX_CANDIDATE_CONTENT_BYTES,
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
                input: Some(pipes.stdin),
                protocol: CandidateProtocolInput::default(),
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
                && Instant::now() < self.deadline,
            "external candidate is terminal, released, awaiting finish or expired"
        );
        // A release error may have crossed the boundary. Never retry it.
        self.released = true;
        self.protocol.frontier = frame.frame().sequence;
        self.application_fence.begin(frame.digest())?;
        if let Err(error) = self.process.release_once() {
            self.close_input();
            return Err(anyhow::Error::msg(error));
        }
        Ok(())
    }

    /// Called only after the supervisor's exact durable application claim and
    /// sticky-revocation check. This object cannot be reconstructed to replay
    /// an uncertain command. Its mutable owner serializes writes with stop.
    pub fn begin_protocol_input(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()> {
        self.require_owner(frame)?;
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

    /// Release the native action fence only after the protected supervisor has
    /// committed `finish_application` for this exact frame. A lost response or
    /// acknowledgement leaves the launcher fenced rather than permitting a
    /// second action after an uncertain effect.
    pub fn acknowledge_application_finish(&mut self, frame_digest: &str) -> Result<()> {
        self.application_fence.acknowledge(frame_digest)
    }

    pub fn cancel(&mut self, frame: &AuthenticatedExecutionFrame, timeout: Duration) -> Result<()> {
        self.require_owner(frame)?;
        ensure!(
            matches!(frame.frame().payload, ExecutionChannelPayload::Cancel),
            "not a cancellation command"
        );
        // A terminal revocation does not wait for a missing data predecessor.
        // The supervisor persists its sticky record before calling this method.
        self.application_fence.begin_terminal(frame.digest());
        self.stop(timeout)
    }

    fn close_input(&mut self) {
        self.terminal = true;
        self.protocol.revoke();
        self.input = None;
    }

    pub fn execution_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Call from the finite supervisor control loop on cancellation, deadline,
    /// transport loss or endpoint failure. This settles local writers only.
    pub fn stop(&mut self, timeout: Duration) -> Result<()> {
        self.close_input();
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
        occurrence_stage: &mut DurableCasUploadStage,
        occurrence_digest: &str,
        timeout: Duration,
    ) -> Result<NativeCandidateExport> {
        self.require_owner(frame)?;
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
        let proof = self
            .process
            .terminate_namespace_for_export(timeout)
            .map_err(anyhow::Error::msg)?;
        authority.ensure_guard(guard)?;
        // The live owner retains the exact admitted inode. Native launch may
        // mask its original pathname in the dedicated launcher's namespace;
        // all capture operations remain descriptor-relative. Reopening that
        // path is neither needed nor authorized recovery of this live owner.
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
                    retained_bytes <= MAX_CANDIDATE_CONTENT_BYTES,
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
                max_bytes: MAX_CANDIDATE_CONTENT_BYTES,
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
        let captured = VerifiedProjectSnapshotClosure::load(&cas, &snapshot_hash)?;
        let mut object_hashes = std::collections::BTreeSet::from([
            snapshot_hash.clone(),
            captured.snapshot().project_tree_hash.clone(),
            captured.snapshot().effective_policy_hash.clone(),
        ]);
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
            Instant::now() < capture_deadline,
            "external capture expired before completion"
        );
        // The caller must retain both outputs before releasing its guard.
        Ok(NativeCandidateExport {
            binding_digest,
            completion_request_digest: completion.clone(),
            occurrence_digest: occurrence_digest.to_owned(),
            durable_stage_id: occurrence_stage.staging_id().to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(fence.acknowledge("release-frame").is_err());
        fence.begin("partial-input").unwrap();
        fence.begin_terminal("cancel");
        assert!(fence.acknowledge("partial-input").is_err());
        fence.acknowledge("cancel").unwrap();
    }
}
