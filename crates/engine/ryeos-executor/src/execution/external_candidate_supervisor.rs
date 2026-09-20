//! Serialized protected-supervisor application loop.
//!
//! The production launcher client is an authenticated inherited duplex peer;
//! this owner never contains `NativeExternalCandidate` itself. Every method
//! crosses the guest journal's SQLite writer gate around exactly one bounded
//! launcher request and waits for the exact durable-finish acknowledgement
//! before permitting a later action.

use anyhow::{Result, ensure};
use ryeos_state::external_execution::AuthenticatedExecutionFrame;
use ryeos_state::external_execution::guest_journal::{
    GuestApplicationClaim, GuestApplicationToken, GuestProtocolApplication,
    GuestTerminalApplicationClaim, LiveGuestJournal, RevocationAppendOutcome,
};

/// Narrow live-launcher capability. Implementations must represent one exact
/// already-bound occurrence; reconnecting or recreating a launcher is not an
/// implementation of this trait.
pub trait ExternalCandidateLauncherClient {
    fn release(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()>;
    fn apply_protocol_chunk(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<usize>;
    fn cancel(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()>;
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

    pub fn into_parts(self) -> (LiveGuestJournal, L) {
        (self.journal, self.launcher)
    }
}
