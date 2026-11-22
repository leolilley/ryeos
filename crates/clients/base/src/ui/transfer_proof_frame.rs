//! Bounded data-only transfer testimony for an unpublished replay candidate.
//! Validation proves consistency, not signatures, membership, event-time
//! composition witnesses, writer admission or a stream completion fence.
//! Outer v1/v2 producer/control/lifecycle fences are deliberately not copied
//! into these records: only the authenticated server stream establishes them.
//! This payload alone cannot establish CAS signatures or chain ancestry.

use ryeos_ui_presentation_contract::{
    transfer::{TransferEvidence, TransferIdentity, TransferPhase, TransferRecord, TransferSeat},
    transfer_proof::TransferTestimony,
    transfer_state::TransferLifecycleState,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferProofAnchor {
    pub presentation_id: String,
    pub chain_root_id: String,
    pub thread_id: String,
    pub event_hash: String,
    /// Native thread-tip ordinal, distinct from its chain position.
    pub thread_seq: u64,
    pub chain_seq: u64,
}

impl UiTransferProofAnchor {
    pub(crate) fn matches(&self, seat: &TransferSeat) -> bool {
        self.presentation_id == seat.presentation_id
            && self.chain_root_id == seat.chain_root_id
            && self.thread_id == seat.thread_id
            && canonical_hash(&self.event_hash)
            && self.thread_seq > 0
            && self.chain_seq > 0
            && self.thread_seq <= self.chain_seq
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferProofPhase {
    pub evidence: TransferEvidence,
    pub record: TransferRecord,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferProofFrame {
    pub schema: u32,
    pub transfer: TransferIdentity,
    pub source_anchor: UiTransferProofAnchor,
    pub destination_anchor: UiTransferProofAnchor,
    /// Canonical per-operation consistency order, never global time order.
    pub phases: Vec<UiTransferProofPhase>,
}

impl UiTransferProofFrame {
    /// Callers must also bound wire bytes before decoding. This typed check
    /// bounds the complete one-operation payload and never truncates testimony.
    pub fn validate(&self, max_bytes: u64) -> Result<TransferLifecycleState, String> {
        if max_bytes == 0
            || serde_json::to_vec(self)
                .map_err(|error| error.to_string())?
                .len() as u64
                > max_bytes
        {
            return Err("transfer proof frame exceeds its exact byte bound".into());
        }
        if self.schema != 1
            || !self.source_anchor.matches(&self.transfer.source)
            || !self.destination_anchor.matches(&self.transfer.destination)
            || self.transfer.source.presentation_id == self.transfer.destination.presentation_id
            || self.transfer.source.chain_root_id == self.transfer.destination.chain_root_id
            || self.transfer.source.thread_id == self.transfer.destination.thread_id
        {
            return Err("transfer proof frame has another schema or exact paired seat".into());
        }
        let mut ranks = BTreeSet::new();
        let mut hashes = BTreeSet::new();
        let mut positions = BTreeSet::new();
        let mut last_rank = None;
        let mut source_position = None;
        let mut destination_position = None;
        let mut lifecycle = None;
        for phase in &self.phases {
            let rank = phase_rank(&phase.record.phase);
            let source = phase.record.event_owner() == &self.transfer.source;
            let (anchor, previous) = if source {
                (&self.source_anchor, &mut source_position)
            } else if phase.record.event_owner() == &self.transfer.destination {
                (&self.destination_anchor, &mut destination_position)
            } else {
                return Err("transfer proof phase has another exact owner".into());
            };
            if phase.record.transfer != self.transfer
                || !canonical_hash(&phase.evidence.event_hash)
                || phase.evidence.chain_seq == 0
                || phase.evidence.chain_seq > anchor.chain_seq
                || phase.evidence.chain_seq == anchor.chain_seq
                    && phase.evidence.event_hash != anchor.event_hash
                || previous.is_some_and(|position| position >= phase.evidence.chain_seq)
                || last_rank.is_some_and(|previous| previous >= rank)
                || !ranks.insert(rank)
                || !hashes.insert(&phase.evidence.event_hash)
                || !positions.insert((source, phase.evidence.chain_seq))
            {
                return Err(
                    "transfer proof phase conflicts with its cut, identity or canonical order"
                        .into(),
                );
            }
            *previous = Some(phase.evidence.chain_seq);
            last_rank = Some(rank);
            let testimony = TransferTestimony {
                evidence: &phase.evidence,
                record: &phase.record,
            };
            if let Some(state) = lifecycle.as_mut() {
                TransferLifecycleState::fold(state, testimony)?;
            } else {
                lifecycle = Some(TransferLifecycleState::begin(testimony)?);
            }
        }
        lifecycle.ok_or("transfer proof frame contains no inbound testimony".into())
    }
}

fn canonical_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn phase_rank(phase: &TransferPhase) -> u8 {
    match phase {
        TransferPhase::InboundCommitment => 0,
        TransferPhase::OrphanCancelled { .. } => 1,
        TransferPhase::Prepared { .. } => 2,
        TransferPhase::Accepted { .. } => 3,
        TransferPhase::Committed { .. } => 4,
        TransferPhase::Aborted { .. } => 5,
        TransferPhase::Activated { .. } => 6,
        TransferPhase::Retained { .. } => 7,
        TransferPhase::Released { .. } => 8,
        TransferPhase::SettlementObserved { .. } => 9,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> UiTransferProofFrame {
        let seat = |id: &str, side: &str| TransferSeat {
            presentation_id: id.into(),
            chain_root_id: format!("root-{side}"),
            thread_id: format!("root-{side}"),
            producer_incarnation: "ab".repeat(32),
            revision: 4,
        };
        let transfer = TransferIdentity {
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            source: seat("11111111-1111-4111-8111-111111111111", "source"),
            destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
            export_digest: "cd".repeat(32),
        };
        let anchor =
            |seat: &TransferSeat, hash: &str, thread_seq, chain_seq| UiTransferProofAnchor {
                presentation_id: seat.presentation_id.clone(),
                chain_root_id: seat.chain_root_id.clone(),
                thread_id: seat.thread_id.clone(),
                event_hash: hash.repeat(32),
                thread_seq,
                chain_seq,
            };
        let inbound = TransferEvidence {
            event_hash: "01".repeat(32),
            chain_seq: 17,
        };
        UiTransferProofFrame {
            schema: 1,
            source_anchor: anchor(&transfer.source, "03", 2, 11),
            destination_anchor: anchor(&transfer.destination, "02", 4, 23),
            phases: vec![
                UiTransferProofPhase {
                    evidence: inbound.clone(),
                    record: TransferRecord {
                        schema: 1,
                        transfer: transfer.clone(),
                        phase: TransferPhase::InboundCommitment,
                    },
                },
                UiTransferProofPhase {
                    evidence: TransferEvidence {
                        event_hash: "02".repeat(32),
                        chain_seq: 23,
                    },
                    record: TransferRecord {
                        schema: 1,
                        transfer: transfer.clone(),
                        phase: TransferPhase::OrphanCancelled { inbound },
                    },
                },
            ],
            transfer,
        }
    }

    #[test]
    fn bounded_frame_preserves_divergent_sequence_domains() {
        let frame = fixture();
        assert!(frame.validate(65536).is_ok());
        assert!(frame.phases[0].evidence.chain_seq > frame.destination_anchor.thread_seq);
        assert!(frame.validate(1).is_err());
        assert!(frame.validate(0).is_err());
        let mut value = serde_json::to_value(frame).unwrap();
        value["checkpoint"] = serde_json::json!({});
        assert!(serde_json::from_value::<UiTransferProofFrame>(value).is_err());
    }

    #[test]
    fn native_proof_anchors_and_phases_cannot_use_genesis_zero() {
        let frame = fixture();
        for source in [true, false] {
            let mut bad = frame.clone();
            let anchor = if source {
                &mut bad.source_anchor
            } else {
                &mut bad.destination_anchor
            };
            anchor.thread_seq = 0;
            assert!(bad.validate(65536).is_err());
            let mut bad = frame.clone();
            let anchor = if source {
                &mut bad.source_anchor
            } else {
                &mut bad.destination_anchor
            };
            anchor.chain_seq = 0;
            assert!(bad.validate(65536).is_err());
        }
        let mut bad = frame;
        bad.phases[0].evidence.chain_seq = 0;
        assert!(bad.validate(65536).is_err());
    }

    #[test]
    fn frame_refuses_conflicting_identity_cut_order_and_links() {
        let frame = fixture();
        let mut bad = frame.clone();
        bad.phases[0].record.transfer.source.revision += 1;
        assert!(bad.validate(65536).is_err());
        let mut bad = frame.clone();
        bad.destination_anchor.thread_id = "another".into();
        assert!(bad.validate(65536).is_err());
        let mut bad = frame.clone();
        bad.phases[1].evidence.chain_seq += 1;
        assert!(bad.validate(65536).is_err());
        let mut bad = frame.clone();
        bad.phases[1].evidence.event_hash = "ff".repeat(32);
        assert!(bad.validate(65536).is_err());
        let mut bad = frame.clone();
        bad.phases.swap(0, 1);
        assert!(bad.validate(65536).is_err());
        let mut bad = frame.clone();
        bad.phases.push(bad.phases[1].clone());
        assert!(bad.validate(65536).is_err());
        let mut bad = frame;
        let TransferPhase::OrphanCancelled { inbound } = &mut bad.phases[1].record.phase else {
            unreachable!()
        };
        inbound.event_hash = "ff".repeat(32);
        assert!(bad.validate(65536).is_err());
    }
}
