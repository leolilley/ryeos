//! Shared authored-transcript rules for the owning node and guest journal.
//!
//! These values project authenticated observations, not applied commands or
//! process authority. They grant no session, allocation, cleanup or replay
//! permission. Storage owners still serialize claims and sticky revocation
//! with actual dispatch, and independently validate retained application state.

use super::{ExecutionChannelPayload, ExecutionFrame};
use anyhow::{Result, bail, ensure};

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
            Release if self == Self::Ready && before_execution_deadline => Self::Running,
            ProtocolBytes { .. } if self == Self::Running && before_execution_deadline => {
                Self::Running
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
            Cancel if !matches!(self, Self::Stopped | Self::Stopping) => Self::Stopping,
            Stopped { .. } if self != Self::Stopped => Self::Stopped,
            Acknowledge { .. } => self,
            _ => bail!(
                "external channel payload contradicts lifecycle state {}",
                self.as_str()
            ),
        })
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
    use crate::external_execution::{ChannelDirection, ExternalStopReason};

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
        assert!(
            stopped
                .advance(None, &ExecutionChannelPayload::Cancel, true)
                .is_err()
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
