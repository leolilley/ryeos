//! Data-only producer-control intents and retained testimony.
//!
//! Exact IDs and internal consistency do not establish authenticated history,
//! current ownership, session authority or permission to append. Existing
//! lifecycle/seat owners must prove those facts at their admission boundary.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SEAT_PRODUCER_EVENT: &str = "ui_seat.producer.v3";
pub const SEAT_PRODUCER_SCHEMA: &str = "ryeos.ui.seat.producer.v3";
pub const SEAT_APPEND_RECEIPT_SCHEMA: &str = "ryeos.ui.seat.append-receipt.v2";

/// Exact durable acknowledgement of one adjacent producer-event interval.
/// Its shape is shared by browser-seat replay and lifecycle history readers;
/// signed placement and current authority are verified by their owners.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SeatAppendReceipt {
    pub schema_version: String,
    pub producer_incarnation: String,
    pub operation_id: String,
    pub first_engine_seq: u64,
    pub last_engine_seq: u64,
    pub event_count: usize,
    pub payload_digest: String,
}

/// Content of one event in a receipt interval. These borrowed fields deliberately
/// contain no storage coordinates or authority claims.
#[derive(Serialize)]
pub struct SeatAppendSemanticEvent<'a> {
    pub engine_seq: u64,
    pub event_type: &'a str,
    pub payload: &'a Value,
}

impl SeatAppendReceipt {
    /// Check the bounded adjacent interval and its exact JSON payload digest.
    /// The caller must separately authenticate event placement and ownership.
    pub fn validate_semantic_interval(
        &self,
        events: &[SeatAppendSemanticEvent<'_>],
        max_events: usize,
    ) -> Result<(), &'static str> {
        if self.event_count == 0
            || self.event_count > max_events
            || self.event_count != events.len()
            || self
                .first_engine_seq
                .checked_add(self.event_count as u64 - 1)
                != Some(self.last_engine_seq)
        {
            return Err("seat receipt has an invalid event interval");
        }
        if events.iter().enumerate().any(|(offset, event)| {
            !event.event_type.starts_with("seat.")
                || self.first_engine_seq.checked_add(offset as u64) != Some(event.engine_seq)
        }) {
            return Err("seat receipt does not cover one exact persisted event interval");
        }
        let value =
            serde_json::to_value(events).map_err(|_| "seat receipt event encoding failed")?;
        let digest = crate::payload_digest::seat_payload_digest(&value)
            .map_err(|_| "seat receipt digest encoding failed")?;
        if digest != self.payload_digest {
            return Err("seat receipt digest differs from its persisted event interval");
        }
        Ok(())
    }
}

#[cfg(test)]
mod receipt_interval_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_semantic_interval_and_digest() {
        let one = json!({"key":"one","value":null});
        let two = json!({"key":"two","value":"é"});
        let events = [
            SeatAppendSemanticEvent {
                engine_seq: 7,
                event_type: "seat.facet",
                payload: &one,
            },
            SeatAppendSemanticEvent {
                engine_seq: 8,
                event_type: "seat.facet",
                payload: &two,
            },
        ];
        let digest =
            crate::payload_digest::seat_payload_digest(&serde_json::to_value(&events).unwrap())
                .unwrap();
        let mut receipt = SeatAppendReceipt {
            schema_version: SEAT_APPEND_RECEIPT_SCHEMA.into(),
            producer_incarnation: "a".repeat(64),
            operation_id: "00000000-0000-4000-8000-000000000002".into(),
            first_engine_seq: 7,
            last_engine_seq: 8,
            event_count: 2,
            payload_digest: digest,
        };
        assert!(receipt.validate_semantic_interval(&events, 128).is_ok());
        receipt.last_engine_seq = 9;
        assert!(receipt.validate_semantic_interval(&events, 128).is_err());
        receipt.last_engine_seq = 8;
        receipt.payload_digest = "b".repeat(64);
        assert!(receipt.validate_semantic_interval(&events, 128).is_err());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiSeatControlPredecessor {
    Initialization { event_hash: String },
    Control { event_hash: String },
}

impl UiSeatControlPredecessor {
    pub fn validate(&self) -> Result<(), &'static str> {
        let (Self::Initialization { event_hash } | Self::Control { event_hash }) = self;
        if !control_digest(event_hash) {
            return Err("control predecessor must name an exact canonical CAS event hash");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiSeatProducerClaim {
    Fresh,
    Resume {
        expected_producer_incarnation: String,
    },
    Takeover {
        expected_producer_incarnation: String,
    },
}

/// The complete immutable control intent. Only operation_id is derived; the
/// random per-intent nonce and original predecessor survive every exact retry.
/// The daemon authenticates this predecessor and conditionally appends; these
/// identifiers do not confer writer/session authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiSeatControlRequest {
    Claim {
        schema: u32,
        operation_id: String,
        presentation_id: String,
        thread_id: String,
        chain_root_id: String,
        predecessor: UiSeatControlPredecessor,
        intent_nonce: String,
        claim: UiSeatProducerClaim,
    },
    Relinquish {
        schema: u32,
        operation_id: String,
        presentation_id: String,
        thread_id: String,
        chain_root_id: String,
        predecessor: UiSeatControlPredecessor,
        intent_nonce: String,
        producer_incarnation: String,
    },
    Close {
        schema: u32,
        operation_id: String,
        presentation_id: String,
        thread_id: String,
        chain_root_id: String,
        predecessor: UiSeatControlPredecessor,
        intent_nonce: String,
        producer_incarnation: String,
    },
}

fn control_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl UiSeatControlRequest {
    pub fn operation_id(&self) -> &str {
        match self {
            Self::Claim { operation_id, .. }
            | Self::Relinquish { operation_id, .. }
            | Self::Close { operation_id, .. } => operation_id,
        }
    }

    pub fn derive_identity(&self) -> Result<String, &'static str> {
        let (schema, presentation_id, thread_id, chain_root_id, predecessor, intent_nonce) =
            match self {
                Self::Claim {
                    schema,
                    presentation_id,
                    thread_id,
                    chain_root_id,
                    predecessor,
                    intent_nonce,
                    ..
                }
                | Self::Relinquish {
                    schema,
                    presentation_id,
                    thread_id,
                    chain_root_id,
                    predecessor,
                    intent_nonce,
                    ..
                }
                | Self::Close {
                    schema,
                    presentation_id,
                    thread_id,
                    chain_root_id,
                    predecessor,
                    intent_nonce,
                    ..
                } => (
                    *schema,
                    presentation_id,
                    thread_id,
                    chain_root_id,
                    predecessor,
                    intent_nonce,
                ),
            };
        if schema != 2
            || !crate::identity::canonical_operation_id(presentation_id)
            || thread_id.is_empty()
            || chain_root_id.is_empty()
            || !crate::identity::canonical_operation_id(intent_nonce)
            || intent_nonce.as_bytes().get(14) != Some(&b'4')
            || !intent_nonce
                .as_bytes()
                .get(19)
                .is_some_and(|byte| matches!(*byte, b'8' | b'9' | b'a' | b'b'))
        {
            return Err(
                "control request requires the current schema, exact seat and random UUID-v4 intent nonce",
            );
        }
        predecessor.validate()?;
        match self {
            Self::Claim {
                claim: UiSeatProducerClaim::Fresh,
                ..
            } => {
                if !matches!(predecessor, UiSeatControlPredecessor::Initialization { .. }) {
                    return Err("fresh claim requires its initialization predecessor");
                }
            }
            Self::Claim {
                claim:
                    UiSeatProducerClaim::Resume {
                        expected_producer_incarnation,
                    }
                    | UiSeatProducerClaim::Takeover {
                        expected_producer_incarnation,
                    },
                ..
            } => {
                if !matches!(predecessor, UiSeatControlPredecessor::Control { .. })
                    || !control_digest(expected_producer_incarnation)
                {
                    return Err(
                        "replacement claim requires its exact control and producer predecessor",
                    );
                }
            }
            Self::Relinquish {
                producer_incarnation,
                ..
            }
            | Self::Close {
                producer_incarnation,
                ..
            } => {
                if !matches!(predecessor, UiSeatControlPredecessor::Control { .. })
                    || !control_digest(producer_incarnation)
                {
                    return Err("producer transition requires its exact control predecessor");
                }
            }
        }
        let mut request =
            serde_json::to_value(self).map_err(|_| "control request encoding failed")?;
        request
            .as_object_mut()
            .ok_or("control request encoding is not an object")?
            .remove("operation_id");
        crate::identity::identity_from_serializable(&serde_json::json!({
            "domain":"ryeos.ui.seat-control.v2", "request":request,
        }))
    }

    pub fn assign_identity(&mut self) -> Result<(), &'static str> {
        let derived = self.derive_identity()?;
        match self {
            Self::Claim { operation_id, .. }
            | Self::Relinquish { operation_id, .. }
            | Self::Close { operation_id, .. } => *operation_id = derived,
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.derive_identity()? != self.operation_id() {
            return Err("control operation ID differs from its original intent commitment");
        }
        Ok(())
    }
}

/// Retained result state, not proof that this producer is current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSeatProducerState {
    Active,
    Relinquished,
    Closed,
}

/// Exact v3 result testimony. Shape validation does not authenticate the
/// signed event or prove that this is the latest control on its owning seat.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UiSeatProducer {
    pub schema_version: String,
    pub request: UiSeatControlRequest,
    pub operation_id: String,
    pub claim: Option<UiSeatProducerClaim>,
    pub predecessor: Option<String>,
    pub producer_incarnation: String,
    pub state: UiSeatProducerState,
    pub next_engine_seq: u64,
}

impl UiSeatProducer {
    /// Validate only the original request commitment and internally consistent
    /// result. Authenticated ancestry, freshness and write authority are external.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.request.validate()?;
        if self.schema_version != SEAT_PRODUCER_SCHEMA
            || !control_digest(&self.producer_incarnation)
            || self.operation_id != self.request.operation_id()
            || self.claim.as_ref()
                != match &self.request {
                    UiSeatControlRequest::Claim { claim, .. } => Some(claim),
                    _ => None,
                }
            || self.predecessor.as_deref()
                != match &self.request {
                    UiSeatControlRequest::Claim {
                        claim: UiSeatProducerClaim::Fresh,
                        ..
                    } => None,
                    UiSeatControlRequest::Claim {
                        claim:
                            UiSeatProducerClaim::Resume {
                                expected_producer_incarnation,
                            }
                            | UiSeatProducerClaim::Takeover {
                                expected_producer_incarnation,
                            },
                        ..
                    } => Some(expected_producer_incarnation.as_str()),
                    UiSeatControlRequest::Relinquish {
                        producer_incarnation,
                        ..
                    }
                    | UiSeatControlRequest::Close {
                        producer_incarnation,
                        ..
                    } => Some(producer_incarnation.as_str()),
                }
            || match &self.request {
                UiSeatControlRequest::Claim { .. } => self.state != UiSeatProducerState::Active,
                UiSeatControlRequest::Relinquish {
                    producer_incarnation,
                    ..
                } => {
                    self.state != UiSeatProducerState::Relinquished
                        || &self.producer_incarnation != producer_incarnation
                }
                UiSeatControlRequest::Close {
                    producer_incarnation,
                    ..
                } => {
                    self.state != UiSeatProducerState::Closed
                        || &self.producer_incarnation != producer_incarnation
                }
            }
        {
            return Err("seat control testimony is inconsistent");
        }
        Ok(())
    }

    /// Compare signed-event placement coordinates in addition to `validate`.
    /// This does not load/authenticate an event or check session membership.
    pub fn validate_subject(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> Result<(), &'static str> {
        let (request_chain, request_thread) = match &self.request {
            UiSeatControlRequest::Claim {
                chain_root_id,
                thread_id,
                ..
            }
            | UiSeatControlRequest::Relinquish {
                chain_root_id,
                thread_id,
                ..
            }
            | UiSeatControlRequest::Close {
                chain_root_id,
                thread_id,
                ..
            } => (chain_root_id, thread_id),
        };
        if request_chain != chain_root_id || request_thread != thread_id {
            return Err("seat control subject differs from signed event");
        }
        Ok(())
    }
}

/// Verify a control result against an already authenticated predecessor and
/// signed event coordinates. The caller remains responsible for authenticating
/// that history and enforcing current writer authority.
pub fn authenticate_control_transition(
    next: &UiSeatProducer,
    previous: Option<&UiSeatProducer>,
    predecessor: &UiSeatControlPredecessor,
    presentation_id: &str,
    chain_root_id: &str,
    thread_id: &str,
    next_engine_seq: u64,
) -> Result<(), &'static str> {
    next.validate()?;
    next.validate_subject(chain_root_id, thread_id)?;
    if next.next_engine_seq != next_engine_seq {
        return Err("control frontier differs from authenticated producer history");
    }
    let (request_predecessor, request_presentation) = match &next.request {
        UiSeatControlRequest::Claim {
            predecessor,
            presentation_id,
            ..
        }
        | UiSeatControlRequest::Relinquish {
            predecessor,
            presentation_id,
            ..
        }
        | UiSeatControlRequest::Close {
            predecessor,
            presentation_id,
            ..
        } => (predecessor, presentation_id),
    };
    if request_predecessor != predecessor || request_presentation != presentation_id {
        return Err("control predecessor or presentation differs from signed history");
    }
    match (&next.request, previous) {
        (
            UiSeatControlRequest::Claim {
                claim: UiSeatProducerClaim::Fresh,
                ..
            },
            None,
        ) => {}
        (
            UiSeatControlRequest::Claim {
                claim:
                    UiSeatProducerClaim::Resume {
                        expected_producer_incarnation,
                    },
                ..
            },
            Some(previous),
        ) if previous.state == UiSeatProducerState::Relinquished
            && &previous.producer_incarnation == expected_producer_incarnation
            && previous.producer_incarnation != next.producer_incarnation => {}
        (
            UiSeatControlRequest::Claim {
                claim:
                    UiSeatProducerClaim::Takeover {
                        expected_producer_incarnation,
                    },
                ..
            },
            Some(previous),
        ) if previous.state != UiSeatProducerState::Closed
            && &previous.producer_incarnation == expected_producer_incarnation
            && previous.producer_incarnation != next.producer_incarnation => {}
        (
            UiSeatControlRequest::Relinquish {
                producer_incarnation,
                ..
            }
            | UiSeatControlRequest::Close {
                producer_incarnation,
                ..
            },
            Some(previous),
        ) if previous.state == UiSeatProducerState::Active
            && &previous.producer_incarnation == producer_incarnation
            && next.producer_incarnation == previous.producer_incarnation => {}
        _ => return Err("control transition is not valid at authenticated predecessor"),
    }
    Ok(())
}

#[cfg(test)]
mod control_transition_tests {
    use super::*;

    fn control(
        predecessor: UiSeatControlPredecessor,
        claim: UiSeatProducerClaim,
        incarnation: char,
    ) -> UiSeatProducer {
        let mut request = UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: "11111111-1111-4111-8111-111111111111".into(),
            thread_id: "thread".into(),
            chain_root_id: "root".into(),
            predecessor,
            intent_nonce: "22222222-2222-4222-8222-222222222222".into(),
            claim: claim.clone(),
        };
        request.assign_identity().unwrap();
        let expected = match &claim {
            UiSeatProducerClaim::Fresh => None,
            UiSeatProducerClaim::Resume {
                expected_producer_incarnation,
            }
            | UiSeatProducerClaim::Takeover {
                expected_producer_incarnation,
            } => Some(expected_producer_incarnation.clone()),
        };
        UiSeatProducer {
            schema_version: SEAT_PRODUCER_SCHEMA.into(),
            operation_id: request.operation_id().into(),
            request,
            claim: Some(claim),
            predecessor: expected,
            producer_incarnation: incarnation.to_string().repeat(64),
            state: UiSeatProducerState::Active,
            next_engine_seq: 7,
        }
    }

    fn verify(
        next: &UiSeatProducer,
        previous: Option<&UiSeatProducer>,
        predecessor: &UiSeatControlPredecessor,
    ) -> Result<(), &'static str> {
        authenticate_control_transition(
            next,
            previous,
            predecessor,
            "11111111-1111-4111-8111-111111111111",
            "root",
            "thread",
            7,
        )
    }

    #[test]
    fn fresh_and_authenticated_coordinates() {
        let initial = UiSeatControlPredecessor::Initialization {
            event_hash: "a".repeat(64),
        };
        let fresh = control(initial.clone(), UiSeatProducerClaim::Fresh, 'b');
        assert!(verify(&fresh, None, &initial).is_ok());
        assert!(verify(&fresh, Some(&fresh), &initial).is_err());
        assert!(
            verify(
                &fresh,
                None,
                &UiSeatControlPredecessor::Initialization {
                    event_hash: "c".repeat(64)
                }
            )
            .is_err()
        );
        assert!(
            authenticate_control_transition(&fresh, None, &initial, "other", "root", "thread", 7)
                .is_err()
        );
        assert!(
            authenticate_control_transition(
                &fresh,
                None,
                &initial,
                "11111111-1111-4111-8111-111111111111",
                "root",
                "thread",
                8
            )
            .is_err()
        );
    }

    #[test]
    fn resume_takeover_relinquish_and_close() {
        let predecessor = UiSeatControlPredecessor::Control {
            event_hash: "c".repeat(64),
        };
        let initial = UiSeatControlPredecessor::Initialization {
            event_hash: "a".repeat(64),
        };
        let active = control(initial, UiSeatProducerClaim::Fresh, 'b');
        let mut relinquished = active.clone();
        relinquished.state = UiSeatProducerState::Relinquished;
        let mut closed = active.clone();
        closed.state = UiSeatProducerState::Closed;
        let resume = control(
            predecessor.clone(),
            UiSeatProducerClaim::Resume {
                expected_producer_incarnation: active.producer_incarnation.clone(),
            },
            'd',
        );
        assert!(verify(&resume, Some(&relinquished), &predecessor).is_ok());
        assert!(verify(&resume, Some(&active), &predecessor).is_err());
        assert!(verify(&resume, Some(&closed), &predecessor).is_err());
        let takeover = control(
            predecessor.clone(),
            UiSeatProducerClaim::Takeover {
                expected_producer_incarnation: active.producer_incarnation.clone(),
            },
            'd',
        );
        assert!(verify(&takeover, Some(&active), &predecessor).is_ok());
        assert!(verify(&takeover, Some(&relinquished), &predecessor).is_ok());
        assert!(verify(&takeover, Some(&closed), &predecessor).is_err());
        let mut same = takeover.clone();
        same.producer_incarnation = active.producer_incarnation.clone();
        assert!(verify(&same, Some(&active), &predecessor).is_err());
        for close in [false, true] {
            let mut request = if close {
                UiSeatControlRequest::Close {
                    schema: 2,
                    operation_id: String::new(),
                    presentation_id: "11111111-1111-4111-8111-111111111111".into(),
                    thread_id: "thread".into(),
                    chain_root_id: "root".into(),
                    predecessor: predecessor.clone(),
                    intent_nonce: "22222222-2222-4222-8222-222222222222".into(),
                    producer_incarnation: active.producer_incarnation.clone(),
                }
            } else {
                UiSeatControlRequest::Relinquish {
                    schema: 2,
                    operation_id: String::new(),
                    presentation_id: "11111111-1111-4111-8111-111111111111".into(),
                    thread_id: "thread".into(),
                    chain_root_id: "root".into(),
                    predecessor: predecessor.clone(),
                    intent_nonce: "22222222-2222-4222-8222-222222222222".into(),
                    producer_incarnation: active.producer_incarnation.clone(),
                }
            };
            request.assign_identity().unwrap();
            let transition = UiSeatProducer {
                schema_version: SEAT_PRODUCER_SCHEMA.into(),
                operation_id: request.operation_id().into(),
                request,
                claim: None,
                predecessor: Some(active.producer_incarnation.clone()),
                producer_incarnation: active.producer_incarnation.clone(),
                state: if close {
                    UiSeatProducerState::Closed
                } else {
                    UiSeatProducerState::Relinquished
                },
                next_engine_seq: 7,
            };
            assert!(verify(&transition, Some(&active), &predecessor).is_ok());
            assert!(verify(&transition, Some(&relinquished), &predecessor).is_err());
        }
    }
}
