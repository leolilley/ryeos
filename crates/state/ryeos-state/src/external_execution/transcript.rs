//! Shared authored-transcript rules for the owning node and guest journal.
//!
//! These values project authenticated observations, not applied commands or
//! process authority. They grant no session, allocation, cleanup or replay
//! permission. Storage owners still serialize claims and sticky revocation
//! with actual dispatch, and independently validate retained application state.

use super::{
    ExecutionChannelPayload, ExecutionFrame, ExternalCommandOutputCommitment,
    ExternalCommandOutputStream, ExternalExecutionMode,
};
use anyhow::{Result, bail, ensure};
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelPhase {
    #[default]
    Prepared,
    Ready,
    Running,
    Quiescing,
    Exported,
    Stopping,
    Stopped,
}

impl ChannelPhase {
    /// Exact current journal spelling; no predecessor coercion.
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "prepared" => Self::Prepared,
            "ready" => Self::Ready,
            "running" => Self::Running,
            "quiescing" => Self::Quiescing,
            "exported" => Self::Exported,
            "stopping" => Self::Stopping,
            "stopped" => Self::Stopped,
            _ => bail!("unknown external channel phase"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Quiescing => "quiescing",
            Self::Exported => "exported",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }

    /// Quiescence may already be authored while prior input still drains.
    /// This is only a projection check, never a dispatch permit.
    pub fn permits_pending_input(self) -> bool {
        matches!(self, Self::Running | Self::Quiescing | Self::Exported)
    }

    pub fn advance(
        self,
        completion: Option<&str>,
        payload: &ExecutionChannelPayload,
        before_execution_deadline: bool,
    ) -> Result<Self> {
        use ExecutionChannelPayload::*;
        Ok(match payload {
            Ready { .. } if self == Self::Prepared && before_execution_deadline => Self::Ready,
            // Cancellation can cross the first authenticated Ready in flight.
            // Retain that observation to preserve the supervisor's signed
            // predecessor chain, without restoring readiness/Release authority.
            // The wire contract permits Ready only at supervisor sequence one.
            Ready { .. } if matches!(self, Self::Stopping | Self::Stopped) => self,
            Release if self == Self::Ready && before_execution_deadline => Self::Running,
            // This signed projection follows the native guest row. It may be
            // retained after a racing cancellation, but never restores input
            // or success authority once the channel is stopping.
            RuntimeApplied { .. } if matches!(self, Self::Running | Self::Stopping) => self,
            ProtocolBytes { .. } if self == Self::Running && before_execution_deadline => {
                Self::Running
            }
            // These are bounded observations, not executable input. They may
            // drain after cancellation/deadline until channel expiry.
            CommandOutput { .. } if matches!(self, Self::Running | Self::Stopping) => self,
            // A target exit is not allocation cleanup or descendant-death proof.
            CommandTerminated { .. } if matches!(self, Self::Running | Self::Stopping) => {
                Self::Stopping
            }
            ProtocolEof if matches!(self, Self::Running | Self::Quiescing) => self,
            Quiesce { .. } if self == Self::Running => Self::Quiescing,
            ExportObjectChunk { .. } if self == Self::Quiescing => Self::Quiescing,
            ExportSealed {
                completion_request_digest,
                ..
            } if self == Self::Quiescing
                && completion == Some(completion_request_digest.as_str()) =>
            {
                Self::Exported
            }
            // Target termination is not cancellation. A first legitimate Cancel
            // may arrive after it; exact sticky-frame and sequence checks, not
            // this projection, reject a distinct second cancellation.
            Cancel if self == Self::Stopped => Self::Stopped,
            Cancel => Self::Stopping,
            Stopped { .. } if self != Self::Stopped => Self::Stopped,
            Acknowledge { .. } => self,
            _ => bail!(
                "external channel payload contradicts lifecycle state {}",
                self.as_str()
            ),
        })
    }
}

/// Validate the next retained stream chunk without retaining all prior bytes.
/// Authentication/direction belong to the frame owner; offsets and bounds must
/// be checked against the journal, never a sender-provided resume coordinate.
pub(crate) fn command_output_bytes(
    mode: ExternalExecutionMode,
    stream: ExternalCommandOutputStream,
    offset: u64,
    expected_offset: u64,
    bytes_base64: &str,
) -> Result<Vec<u8>> {
    let limit = mode.output_limit(stream)?;
    ensure!(
        offset == expected_offset,
        "external command output has a gap or overlap"
    );
    let bytes = super::chunk(bytes_base64, false)?;
    ensure!(
        offset
            .checked_add(bytes.len() as u64)
            .is_some_and(|end| end <= limit),
        "external command output exceeds admitted stream bounds"
    );
    Ok(bytes)
}

#[derive(Default)]
struct CommandStreamTranscript {
    bytes: u64,
    digest: Sha256,
}

impl CommandStreamTranscript {
    fn require_commitment(&self, commitment: &ExternalCommandOutputCommitment) -> Result<()> {
        ensure!(
            self.bytes == commitment.bytes
                && format!("{:x}", self.digest.clone().finalize()) == commitment.sha256,
            "external command termination contradicts retained output"
        );
        Ok(())
    }
}

/// One-pass projection of authenticated output for terminal verification and
/// recovery. This carries no command, candidate, evaluation or cleanup authority.
/// Raw target termination stays truthful after cancel; success eligibility is
/// checked independently against sticky revocation by the journal owner.
#[derive(Default)]
pub(crate) struct DirectCommandTranscript {
    stdout: CommandStreamTranscript,
    stderr: CommandStreamTranscript,
    terminated: bool,
}

impl DirectCommandTranscript {
    pub(crate) fn observe(
        &mut self,
        mode: ExternalExecutionMode,
        payload: &ExecutionChannelPayload,
    ) -> Result<()> {
        match payload {
            ExecutionChannelPayload::CommandOutput {
                stream,
                offset,
                bytes_base64,
            } => {
                ensure!(
                    !self.terminated,
                    "external command output follows termination"
                );
                let retained = match stream {
                    ExternalCommandOutputStream::Stdout => &mut self.stdout,
                    ExternalCommandOutputStream::Stderr => &mut self.stderr,
                };
                let bytes =
                    command_output_bytes(mode, *stream, *offset, retained.bytes, bytes_base64)?;
                retained.bytes += bytes.len() as u64;
                retained.digest.update(&bytes);
            }
            ExecutionChannelPayload::CommandTerminated { observation } => {
                ensure!(
                    !self.terminated,
                    "external command has more than one termination"
                );
                observation.validate(mode)?;
                self.stdout.require_commitment(&observation.stdout)?;
                self.stderr.require_commitment(&observation.stderr)?;
                self.terminated = true;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Last retained directional frame, not a peer-supplied resume coordinate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameFrontier {
    sequence: u64,
    digest: Option<String>,
    acknowledged_peer_sequence: u64,
}

impl FrameFrontier {
    pub fn from_retained(sequence: u64, digest: Option<String>, ack: u64) -> Result<Self> {
        ensure!(
            (sequence == 0 && digest.is_none() && ack == 0) || (sequence > 0 && digest.is_some()),
            "external frontier has no exact predecessor"
        );
        if let Some(digest) = &digest {
            super::hash(digest)?;
        }
        Ok(Self {
            sequence,
            digest,
            acknowledged_peer_sequence: ack,
        })
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Check ordering before changing any projection or claiming application.
    /// Exact duplicates are handled by the journal, not executable successors.
    pub fn require_successor(&self, frame: &ExecutionFrame, peer_sequence: u64) -> Result<()> {
        ensure!(
            self.sequence.checked_add(1) == Some(frame.sequence)
                && frame.previous_frame_digest == self.digest
                && frame.acknowledged_peer_sequence >= self.acknowledged_peer_sequence,
            "external transcript has a gap, fork or regressed acknowledgement"
        );
        ensure!(
            frame.acknowledged_peer_sequence <= peer_sequence,
            "external acknowledgement refers to an unauthored peer frame"
        );
        Ok(())
    }
}

pub fn urgent_control(payload: &ExecutionChannelPayload) -> bool {
    matches!(
        payload,
        ExecutionChannelPayload::Cancel
            | ExecutionChannelPayload::Stopped { .. }
            | ExecutionChannelPayload::Acknowledge { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_execution::{
        ChannelDirection, ExternalCommandTermination, ExternalCommandTerminationReason,
        ExternalStopReason, ExternalTargetExit,
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    fn direct_mode() -> ExternalExecutionMode {
        ExternalExecutionMode::DirectCommand {
            stdout_max_bytes: 8,
            stderr_max_bytes: 8,
        }
    }

    fn output(
        stream: ExternalCommandOutputStream,
        offset: u64,
        bytes: &[u8],
    ) -> ExecutionChannelPayload {
        ExecutionChannelPayload::CommandOutput {
            stream,
            offset,
            bytes_base64: STANDARD.encode(bytes),
        }
    }

    fn terminal(stdout: &[u8], stderr: &[u8]) -> ExecutionChannelPayload {
        let commitment = |bytes: &[u8]| ExternalCommandOutputCommitment {
            bytes: bytes.len() as u64,
            sha256: lillux::sha256_hex(bytes),
            truncated: false,
        };
        ExecutionChannelPayload::CommandTerminated {
            observation: ExternalCommandTermination {
                target_exit: ExternalTargetExit::Code(0),
                reason: ExternalCommandTerminationReason::TargetExited,
                stdout: commitment(stdout),
                stderr: commitment(stderr),
            },
        }
    }

    #[test]
    fn command_transcript_checks_each_stream_and_terminal_digest_once() {
        let mut transcript = DirectCommandTranscript::default();
        for payload in [
            output(ExternalCommandOutputStream::Stdout, 0, b"abc"),
            output(ExternalCommandOutputStream::Stderr, 0, b"err"),
            output(ExternalCommandOutputStream::Stdout, 3, b"def"),
        ] {
            transcript.observe(direct_mode(), &payload).unwrap();
        }
        assert!(
            transcript
                .observe(direct_mode(), &terminal(b"abcdef", b"wrong"))
                .is_err()
        );
        assert!(
            transcript
                .observe(direct_mode(), &terminal(b"abcde", b"err"))
                .is_err()
        );
        transcript
            .observe(direct_mode(), &terminal(b"abcdef", b"err"))
            .unwrap();
        assert!(
            transcript
                .observe(direct_mode(), &terminal(b"abcdef", b"err"))
                .is_err()
        );
        assert!(
            transcript
                .observe(
                    direct_mode(),
                    &output(ExternalCommandOutputStream::Stdout, 6, b"x")
                )
                .is_err()
        );
    }

    #[test]
    fn command_transcript_refuses_gaps_overlap_malformed_and_cumulative_excess() {
        let mut transcript = DirectCommandTranscript::default();
        transcript
            .observe(
                direct_mode(),
                &output(ExternalCommandOutputStream::Stdout, 0, b"abc"),
            )
            .unwrap();
        for payload in [
            output(ExternalCommandOutputStream::Stdout, 0, b"x"),
            output(ExternalCommandOutputStream::Stdout, 4, b"x"),
            output(ExternalCommandOutputStream::Stdout, 3, b"123456"),
            output(ExternalCommandOutputStream::Stdout, 3, b""),
            ExecutionChannelPayload::CommandOutput {
                stream: ExternalCommandOutputStream::Stdout,
                offset: 3,
                bytes_base64: "not-base64!".into(),
            },
        ] {
            assert!(transcript.observe(direct_mode(), &payload).is_err());
        }
        transcript
            .observe(direct_mode(), &terminal(b"abc", b""))
            .unwrap();
        assert!(
            DirectCommandTranscript::default()
                .observe(
                    ExternalExecutionMode::StructuredSession {},
                    &output(ExternalCommandOutputStream::Stdout, 0, b"x"),
                )
                .is_err()
        );
    }

    #[test]
    fn command_termination_drains_but_never_projects_cleanup() {
        for phase in [ChannelPhase::Running, ChannelPhase::Stopping] {
            assert_eq!(
                phase
                    .advance(
                        None,
                        &output(ExternalCommandOutputStream::Stdout, 0, b"x"),
                        false
                    )
                    .unwrap(),
                phase
            );
            assert_eq!(
                phase.advance(None, &terminal(b"x", b""), false).unwrap(),
                ChannelPhase::Stopping
            );
        }
        assert!(
            ChannelPhase::Stopped
                .advance(None, &terminal(b"", b""), true)
                .is_err()
        );
        assert!(
            ChannelPhase::Ready
                .advance(
                    None,
                    &output(ExternalCommandOutputStream::Stdout, 0, b"x"),
                    true
                )
                .is_err()
        );
        assert!(!urgent_control(&terminal(b"", b"")));
        assert!(!urgent_control(&output(
            ExternalCommandOutputStream::Stdout,
            0,
            b"x"
        )));
    }

    #[test]
    fn phase_projection_preserves_drain_and_terminal_rules() {
        let completion = "a".repeat(64);
        let quiesce = ExecutionChannelPayload::Quiesce {
            completion_request_digest: completion.clone(),
        };
        let phase = ChannelPhase::Running
            .advance(None, &quiesce, false)
            .unwrap();
        assert_eq!(phase, ChannelPhase::Quiescing);
        assert_eq!(
            ChannelPhase::Running
                .advance(None, &ExecutionChannelPayload::ProtocolEof, false)
                .unwrap(),
            ChannelPhase::Running
        );
        assert_eq!(
            phase
                .advance(None, &ExecutionChannelPayload::ProtocolEof, false)
                .unwrap(),
            ChannelPhase::Quiescing
        );
        assert!(
            ChannelPhase::Ready
                .advance(None, &ExecutionChannelPayload::ProtocolEof, false)
                .is_err()
        );
        assert!(phase.permits_pending_input());
        let seal = ExecutionChannelPayload::ExportSealed {
            candidate_snapshot_hash: "b".repeat(64),
            candidate_output_capture_hash: None,
            completion_request_digest: completion.clone(),
            writer_exclusion_evidence_hash: "c".repeat(64),
        };
        assert!(phase.advance(None, &seal, false).is_err());
        assert_eq!(
            phase.advance(Some(&completion), &seal, false).unwrap(),
            ChannelPhase::Exported
        );
        let stopped = phase
            .advance(Some(&completion), &ExecutionChannelPayload::Cancel, false)
            .unwrap();
        assert!(!stopped.permits_pending_input());
        assert!(
            stopped
                .advance(None, &ExecutionChannelPayload::Release, true)
                .is_err()
        );
        assert_eq!(
            stopped
                .advance(None, &ExecutionChannelPayload::Cancel, true)
                .unwrap(),
            ChannelPhase::Stopping
        );
        assert_eq!(
            stopped
                .advance(
                    None,
                    &ExecutionChannelPayload::Stopped {
                        reason: ExternalStopReason::Cancelled
                    },
                    false
                )
                .unwrap(),
            ChannelPhase::Stopped
        );
    }

    #[test]
    fn phase_spellings_and_deadlines_are_exact() {
        for name in [
            "prepared",
            "ready",
            "running",
            "quiescing",
            "exported",
            "stopping",
            "stopped",
        ] {
            let phase = ChannelPhase::parse(name).unwrap();
            assert_eq!(phase.as_str(), name);
            assert_eq!(
                phase
                    .advance(
                        None,
                        &ExecutionChannelPayload::Acknowledge {
                            peer_frame_sequence: 1,
                            peer_frame_digest: "a".repeat(64),
                            application: super::super::ExecutionFrameApplication::Retained,
                        },
                        false,
                    )
                    .unwrap(),
                phase
            );
        }
        for name in ["", "RUNNING", "running ", "completed"] {
            assert!(ChannelPhase::parse(name).is_err());
        }
        assert!(
            ChannelPhase::Ready
                .advance(None, &ExecutionChannelPayload::Release, false)
                .is_err()
        );
        assert!(
            ChannelPhase::Prepared
                .advance(None, &ExecutionChannelPayload::Release, true)
                .is_err()
        );
    }

    #[test]
    fn frontier_refuses_gaps_forks_ack_regression_and_overflow() {
        let digest = "a".repeat(64);
        let frontier = FrameFrontier::from_retained(2, Some(digest.clone()), 1).unwrap();
        let mut frame = ExecutionFrame {
            schema: 1,
            binding_digest: "b".repeat(64),
            direction: ChannelDirection::OwnerToSupervisor,
            sequence: 3,
            previous_frame_digest: Some(digest.clone()),
            acknowledged_peer_sequence: 1,
            payload: ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 1,
                peer_frame_digest: "c".repeat(64),
                application: super::super::ExecutionFrameApplication::Retained,
            },
        };
        frontier.require_successor(&frame, 1).unwrap();
        frame.sequence = 2;
        assert!(frontier.require_successor(&frame, 1).is_err());
        frame.sequence = 4;
        assert!(frontier.require_successor(&frame, 1).is_err());
        frame.sequence = 3;
        frame.previous_frame_digest = None;
        assert!(frontier.require_successor(&frame, 1).is_err());
        frame.previous_frame_digest = Some(digest.clone());
        frame.acknowledged_peer_sequence = 0;
        assert!(frontier.require_successor(&frame, 1).is_err());
        frame.acknowledged_peer_sequence = 2;
        assert!(frontier.require_successor(&frame, 1).is_err());
        assert!(FrameFrontier::from_retained(0, Some(digest.clone()), 0).is_err());
        assert!(FrameFrontier::from_retained(1, None, 0).is_err());
        assert!(FrameFrontier::from_retained(0, None, 1).is_err());
        let exhausted = FrameFrontier::from_retained(u64::MAX, Some(digest), 0).unwrap();
        frame.sequence = 0;
        assert!(exhausted.require_successor(&frame, 2).is_err());
    }
}
