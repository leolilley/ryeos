//! Bounded, data-only negative MOVE-freeze testimony. The server may construct
//! this only from completed paired absence and receipted source witnesses.
//! Validation here checks shape and correlation, never signatures or authority.

use ryeos_ui_presentation_contract::{
    move_freeze_lifecycle::MoveFreezeCancellation,
    transfer::{TransferEvidence, TransferIdentity, TransferPhase},
};
use serde::{Deserialize, Serialize};

use super::{
    seat::{SeatEvent, SeatEventKind},
    transfer_proof_frame::{UiTransferProofAnchor, UiTransferProofPhase},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiReceiptedFreezeWitness {
    pub evidence: TransferEvidence,
    pub event: SeatEvent,
    pub receipt_evidence: TransferEvidence,
    pub receipt_operation_id: String,
    pub receipt_producer_incarnation: String,
    pub receipt_first_engine_seq: u64,
    pub receipt_last_engine_seq: u64,
}

impl UiReceiptedFreezeWitness {
    fn validate(
        &self,
        source: &UiTransferProofAnchor,
        cancellation_seq: u64,
    ) -> Result<(), String> {
        let engine_seq = self
            .event
            .producer_engine_seq
            .ok_or("freeze witness has no producer position")?;
        if !canonical_hash(&self.evidence.event_hash)
            || !canonical_hash(&self.receipt_evidence.event_hash)
            || self.event.seq != self.evidence.chain_seq
            || self.evidence.chain_seq == 0
            || self.evidence.chain_seq >= self.receipt_evidence.chain_seq
            || self.receipt_evidence.chain_seq >= cancellation_seq
            || cancellation_seq > source.chain_seq
            || self.receipt_operation_id.is_empty()
            || !canonical_hash(&self.receipt_producer_incarnation)
            || self.receipt_first_engine_seq > engine_seq
            || engine_seq > self.receipt_last_engine_seq
        {
            return Err("freeze witness differs from its receipted source interval".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiMoveFreezeCancellationProofFrame {
    pub schema: u32,
    pub transfer: TransferIdentity,
    pub source_anchor: UiTransferProofAnchor,
    pub destination_anchor: UiTransferProofAnchor,
    pub cancellation_evidence: TransferEvidence,
    pub cancellation: MoveFreezeCancellation,
    /// The actual destination InboundCommitment, if retained. Required null
    /// is paired absence at these cuts, not a synthetic lifecycle phase.
    #[serde(deserialize_with = "required_inbound")]
    pub destination_inbound: Option<UiTransferProofPhase>,
    /// Required null unless the exact completed destination cut also retains
    /// its Inbound-linked OrphanCancelled phase. This never proves source
    /// absence by itself; the paired app proof owns that conclusion.
    #[serde(deserialize_with = "required_inbound")]
    pub destination_orphan_cancelled: Option<UiTransferProofPhase>,
    pub preparation: UiReceiptedFreezeWitness,
    /// Required null represents completed paired absence of a completion.
    #[serde(deserialize_with = "required_completion")]
    pub completion: Option<UiReceiptedFreezeWitness>,
}

fn required_completion<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<UiReceiptedFreezeWitness>, D::Error> {
    Option::<UiReceiptedFreezeWitness>::deserialize(deserializer)
}

fn required_inbound<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<UiTransferProofPhase>, D::Error> {
    Option::<UiTransferProofPhase>::deserialize(deserializer)
}

impl UiMoveFreezeCancellationProofFrame {
    /// Data-only consistency; the server stream owner must separately prove
    /// the same-store complete pair, current memberships and both signed tips.
    pub fn validate(&self, max_bytes: u64) -> Result<(), String> {
        if max_bytes == 0
            || serde_json::to_vec(self)
                .map_err(|error| error.to_string())?
                .len() as u64
                > max_bytes
        {
            return Err("cancellation proof frame exceeds its byte bound".into());
        }
        self.cancellation.validate_shape()?;
        if self.schema != 1
            || self.transfer != self.cancellation.transfer
            || !self.source_anchor.matches(&self.transfer.source)
            || !self.destination_anchor.matches(&self.transfer.destination)
            || self.transfer.source.presentation_id == self.transfer.destination.presentation_id
            || self.transfer.source.chain_root_id == self.transfer.destination.chain_root_id
            || !canonical_hash(&self.cancellation_evidence.event_hash)
            || self.cancellation_evidence.chain_seq == 0
            || self.cancellation_evidence.chain_seq > self.source_anchor.chain_seq
            || (self.cancellation_evidence.chain_seq == self.source_anchor.chain_seq
                && self.cancellation_evidence.event_hash != self.source_anchor.event_hash)
            || self.preparation.evidence != self.cancellation.freeze.preparation
            || self.completion.as_ref().map(|witness| &witness.evidence)
                != self.cancellation.freeze.completion.as_ref()
        {
            return Err("cancellation frame differs from its exact pair or nullable freeze".into());
        }
        if let Some(inbound) = &self.destination_inbound
            && (inbound.record.transfer != self.transfer
                || inbound.record.event_owner() != &self.transfer.destination
                || !matches!(inbound.record.phase, TransferPhase::InboundCommitment)
                || inbound.record.validate_shape().is_err()
                || !canonical_hash(&inbound.evidence.event_hash)
                || inbound.evidence.chain_seq == 0
                || inbound.evidence.chain_seq > self.destination_anchor.chain_seq
                || (inbound.evidence.chain_seq == self.destination_anchor.chain_seq
                    && inbound.evidence.event_hash != self.destination_anchor.event_hash)
                || inbound.evidence.event_hash == self.cancellation_evidence.event_hash)
        {
            return Err("cancellation frame differs from exact destination inbound".into());
        }
        if let Some(orphan) = &self.destination_orphan_cancelled {
            let inbound = self
                .destination_inbound
                .as_ref()
                .ok_or("orphan cancellation has no exact destination inbound")?;
            if orphan.record.transfer != self.transfer
                || orphan.record.event_owner() != &self.transfer.destination
                || !matches!(&orphan.record.phase,
                    TransferPhase::OrphanCancelled { inbound: link } if link == &inbound.evidence)
                || orphan.record.validate_shape().is_err()
                || !canonical_hash(&orphan.evidence.event_hash)
                || orphan.evidence.chain_seq <= inbound.evidence.chain_seq
                || orphan.evidence.chain_seq > self.destination_anchor.chain_seq
                || (orphan.evidence.chain_seq == self.destination_anchor.chain_seq
                    && orphan.evidence.event_hash != self.destination_anchor.event_hash)
                || orphan.evidence.event_hash == self.cancellation_evidence.event_hash
            {
                return Err("cancellation frame differs from exact destination orphan".into());
            }
        }
        self.preparation
            .validate(&self.source_anchor, self.cancellation_evidence.chain_seq)?;
        let SeatEventKind::PresentationOperationPrepared(prepared) = &self.preparation.event.kind
        else {
            return Err("cancellation preparation has another event kind".into());
        };
        prepared.validate().map_err(str::to_owned)?;
        let freeze = prepared
            .move_freeze()
            .ok_or("cancellation preparation is not a MOVE freeze")?;
        if freeze.transfer != self.transfer
            || self.preparation.receipt_producer_incarnation
                != prepared.producer.producer_incarnation
        {
            return Err("cancellation preparation has another transfer identity".into());
        }
        if let Some(completion) = &self.completion {
            completion.validate(&self.source_anchor, self.cancellation_evidence.chain_seq)?;
            let SeatEventKind::PresentationOperationCompleted(completed) = &completion.event.kind
            else {
                return Err("cancellation completion has another event kind".into());
            };
            completed.validate_for(prepared).map_err(str::to_owned)?;
            if completion.evidence.chain_seq <= self.preparation.evidence.chain_seq {
                return Err("cancellation completion precedes its exact preparation".into());
            }
        }
        Ok(())
    }
}

fn canonical_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct RequiredNullable {
        #[serde(deserialize_with = "required_completion")]
        completion: Option<UiReceiptedFreezeWitness>,
    }

    #[derive(Debug, Deserialize)]
    struct RequiredInbound {
        #[serde(deserialize_with = "required_inbound")]
        destination_inbound: Option<UiTransferProofPhase>,
    }

    #[test]
    fn negative_proof_requires_explicit_nullable_completion() {
        assert!(serde_json::from_value::<RequiredNullable>(serde_json::json!({})).is_err());
        assert!(
            serde_json::from_value::<RequiredNullable>(serde_json::json!({"completion":null}))
                .unwrap()
                .completion
                .is_none()
        );
        assert!(
            serde_json::from_value::<RequiredNullable>(serde_json::json!({"completion":{}}))
                .is_err()
        );
        assert!(serde_json::from_value::<RequiredInbound>(serde_json::json!({})).is_err());
        assert!(
            serde_json::from_value::<RequiredInbound>(
                serde_json::json!({"destination_inbound":null})
            )
            .unwrap()
            .destination_inbound
            .is_none()
        );
    }
}
