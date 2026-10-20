//! Data-only lifecycle settlement with disjoint Closed and lease-reaping fences.
//!
//! A signed event is node-lifecycle testimony, not proof that the lease claim
//! was admissible. The writer must authenticate the durable claim under the
//! same publication locks. This is the only lifecycle settlement contract:
//! earlier event versions are not decoded.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    payload_digest::seat_payload_digest,
    seat_control::{UiSeatControlRequest, UiSeatProducer, UiSeatProducerState},
    transfer::{TransferEvidence, TransferPhase, TransferRecord, TransferSeat},
};

pub const TRANSFER_LIFECYCLE_EVENT: &str = "contract.ryeos_ui.presentation_transfer_lifecycle.v3";
pub const TRANSFER_LIFECYCLE_SCHEMA: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferLifecycleWriter {
    NodeLifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransferLifecycleFence {
    /// The seat's newest control is its browser Close.
    ClosedControl { latest_control: TransferEvidence },
    /// The seat's newest control is not a Close: its lease expired.
    LeaseReaping { latest_control: TransferEvidence },
}

impl TransferLifecycleFence {
    pub fn latest_control(&self) -> &TransferEvidence {
        match self {
            Self::ClosedControl { latest_control, .. }
            | Self::LeaseReaping { latest_control, .. } => latest_control,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferLifecycleEventPayload {
    pub schema: u32,
    pub writer: TransferLifecycleWriter,
    pub operation_id: String,
    pub settlement_id: String,
    pub owning_seat: TransferSeat,
    pub fence: TransferLifecycleFence,
    pub payload: TransferRecord,
}

impl TransferLifecycleEventPayload {
    pub fn new(payload: TransferRecord, fence: TransferLifecycleFence) -> Result<Self, String> {
        let mut event = Self {
            schema: TRANSFER_LIFECYCLE_SCHEMA,
            writer: TransferLifecycleWriter::NodeLifecycle,
            operation_id: payload.transfer.operation_id.clone(),
            settlement_id: String::new(),
            owning_seat: payload.event_owner().clone(),
            fence,
            payload,
        };
        event.settlement_id = event.derive_settlement_id()?;
        event.validate_shape()?;
        Ok(event)
    }

    pub fn validate_shape(&self) -> Result<(), String> {
        self.payload.validate_shape()?;
        let control = self.fence.latest_control();
        if self.schema != TRANSFER_LIFECYCLE_SCHEMA
            || self.operation_id != self.payload.transfer.operation_id
            || self.owning_seat != *self.payload.event_owner()
            || !matches!(
                self.payload.phase,
                TransferPhase::Aborted { .. }
                    | TransferPhase::OrphanCancelled { .. }
                    | TransferPhase::Released { .. }
                    | TransferPhase::Retained { .. }
                    | TransferPhase::SettlementObserved { .. }
            )
            || control.chain_seq == 0
            || !canonical_hash(&control.event_hash)
            || self.settlement_id != self.derive_settlement_id()?
        {
            return Err("lifecycle settlement identity, seat or fence is invalid".into());
        }
        Ok(())
    }

    pub fn evidence_links(&self) -> Vec<&TransferEvidence> {
        let mut links = self.payload.evidence_links();
        links.push(self.fence.latest_control());
        links
    }

    pub fn validate_bounds(&self, max_payload_bytes: u64) -> Result<(), String> {
        self.validate_shape()?;
        if max_payload_bytes == 0
            || serde_json::to_vec(self)
                .map_err(|error| error.to_string())?
                .len() as u64
                > max_payload_bytes
        {
            return Err("lifecycle settlement exceeds the existing seat payload bound".into());
        }
        Ok(())
    }

    fn derive_settlement_id(&self) -> Result<String, String> {
        seat_payload_digest(&serde_json::json!({
            "domain": TRANSFER_LIFECYCLE_EVENT,
            "schema": self.schema,
            "writer": self.writer,
            "owning_seat": self.owning_seat,
            "fence": self.fence,
            "payload": self.payload,
        }))
        .map_err(|error| error.to_string())
    }
}

fn canonical_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn from_event_value(event: &Value) -> Result<TransferLifecycleEventPayload, String> {
    if event.get("event_type").and_then(Value::as_str) != Some(TRANSFER_LIFECYCLE_EVENT) {
        return Err("not a lifecycle transfer settlement event".into());
    }
    let payload: TransferLifecycleEventPayload = serde_json::from_value(
        event
            .get("payload")
            .cloned()
            .ok_or("lifecycle event omitted payload")?,
    )
    .map_err(|error| format!("invalid lifecycle transfer settlement: {error}"))?;
    payload.validate_shape()?;
    if event.get("thread_id").and_then(Value::as_str)
        != Some(payload.owning_seat.thread_id.as_str())
        || event.get("chain_root_id").and_then(Value::as_str)
            != Some(payload.owning_seat.chain_root_id.as_str())
    {
        return Err("lifecycle settlement is not on its exact owning seat".into());
    }
    Ok(payload)
}

/// Pure consistency with an authenticated latest control. Lease expiry and
/// runtime-owner exclusion are deliberately outside this function.
pub fn verify_latest_control(
    settlement: &TransferLifecycleEventPayload,
    evidence: &TransferEvidence,
    control: &UiSeatProducer,
) -> Result<(), String> {
    settlement.validate_shape()?;
    verify_control_fence(
        &settlement.owning_seat,
        &settlement.fence,
        evidence,
        control,
    )
}

/// Authenticate retained control semantics only. This does not establish the
/// durable runtime reaping claim or publication exclusion.
pub fn verify_control_fence(
    owning_seat: &TransferSeat,
    fence: &TransferLifecycleFence,
    evidence: &TransferEvidence,
    control: &UiSeatProducer,
) -> Result<(), String> {
    control.validate().map_err(str::to_owned)?;
    control
        .validate_subject(&owning_seat.chain_root_id, &owning_seat.thread_id)
        .map_err(str::to_owned)?;
    if fence.latest_control() != evidence {
        return Err("settlement references another producer control".into());
    }
    let presentation_id = match &control.request {
        UiSeatControlRequest::Claim {
            presentation_id, ..
        }
        | UiSeatControlRequest::Relinquish {
            presentation_id, ..
        }
        | UiSeatControlRequest::Close {
            presentation_id, ..
        } => presentation_id,
    };
    if presentation_id != &owning_seat.presentation_id {
        return Err("settlement control belongs to another presentation".into());
    }
    match fence {
        TransferLifecycleFence::ClosedControl { .. }
            if control.state == UiSeatProducerState::Closed
                && matches!(&control.request, UiSeatControlRequest::Close { .. }) =>
        {
            Ok(())
        }
        TransferLifecycleFence::LeaseReaping { .. }
            if control.state != UiSeatProducerState::Closed =>
        {
            Ok(())
        }
        _ => Err("settlement fence differs from latest control semantics".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        seat_control::{SEAT_PRODUCER_SCHEMA, UiSeatControlPredecessor, UiSeatProducerClaim},
        transfer::TransferIdentity,
    };
    use serde_json::json;

    fn fixture() -> (TransferRecord, TransferEvidence, UiSeatProducer) {
        let seat = |presentation_id: &str, suffix: &str| TransferSeat {
            presentation_id: presentation_id.into(),
            thread_id: format!("thread-{suffix}"),
            chain_root_id: format!("chain-{suffix}"),
            producer_incarnation: "a".repeat(64),
            revision: 1,
        };
        let source = seat("11111111-1111-4111-8111-111111111111", "source");
        let destination = seat("22222222-2222-4222-8222-222222222222", "destination");
        let record = TransferRecord {
            schema: 1,
            transfer: TransferIdentity {
                operation_id: "33333333-3333-4333-8333-333333333333".into(),
                source: source.clone(),
                destination,
                export_digest: "c".repeat(64),
            },
            phase: TransferPhase::Aborted {
                preparation: TransferEvidence {
                    event_hash: "d".repeat(64),
                    chain_seq: 2,
                },
            },
        };
        let evidence = TransferEvidence {
            event_hash: "e".repeat(64),
            chain_seq: 5,
        };
        let mut request = UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: source.presentation_id,
            thread_id: source.thread_id,
            chain_root_id: source.chain_root_id,
            predecessor: UiSeatControlPredecessor::Initialization {
                event_hash: "f".repeat(64),
            },
            intent_nonce: "44444444-4444-4444-8444-444444444444".into(),
            claim: UiSeatProducerClaim::Fresh,
        };
        request.assign_identity().unwrap();
        let control = UiSeatProducer {
            schema_version: SEAT_PRODUCER_SCHEMA.into(),
            operation_id: request.operation_id().into(),
            request,
            claim: Some(UiSeatProducerClaim::Fresh),
            predecessor: None,
            producer_incarnation: "a".repeat(64),
            state: UiSeatProducerState::Active,
            next_engine_seq: 0,
        };
        (record, evidence, control)
    }

    #[test]
    fn reaping_variant_commits_exact_owner_and_control() {
        let (record, evidence, control) = fixture();
        let event = TransferLifecycleEventPayload::new(
            record,
            TransferLifecycleFence::LeaseReaping {
                latest_control: evidence.clone(),
            },
        )
        .unwrap();
        assert!(verify_latest_control(&event, &evidence, &control).is_ok());
        assert_eq!(
            verify_latest_control(&event, &evidence, &control),
            verify_control_fence(&event.owning_seat, &event.fence, &evidence, &control)
        );
        let mut malformed = control.clone();
        malformed.next_engine_seq += 1;
        malformed.producer_incarnation = "not-a-hash".into();
        assert!(
            verify_control_fence(&event.owning_seat, &event.fence, &evidence, &malformed).is_err()
        );
        assert_eq!(event.evidence_links().len(), 2);
        let value = json!({"event_type":TRANSFER_LIFECYCLE_EVENT,
            "thread_id":event.owning_seat.thread_id,
            "chain_root_id":event.owning_seat.chain_root_id,
            "payload":event});
        assert_eq!(from_event_value(&value).unwrap(), event);
        let mut wrong = event.clone();
        wrong.owning_seat.presentation_id = "99999999-9999-4999-8999-999999999999".into();
        assert!(wrong.validate_shape().is_err());
        let mut changed = event.clone();
        changed.fence = TransferLifecycleFence::ClosedControl {
            latest_control: evidence,
        };
        assert!(changed.validate_shape().is_err());
        assert!(
            TransferLifecycleEventPayload::new(
                event.payload.clone(),
                TransferLifecycleFence::ClosedControl {
                    latest_control: event.fence.latest_control().clone(),
                }
            )
            .and_then(|closed| verify_latest_control(
                &closed,
                closed.fence.latest_control(),
                &control
            ))
            .is_err()
        );
    }

    #[test]
    fn orphan_cancellation_is_destination_owned_and_retains_inbound_link() {
        let (mut record, evidence, _) = fixture();
        let inbound = TransferEvidence {
            event_hash: "b".repeat(64),
            chain_seq: 3,
        };
        record.phase = TransferPhase::OrphanCancelled {
            inbound: inbound.clone(),
        };
        let payload = TransferLifecycleEventPayload::new(
            record.clone(),
            TransferLifecycleFence::LeaseReaping {
                latest_control: evidence.clone(),
            },
        )
        .unwrap();
        assert_eq!(payload.owning_seat, record.transfer.destination);
        assert_eq!(payload.evidence_links(), vec![&inbound, &evidence]);
    }

    #[test]
    fn a_fence_commits_to_the_exact_control_it_names() {
        let (record, evidence, control) = fixture();
        let event = TransferLifecycleEventPayload::new(
            record,
            TransferLifecycleFence::LeaseReaping {
                latest_control: evidence.clone(),
            },
        )
        .unwrap();
        let mut other_control = event.clone();
        other_control.fence = TransferLifecycleFence::LeaseReaping {
            latest_control: TransferEvidence {
                event_hash: "9".repeat(64),
                chain_seq: 5,
            },
        };
        assert!(other_control.validate_shape().is_err());
        assert!(verify_latest_control(&event, &evidence, &control).is_ok());
        assert!(
            verify_latest_control(
                &event,
                &TransferEvidence {
                    event_hash: "9".repeat(64),
                    chain_seq: 5,
                },
                &control
            )
            .is_err()
        );
    }

    #[test]
    fn earlier_lifecycle_event_versions_are_not_decoded() {
        let (record, evidence, _) = fixture();
        let event = TransferLifecycleEventPayload::new(
            record,
            TransferLifecycleFence::LeaseReaping {
                latest_control: evidence,
            },
        )
        .unwrap();
        for earlier in [
            "contract.ryeos_ui.presentation_transfer_lifecycle.v1",
            "contract.ryeos_ui.presentation_transfer_lifecycle.v2",
        ] {
            let value = json!({"event_type":earlier,
                "thread_id":event.owning_seat.thread_id,
                "chain_root_id":event.owning_seat.chain_root_id,
                "payload":event});
            assert!(from_event_value(&value).is_err());
        }
        let mut schema = serde_json::to_value(&event).unwrap();
        schema["schema"] = json!(2);
        let value = json!({"event_type":TRANSFER_LIFECYCLE_EVENT,
            "thread_id":event.owning_seat.thread_id,
            "chain_root_id":event.owning_seat.chain_root_id,
            "payload":schema});
        assert!(from_event_value(&value).is_err());
    }
}
