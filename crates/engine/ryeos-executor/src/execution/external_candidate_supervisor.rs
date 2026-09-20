//! Serialized protected-supervisor application loop.
//!
//! The production launcher client is an authenticated inherited duplex peer;
//! this owner never contains `NativeExternalCandidate` itself. Every method
//! crosses the guest journal's SQLite writer gate around exactly one bounded
//! launcher request and waits for the exact durable-finish acknowledgement
//! before permitting a later action.

use anyhow::{Context as _, Result, ensure};
use ryeos_state::external_execution::export::CandidateExportAssembler;
use ryeos_state::external_execution::guest_journal::{
    DurableNativeCandidateCapture, GuestApplicationClaim, GuestApplicationToken,
    GuestProtocolApplication, GuestTerminalApplicationClaim, LiveGuestJournal,
    RevocationAppendOutcome,
};
use ryeos_state::external_execution::{
    AuthenticatedExecutionFrame, ChannelDirection, ExecutionChannelPayload,
};
use ryeos_state::{
    DurableCasPublicationKey, DurableExternalCandidateReceipt, PinnedStateAuthority,
};

/// Narrow live-launcher capability. Implementations must represent one exact
/// already-bound occurrence; reconnecting or recreating a launcher is not an
/// implementation of this trait.
pub trait ExternalCandidateLauncherClient {
    fn release(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()>;
    fn apply_protocol_chunk(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<usize>;
    fn cancel(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()>;
    fn capture(
        &mut self,
        frame: &AuthenticatedExecutionFrame,
    ) -> Result<DurableNativeCandidateCapture>;
    fn acknowledge_finish(&mut self, frame_digest: &str) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorApplicationOutcome {
    Applied,
    Partial,
    FinishReconciled,
    ClaimedUnknown,
    Revoked,
    RevokedAwaitingCleanup,
}

pub struct SupervisorCaptureOutcome {
    pub receipt: DurableExternalCandidateReceipt,
    pub sealed_frame: String,
}

pub struct SerializedExternalCandidateSupervisor<L> {
    journal: LiveGuestJournal,
    launcher: L,
    pending_protocol: Option<GuestApplicationToken>,
}

impl<L: ExternalCandidateLauncherClient> SerializedExternalCandidateSupervisor<L> {
    pub fn new(journal: LiveGuestJournal, launcher: L) -> Self {
        Self {
            journal,
            launcher,
            pending_protocol: None,
        }
    }

    pub fn journal(&self) -> &LiveGuestJournal {
        &self.journal
    }

    /// Retain a controller-authored acknowledgement without inventing an
    /// executable application for it. The shared journal reconciles the exact
    /// supervisor frame state named by the signed payload.
    pub fn record_owner_acknowledgement(&self, wire: &[u8]) -> Result<bool> {
        let verified = ryeos_state::external_execution::SignedExecutionFrame::decode_and_verify(
            wire,
            self.journal.binding(),
            lillux::time::timestamp_millis(),
        )?;
        ensure!(
            verified.frame().direction == ChannelDirection::OwnerToSupervisor
                && matches!(
                    verified.frame().payload,
                    ExecutionChannelPayload::Acknowledge { .. }
                ),
            "supervisor acknowledgement ingress requires an owner acknowledgement"
        );
        self.journal.record_owner_acknowledgement(wire)
    }

    pub fn ensure_supervisor_acknowledgement(
        &self,
        signing_key: &lillux::crypto::SigningKey,
        peer_sequence: u64,
        peer_digest: &str,
    ) -> Result<Option<AuthenticatedExecutionFrame>> {
        self.journal
            .ensure_supervisor_acknowledgement(signing_key, peer_sequence, peer_digest)
    }

    pub fn publish_ready(
        &self,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<AuthenticatedExecutionFrame> {
        let binding = self.journal.binding();
        self.journal.author_supervisor_frame(
            signing_key,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        )
    }

    pub fn dispatch_release(&mut self, wire: &[u8]) -> Result<SupervisorApplicationOutcome> {
        ensure!(
            self.pending_protocol.is_none(),
            "release cannot overtake pending protocol input"
        );
        let digest = lillux::sha256_hex(wire);
        match self.journal.record_and_claim(wire)? {
            GuestApplicationClaim::New(token) => {
                let (_, performed) = self
                    .journal
                    .apply_once(token, |frame| self.launcher.release(frame))?;
                let ack = self.journal.finish(performed)?;
                self.launcher.acknowledge_finish(ack.frame_digest())?;
                Ok(SupervisorApplicationOutcome::Applied)
            }
            GuestApplicationClaim::AlreadyApplied => {
                self.launcher.acknowledge_finish(&digest)?;
                Ok(SupervisorApplicationOutcome::FinishReconciled)
            }
            GuestApplicationClaim::AlreadyClaimed => {
                Ok(SupervisorApplicationOutcome::ClaimedUnknown)
            }
            GuestApplicationClaim::Revoked => Ok(SupervisorApplicationOutcome::Revoked),
        }
    }

    /// Apply at most one native pipe write. The same opaque claim token is
    /// retained between partial writes; a different wire cannot replace it.
    pub fn dispatch_protocol_chunk(&mut self, wire: &[u8]) -> Result<SupervisorApplicationOutcome> {
        let digest = lillux::sha256_hex(wire);
        let token = match self.pending_protocol.take() {
            Some(token) => {
                ensure!(
                    token.frame_digest() == digest,
                    "protocol continuation changed its claimed frame"
                );
                token
            }
            None => match self.journal.record_and_claim(wire)? {
                GuestApplicationClaim::New(token) => token,
                GuestApplicationClaim::AlreadyApplied => {
                    self.launcher.acknowledge_finish(&digest)?;
                    return Ok(SupervisorApplicationOutcome::FinishReconciled);
                }
                GuestApplicationClaim::AlreadyClaimed => {
                    return Ok(SupervisorApplicationOutcome::ClaimedUnknown);
                }
                GuestApplicationClaim::Revoked => {
                    return Ok(SupervisorApplicationOutcome::Revoked);
                }
            },
        };
        match self
            .journal
            .apply_protocol_chunk(token, |frame, _| self.launcher.apply_protocol_chunk(frame))?
        {
            GuestProtocolApplication::Pending(token) => {
                self.pending_protocol = Some(token);
                Ok(SupervisorApplicationOutcome::Partial)
            }
            GuestProtocolApplication::Performed(performed) => {
                let ack = self.journal.finish(performed)?;
                self.launcher.acknowledge_finish(ack.frame_digest())?;
                Ok(SupervisorApplicationOutcome::Applied)
            }
        }
    }

    pub fn dispatch_cancel(&mut self, wire: &[u8]) -> Result<SupervisorApplicationOutcome> {
        // Cancellation terminally overtakes a process-local partial token. Its
        // durable input claim stays unknown and can never be reconstructed.
        self.pending_protocol = None;
        let committed = self.journal.commit_terminal_revocation(wire)?;
        let appended = self.journal.try_append_terminal_observation(&committed);
        let observation_blocked = matches!(appended, RevocationAppendOutcome::Blocked { .. });
        match self.journal.claim_terminal_revocation(&committed)? {
            GuestTerminalApplicationClaim::New(token) => {
                let (_, performed) = self
                    .journal
                    .apply_terminal_revocation(token, |frame| self.launcher.cancel(frame))?;
                let ack = self.journal.finish_terminal_revocation(performed)?;
                self.launcher.acknowledge_finish(ack.frame_digest())?;
                Ok(if observation_blocked {
                    SupervisorApplicationOutcome::RevokedAwaitingCleanup
                } else {
                    SupervisorApplicationOutcome::Applied
                })
            }
            GuestTerminalApplicationClaim::AlreadyApplied => {
                self.launcher.acknowledge_finish(committed.frame_digest())?;
                Ok(if observation_blocked {
                    SupervisorApplicationOutcome::RevokedAwaitingCleanup
                } else {
                    SupervisorApplicationOutcome::FinishReconciled
                })
            }
            GuestTerminalApplicationClaim::AlreadyClaimed => {
                Ok(SupervisorApplicationOutcome::RevokedAwaitingCleanup)
            }
        }
    }

    pub fn dispatch_quiesce(
        &mut self,
        wire: &[u8],
        authority: &PinnedStateAuthority,
        signing_key: &lillux::crypto::SigningKey,
        occurrence_digest: &str,
        bootstrap_digest: &str,
    ) -> Result<SupervisorCaptureOutcome> {
        ensure!(
            self.pending_protocol.is_none(),
            "quiescence cannot overtake pending protocol input"
        );
        let quiesce = ryeos_state::external_execution::SignedExecutionFrame::decode_and_verify(
            wire,
            self.journal.binding(),
            lillux::time::timestamp_millis(),
        )?;
        let capture = match self.journal.record_and_claim(wire)? {
            GuestApplicationClaim::New(token) => {
                let (capture, performed) = self
                    .journal
                    .apply_once(token, |frame| self.launcher.capture(frame))?;
                ensure!(
                    capture.occurrence_digest == occurrence_digest,
                    "launcher capture changed its exact occurrence"
                );
                self.journal
                    .record_native_capture(authority, &quiesce, &capture)?;
                let acknowledged = self.journal.finish(performed)?;
                self.launcher
                    .acknowledge_finish(acknowledged.frame_digest())?;
                capture
            }
            GuestApplicationClaim::AlreadyClaimed | GuestApplicationClaim::AlreadyApplied => {
                let capture = self
                    .journal
                    .native_capture_for_quiesce(quiesce.digest())?
                    .context(
                        "quiescence is claimed/applied without durable native capture evidence",
                    )?;
                ensure!(
                    capture.occurrence_digest == occurrence_digest,
                    "durable capture changed its exact occurrence"
                );
                self.journal
                    .record_native_capture(authority, &quiesce, &capture)?;
                let acknowledged = self
                    .journal
                    .reconcile_native_capture_application(&capture)?;
                self.launcher
                    .acknowledge_finish(acknowledged.frame_digest())?;
                capture
            }
            GuestApplicationClaim::Revoked => {
                anyhow::bail!("external quiescence was durably revoked")
            }
        };
        let sealed = match self.journal.supervisor_export_for_capture(&capture)? {
            Some(sealed) => sealed,
            None => self.journal.author_supervisor_frame(
                signing_key,
                ExecutionChannelPayload::ExportSealed {
                    candidate_snapshot_hash: capture.snapshot_hash.clone(),
                    completion_request_digest: capture.completion_request_digest.clone(),
                    writer_exclusion_evidence_hash: capture.writer_exclusion_evidence_hash.clone(),
                },
            )?,
        };
        let guard = authority.acquire_shared_guard()?;
        let mut assembler = CandidateExportAssembler::new(
            authority,
            &guard,
            self.journal.binding().clone(),
            &quiesce,
        )?;
        let imported = assembler
            .accept(&sealed)?
            .context("sealed external candidate did not complete validation")?;
        let retained = imported.validate_retention(authority, &guard, self.journal.binding())?;
        let publication_key = DurableCasPublicationKey::external_candidate_occurrence(
            &self.journal.binding().digest()?,
            occurrence_digest,
        )?;
        let mut stage = authority
            .require_recovery()?
            .open_durable_cas_upload_admitted(
                &guard,
                &capture.durable_stage_id,
                bootstrap_digest,
            )?;
        stage.ensure_publication_contract(&publication_key, None)?;
        let receipt = self.journal.retain_export(
            authority,
            &retained,
            &sealed,
            occurrence_digest,
            &mut stage,
        )?;
        // Native retention proves that these exact candidate bytes remain
        // available to transfer. It does not apply the supervisor-authored
        // ExportSealed observation at the controller. Leave the outbound frame
        // pending until signed controller acknowledgement reports its actual
        // destination-side application state.
        Ok(SupervisorCaptureOutcome {
            receipt,
            sealed_frame: sealed.canonical().to_owned(),
        })
    }

    pub fn into_parts(self) -> (LiveGuestJournal, L) {
        (self.journal, self.launcher)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ryeos_state::external_execution::guest_journal::{
        AuthenticatedLauncherReady, LauncherOccurrenceEvidence, PreparedGuestJournal,
    };
    use ryeos_state::external_execution::{
        ExecutionChannelBinding, ExecutionFrame, NativeNamespaceExit,
        NativeWriterExclusionMechanism, NativeWriterExclusionObservation, SignedExecutionFrame,
    };
    use ryeos_state::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};

    use super::*;

    #[derive(Default)]
    struct RecordingLauncher {
        capture: Option<DurableNativeCandidateCapture>,
        releases: usize,
        captures: usize,
        finish_acks: Vec<String>,
    }

    impl ExternalCandidateLauncherClient for RecordingLauncher {
        fn release(&mut self, _frame: &AuthenticatedExecutionFrame) -> Result<()> {
            self.releases += 1;
            Ok(())
        }

        fn apply_protocol_chunk(&mut self, _frame: &AuthenticatedExecutionFrame) -> Result<usize> {
            anyhow::bail!("test launcher does not admit protocol input")
        }

        fn cancel(&mut self, _frame: &AuthenticatedExecutionFrame) -> Result<()> {
            anyhow::bail!("test launcher does not admit cancellation")
        }

        fn capture(
            &mut self,
            _frame: &AuthenticatedExecutionFrame,
        ) -> Result<DurableNativeCandidateCapture> {
            self.captures += 1;
            self.capture.clone().context("test capture is absent")
        }

        fn acknowledge_finish(&mut self, frame_digest: &str) -> Result<()> {
            self.finish_acks.push(frame_digest.to_owned());
            Ok(())
        }
    }

    fn signed_owner_frame(
        binding: &ExecutionChannelBinding,
        owner: &lillux::crypto::SigningKey,
        sequence: u64,
        previous_frame_digest: Option<String>,
        payload: ExecutionChannelPayload,
    ) -> AuthenticatedExecutionFrame {
        let signed = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction: ChannelDirection::OwnerToSupervisor,
                sequence,
                previous_frame_digest,
                acknowledged_peer_sequence: 1,
                payload,
            },
            binding,
            owner,
        )
        .unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(signed).unwrap()).unwrap();
        SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            binding,
            lillux::time::timestamp_millis(),
        )
        .unwrap()
    }

    #[test]
    fn complete_capture_replay_reuses_exact_receipt_without_second_native_capture() {
        let state_root = tempfile::tempdir().unwrap();
        let state =
            ryeos_state::StateDb::open(state_root.path(), Arc::new(ryeos_state::TrustStore::new()))
                .unwrap();
        let authority = state.pinned_authority().unwrap();
        let cas = authority.cas_store().unwrap();
        let policy = ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            Default::default(),
        )
        .unwrap();
        let policy_hash = cas.store_object(&policy.to_value()).unwrap();
        let candidate_bytes = b"print('retained external candidate')\n";
        let candidate_blob_hash = cas.store_blob(candidate_bytes).unwrap();
        let file = ProjectFile {
            blob_hash: candidate_blob_hash.clone(),
            size: candidate_bytes.len() as u64,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let file_hash = cas.store_object(&file.to_value()).unwrap();
        let tree = ProjectTree {
            files: [("main.py".to_owned(), file_hash.clone())]
                .into_iter()
                .collect(),
        };
        let tree_hash = cas.store_object(&tree.to_value()).unwrap();
        let base = ProjectSnapshot {
            project_tree_hash: tree_hash.clone(),
            effective_policy_hash: policy_hash.clone(),
            parent_hashes: vec![],
            created_at: "2026-09-20T00:00:00Z".into(),
            message: None,
            source: "external-supervisor-replay-base".into(),
        };
        let base_hash = cas.store_object(&base.to_value()).unwrap();
        let candidate = ProjectSnapshot {
            project_tree_hash: tree_hash.clone(),
            effective_policy_hash: policy_hash.clone(),
            parent_hashes: vec![base_hash.clone()],
            created_at: "2026-09-20T00:00:01Z".into(),
            message: None,
            source: "external_candidate_terminal_capture".into(),
        };
        let snapshot_hash = cas.store_object(&candidate.to_value()).unwrap();

        let owner = lillux::crypto::generate_signing_key();
        let supervisor_key = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        let binding = ExecutionChannelBinding {
            schema: 1,
            placement_thread_id: "T-external-supervisor-replay".into(),
            allocation_request_digest: "a".repeat(64),
            occurrence_id: "occurrence-external-supervisor-replay".into(),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: base_hash,
            execution_binding_hash: "c".repeat(64),
            supervisor_runtime_hash: "d".repeat(64),
            channel_nonce: "e".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor_key.verifying_key().as_bytes()),
            issued_at_ms: now - 1_000,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            max_frames: 32,
            max_bytes: 2 * 1024 * 1024,
        };
        let journal_root = tempfile::tempdir().unwrap();
        let journal_directory = lillux::PinnedDirectory::open(journal_root.path())
            .unwrap()
            .unwrap();
        journal_directory.tighten_owner_private_directory().unwrap();
        let bootstrap_digest = "f".repeat(64);
        let prepared = PreparedGuestJournal::create(
            journal_directory,
            &authority,
            &bootstrap_digest,
            binding.clone(),
        )
        .unwrap();
        let occurrence = LauncherOccurrenceEvidence::from_held_launcher(
            lillux::ExactProcessIdentity {
                boot_id: "external-supervisor-replay-boot".into(),
                target_pid: 200,
                target_start_time_ticks: 300,
                group_leader_pid: 200,
                group_leader_start_time_ticks: 300,
            },
            &"1".repeat(64),
            &"2".repeat(64),
        )
        .unwrap();
        let occurrence_digest = occurrence.digest().unwrap();
        let live = prepared
            .bind_launcher(occurrence)
            .unwrap()
            .mark_launcher_ready(
                AuthenticatedLauncherReady::from_handshake_transcript(&"3".repeat(64)).unwrap(),
            )
            .unwrap();

        let completion_request_digest = "4".repeat(64);
        let observation = NativeWriterExclusionObservation {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            base_snapshot_hash: binding.base_snapshot_hash.clone(),
            completion_request_digest: completion_request_digest.clone(),
            mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
            exit: NativeNamespaceExit::Code(0),
        };
        let writer_exclusion_evidence_hash = cas
            .store_blob(
                lillux::canonical_json(&serde_json::to_value(observation).unwrap())
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let publication_key = DurableCasPublicationKey::external_candidate_occurrence(
            &binding.digest().unwrap(),
            &occurrence_digest,
        )
        .unwrap();
        let mut stage = authority
            .require_recovery()
            .unwrap()
            .begin_durable_cas_upload_admitted(
                &guard,
                &bootstrap_digest,
                "external-supervisor-replay-capture",
                &publication_key,
                None,
            )
            .unwrap();
        let objects = BTreeSet::from([snapshot_hash.clone(), tree_hash, policy_hash, file_hash]);
        let blobs = BTreeSet::from([candidate_blob_hash, writer_exclusion_evidence_hash.clone()]);
        stage
            .protect_cas_closure(
                &guard,
                objects.iter().map(String::as_str),
                blobs.iter().map(String::as_str),
            )
            .unwrap();
        let capture = DurableNativeCandidateCapture {
            quiesce_frame_digest: String::new(),
            occurrence_digest: occurrence_digest.clone(),
            durable_stage_id: stage.staging_id().to_owned(),
            snapshot_hash,
            completion_request_digest: completion_request_digest.clone(),
            writer_exclusion_evidence_hash,
        };
        drop(stage);
        drop(guard);

        let launcher = RecordingLauncher {
            capture: Some(capture),
            ..Default::default()
        };
        let mut runtime = SerializedExternalCandidateSupervisor::new(live, launcher);
        let ready = runtime.publish_ready(&supervisor_key).unwrap();
        let ready_ack = signed_owner_frame(
            &binding,
            &owner,
            1,
            None,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: ready.frame().sequence,
                peer_frame_digest: ready.digest().to_owned(),
                application: ryeos_state::external_execution::ExecutionFrameApplication::Applied,
            },
        );
        runtime
            .record_owner_acknowledgement(ready_ack.canonical().as_bytes())
            .unwrap();
        let release = signed_owner_frame(
            &binding,
            &owner,
            2,
            Some(ready_ack.digest().to_owned()),
            ExecutionChannelPayload::Release,
        );
        assert_eq!(
            runtime
                .dispatch_release(release.canonical().as_bytes())
                .unwrap(),
            SupervisorApplicationOutcome::Applied
        );
        let quiesce = signed_owner_frame(
            &binding,
            &owner,
            3,
            Some(release.digest().to_owned()),
            ExecutionChannelPayload::Quiesce {
                completion_request_digest,
            },
        );
        runtime
            .launcher
            .capture
            .as_mut()
            .unwrap()
            .quiesce_frame_digest = quiesce.digest().to_owned();
        let first = runtime
            .dispatch_quiesce(
                quiesce.canonical().as_bytes(),
                &authority,
                &supervisor_key,
                &occurrence_digest,
                &bootstrap_digest,
            )
            .unwrap();
        let repeated = runtime
            .dispatch_quiesce(
                quiesce.canonical().as_bytes(),
                &authority,
                &supervisor_key,
                &occurrence_digest,
                &bootstrap_digest,
            )
            .unwrap();
        assert_eq!(first.receipt.staging_id(), repeated.receipt.staging_id());
        assert_eq!(first.sealed_frame, repeated.sealed_frame);
        let sealed = SignedExecutionFrame::decode_and_verify(
            first.sealed_frame.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let retained_ack = signed_owner_frame(
            &binding,
            &owner,
            4,
            Some(quiesce.digest().to_owned()),
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: sealed.frame().sequence,
                peer_frame_digest: sealed.digest().to_owned(),
                application: ryeos_state::external_execution::ExecutionFrameApplication::Retained,
            },
        );
        assert!(
            runtime
                .record_owner_acknowledgement(retained_ack.canonical().as_bytes())
                .unwrap()
        );
        let (journal, launcher) = runtime.into_parts();
        assert_eq!(launcher.releases, 1);
        assert_eq!(launcher.captures, 1);
        assert_eq!(launcher.finish_acks.len(), 3);
        journal.validate().unwrap();
    }
}
