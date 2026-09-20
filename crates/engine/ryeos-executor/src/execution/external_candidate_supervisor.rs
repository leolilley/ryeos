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

    pub fn publish_ready(
        &self,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<AuthenticatedExecutionFrame> {
        let binding = self.journal.binding();
        let ready = self.journal.author_supervisor_frame(
            signing_key,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        )?;
        let GuestApplicationClaim::New(token) = self.journal.claim(
            ChannelDirection::SupervisorToOwner,
            ready.frame().sequence,
            ready.digest(),
        )?
        else {
            anyhow::bail!("fresh supervisor readiness was not claimable")
        };
        let (_, performed) = self.journal.apply_once(token, |_| Ok(()))?;
        self.journal.finish(performed)?;
        Ok(ready)
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
        match self
            .journal
            .record_and_claim(sealed.canonical().as_bytes())?
        {
            GuestApplicationClaim::New(token) => {
                let (_, performed) = self.journal.apply_once(token, |_| Ok(()))?;
                self.journal.finish(performed)?;
            }
            GuestApplicationClaim::AlreadyClaimed => {
                self.journal
                    .reconcile_retained_export_application(&sealed)?;
            }
            GuestApplicationClaim::AlreadyApplied => {}
            GuestApplicationClaim::Revoked => {
                anyhow::bail!("sealed external candidate was durably revoked")
            }
        }
        Ok(SupervisorCaptureOutcome {
            receipt,
            sealed_frame: sealed.canonical().to_owned(),
        })
    }

    pub fn into_parts(self) -> (LiveGuestJournal, L) {
        (self.journal, self.launcher)
    }
}
