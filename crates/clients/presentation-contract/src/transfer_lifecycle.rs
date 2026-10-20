//! System-attributed transfer settlement testimony, not lifecycle admission.
//!
//! This is deliberately not the browser producer wrapper: there is no engine
//! sequence and no claim of a live producer, attachment, grant or execution.
//! A valid shape or digest does NOT prove the referenced fence is authoritative.
//! The existing lifecycle owner must authenticate it, prove the complete current
//! transfer phase, and conditionally append under its shared producer fence.

use crate::{
    payload_digest::seat_payload_digest,
    seat_control::{UiSeatControlRequest, UiSeatProducer, UiSeatProducerState},
    transfer::{TransferEvidence, TransferPhase, TransferRecord},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TRANSFER_LIFECYCLE_EVENT: &str = "contract.ryeos_ui.presentation_transfer_lifecycle.v1";
pub const TRANSFER_LIFECYCLE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferLifecycleWriter {
    NodeLifecycle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferLifecycleEventPayload {
    pub schema: u32,
    pub writer: TransferLifecycleWriter,
    pub operation_id: String,
    /// Deterministic commitment to this exact settlement and retained fence.
    /// It is not a new decision or a browser producer sequence.
    pub settlement_id: String,
    /// Exact owning-seat producer-fence testimony. Only the lifecycle owner
    /// can establish its kind, authority, ancestry and continuing applicability.
    pub producer_fence: TransferEvidence,
    pub payload: TransferRecord,
}

impl TransferLifecycleEventPayload {
    /// Construct data only. In particular, this cannot mint a fenced-seat
    /// admission token or authorize writing a terminal or removed seat.
    pub fn new(payload: TransferRecord, producer_fence: TransferEvidence) -> Result<Self, String> {
        let mut event = Self {
            schema: TRANSFER_LIFECYCLE_SCHEMA,
            writer: TransferLifecycleWriter::NodeLifecycle,
            operation_id: payload.transfer.operation_id.clone(),
            settlement_id: String::new(),
            producer_fence,
            payload,
        };
        event.settlement_id = event.derive_settlement_id()?;
        event.validate_shape()?;
        Ok(event)
    }

    pub fn validate_shape(&self) -> Result<(), String> {
        self.payload.validate_shape()?;
        if self.schema != TRANSFER_LIFECYCLE_SCHEMA
            || self.operation_id != self.payload.transfer.operation_id
            || !matches!(
                self.payload.phase,
                TransferPhase::Aborted { .. }
                    | TransferPhase::Released { .. }
                    | TransferPhase::Retained { .. }
                    | TransferPhase::SettlementObserved { .. }
            )
        {
            return Err("lifecycle testimony is not a permitted deterministic settlement".into());
        }
        if !canonical_hash(&self.producer_fence.event_hash)
            || self.producer_fence.chain_seq == 0
            || self.settlement_id != self.derive_settlement_id()?
        {
            return Err(
                "lifecycle settlement identity or producer-fence coordinate differs".into(),
            );
        }
        Ok(())
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

    /// Both the ordinary transfer ancestry and the exact producer fence stay
    /// reachable through the existing typed ThreadEvent object contract.
    pub fn evidence_links(&self) -> Vec<&TransferEvidence> {
        let mut links = self.payload.evidence_links();
        links.push(&self.producer_fence);
        links
    }

    fn derive_settlement_id(&self) -> Result<String, String> {
        seat_payload_digest(&serde_json::json!({
            "domain": TRANSFER_LIFECYCLE_EVENT,
            "schema": self.schema,
            "writer": self.writer,
            "producer_fence": self.producer_fence,
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

/// Validate only the closed system envelope and exact owning-seat placement.
/// Referenced current-phase/fence proof and writer admission remain separate.
pub fn from_event_value(event: &Value) -> Result<TransferLifecycleEventPayload, String> {
    if event.get("event_type").and_then(Value::as_str) != Some(TRANSFER_LIFECYCLE_EVENT) {
        return Err("not a current lifecycle transfer settlement event".into());
    }
    let payload: TransferLifecycleEventPayload = serde_json::from_value(
        event
            .get("payload")
            .cloned()
            .ok_or("lifecycle transfer event omitted its payload")?,
    )
    .map_err(|error| format!("invalid lifecycle transfer settlement: {error}"))?;
    payload.validate_shape()?;
    let owner = payload.payload.event_owner();
    if event.get("thread_id").and_then(Value::as_str) != Some(owner.thread_id.as_str())
        || event.get("chain_root_id").and_then(Value::as_str) != Some(owner.chain_root_id.as_str())
    {
        return Err("lifecycle transfer settlement is not on its exact owning seat".into());
    }
    Ok(payload)
}

/// Compare the exact retained Closed testimony with this system envelope.
/// This is a pure consistency check, not a proof of CAS ancestry, latest-control
/// status, a legal control predecessor/engine frontier, session expiry or append
/// admission. The state/lifecycle owner must establish the applicable approved
/// fence at the same fixed anchor used for its conditional publication. A
/// browser's explicit Close does not testify that its session has expired.
pub fn verify_closed_producer_fence(
    settlement: &TransferLifecycleEventPayload,
    evidence: &TransferEvidence,
    control: &UiSeatProducer,
) -> Result<(), String> {
    settlement.validate_shape()?;
    control.validate().map_err(str::to_owned)?;
    let owner = settlement.payload.event_owner();
    control
        .validate_subject(&owner.chain_root_id, &owner.thread_id)
        .map_err(str::to_owned)?;
    if &settlement.producer_fence != evidence || control.state != UiSeatProducerState::Closed {
        return Err("settlement does not reference its exact Closed producer fence".into());
    }
    let UiSeatControlRequest::Close {
        presentation_id, ..
    } = &control.request
    else {
        return Err("settlement fence is not an original Close control".into());
    };
    if presentation_id != &owner.presentation_id {
        return Err("settlement fence belongs to another presentation".into());
    }
    // The closing producer may have taken over after preparation. Historical
    // transfer identity survives that replacement; do not impersonate its old
    // incarnation or require a fresh producer to adopt the transfer.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::{TransferIdentity, TransferSeat};
    use serde_json::json;

    fn evidence(marker: &str, chain_seq: u64) -> TransferEvidence {
        TransferEvidence {
            event_hash: marker.repeat(32),
            chain_seq,
        }
    }

    fn closed(record: &TransferRecord) -> UiSeatProducer {
        use crate::seat_control::{SEAT_PRODUCER_SCHEMA, UiSeatControlPredecessor};
        let owner = record.event_owner();
        let mut request = UiSeatControlRequest::Close {
            schema: 2,
            operation_id: String::new(),
            presentation_id: owner.presentation_id.clone(),
            thread_id: owner.thread_id.clone(),
            chain_root_id: owner.chain_root_id.clone(),
            predecessor: UiSeatControlPredecessor::Control {
                event_hash: "ee".repeat(32),
            },
            intent_nonce: "44444444-4444-4444-8444-444444444444".into(),
            producer_incarnation: "cd".repeat(32),
        };
        request.assign_identity().unwrap();
        UiSeatProducer {
            schema_version: SEAT_PRODUCER_SCHEMA.into(),
            operation_id: request.operation_id().into(),
            request,
            claim: None,
            predecessor: Some("cd".repeat(32)),
            producer_incarnation: "cd".repeat(32),
            state: UiSeatProducerState::Closed,
            next_engine_seq: 8,
        }
    }

    fn record() -> TransferRecord {
        let seat = |presentation_id: &str, side: &str| TransferSeat {
            presentation_id: presentation_id.into(),
            thread_id: format!("thread-{side}"),
            chain_root_id: format!("chain-{side}"),
            producer_incarnation: "ab".repeat(32),
            revision: 1,
        };
        TransferRecord {
            schema: 1,
            transfer: TransferIdentity {
                operation_id: "33333333-3333-4333-8333-333333333333".into(),
                source: seat("11111111-1111-4111-8111-111111111111", "source"),
                destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
                export_digest: seat_payload_digest(&json!({"view": "retained"})).unwrap(),
            },
            phase: TransferPhase::Aborted {
                preparation: evidence("aa", 2),
            },
        }
    }

    fn event(payload: &TransferLifecycleEventPayload) -> Value {
        json!({
            "event_type": TRANSFER_LIFECYCLE_EVENT,
            "thread_id": payload.payload.event_owner().thread_id,
            "chain_root_id": payload.payload.event_owner().chain_root_id,
            "payload": payload,
        })
    }

    #[test]
    fn exact_retry_has_stable_system_identity_and_no_producer_sequence() {
        let payload = TransferLifecycleEventPayload::new(record(), evidence("ff", 9)).unwrap();
        assert_eq!(
            payload,
            TransferLifecycleEventPayload::new(record(), evidence("ff", 9)).unwrap()
        );
        let value = event(&payload);
        assert_eq!(value["payload"]["writer"], "node_lifecycle");
        assert!(value["payload"].get("seq").is_none());
        assert_eq!(from_event_value(&value).unwrap(), payload);
        assert!(crate::transfer::from_event_value(&value).is_err());
        assert_eq!(
            payload.evidence_links(),
            vec![&evidence("aa", 2), &evidence("ff", 9)]
        );
    }

    #[test]
    fn lifecycle_writer_cannot_prepare_accept_commit_or_activate() {
        let export = json!({"mode": "copy", "view": "retained"});
        let reservation = json!({"mount": 1});
        let digest = seat_payload_digest(&reservation).unwrap();
        for phase in [
            TransferPhase::InboundCommitment,
            TransferPhase::Prepared {
                inbound: evidence("00", 1),
                freeze: None,
                export,
            },
            TransferPhase::Accepted {
                preparation: evidence("aa", 2),
                reservation,
                import_digest: digest.clone(),
            },
            TransferPhase::Committed {
                preparation: evidence("aa", 2),
                acceptance: evidence("bb", 3),
                import_digest: digest.clone(),
            },
            TransferPhase::Activated {
                commit: evidence("cc", 4),
                import_digest: digest,
            },
        ] {
            let mut record = record();
            record.phase = phase;
            assert!(TransferLifecycleEventPayload::new(record, evidence("ff", 9)).is_err());
        }
    }

    #[test]
    fn settlement_whitelist_keeps_exact_owner_and_all_typed_links() {
        for phase in [
            record().phase,
            TransferPhase::Released {
                abort: evidence("cc", 4),
                acceptance: evidence("bb", 3),
                import_digest: "dd".repeat(32),
            },
            TransferPhase::Retained {
                commit: evidence("cc", 4),
                import_digest: "dd".repeat(32),
            },
            TransferPhase::SettlementObserved {
                settlement: evidence("ee", 5),
            },
        ] {
            let mut record = record();
            record.phase = phase;
            let payload =
                TransferLifecycleEventPayload::new(record.clone(), evidence("ff", 9)).unwrap();
            assert_eq!(from_event_value(&event(&payload)).unwrap(), payload);
            let mut expected = record.evidence_links();
            let fence = evidence("ff", 9);
            expected.push(&fence);
            assert_eq!(payload.evidence_links(), expected);
            let mut wrong = event(&payload);
            wrong["thread_id"] = json!("foreign");
            assert!(from_event_value(&wrong).is_err());
        }
    }

    #[test]
    fn changed_fence_payload_identity_and_unknown_writer_are_rejected() {
        let payload = TransferLifecycleEventPayload::new(record(), evidence("ff", 9)).unwrap();
        let original = event(&payload);
        for (field, value) in [
            ("writer", json!("browser")),
            ("schema", json!(0)),
            (
                "operation_id",
                json!("44444444-4444-4444-8444-444444444444"),
            ),
            ("settlement_id", json!("00".repeat(32))),
            ("producer_fence", json!(evidence("ff", 10))),
            ("seq", json!(1)),
        ] {
            let mut wrong = original.clone();
            wrong["payload"][field] = value;
            assert!(
                from_event_value(&wrong).is_err(),
                "accepted changed {field}"
            );
        }
        assert!(TransferLifecycleEventPayload::new(record(), evidence("ff", 0)).is_err());
        let size = serde_json::to_vec(&payload).unwrap().len() as u64;
        assert!(payload.validate_bounds(size).is_ok());
        assert!(payload.validate_bounds(size - 1).is_err());
        assert!(payload.validate_bounds(0).is_err());
    }

    #[test]
    fn closed_fence_requires_exact_evidence_and_presentation_not_old_incarnation() {
        let record = record();
        let control = closed(&record);
        let fence = evidence("ff", 9);
        let payload = TransferLifecycleEventPayload::new(record, fence.clone()).unwrap();
        assert!(verify_closed_producer_fence(&payload, &fence, &control).is_ok());
        assert!(verify_closed_producer_fence(&payload, &evidence("ff", 10), &control).is_err());
        assert!(verify_closed_producer_fence(&payload, &evidence("ee", 9), &control).is_err());

        let mut other = payload.payload.clone();
        other.transfer.source.presentation_id = "55555555-5555-4555-8555-555555555555".into();
        let other_control = closed(&other);
        assert!(verify_closed_producer_fence(&payload, &fence, &other_control).is_err());
        let mut not_closed = control;
        not_closed.state = UiSeatProducerState::Relinquished;
        assert!(verify_closed_producer_fence(&payload, &fence, &not_closed).is_err());
    }
}
