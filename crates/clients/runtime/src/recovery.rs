//! What to offer when a seat is lost, and how to settle what it left behind.
//!
//! When delivery stops ([`crate::seat::Unavailable`]) nothing is declared
//! about the old writer. The operator first reconciles the retained append,
//! then observes the seat read-only, and only then chooses: resume a writer
//! that relinquished, take over one that did not, or open an independent
//! presentation. Choices are derived mechanically from the observation, never
//! from diagnostic text, and the daemon checks the predecessor again when the
//! chosen claim arrives.

use std::rc::Rc;

use ryeos_client_base::ui::{
    RyeOsCore, RyeOsEffect, UiObservedPresentationProducer, UiPresentationProducerObservation,
    UiSeatControlPredecessor,
};

use crate::api::wire_events_digest;
use crate::error::RuntimeError;
use crate::platform::{Platform, Store};
use crate::replay::replay_acknowledged_events;
use crate::retention;
use crate::seat::{
    AppendFailure, PlainSeatEvent, RetainedClaim, RetryPolicy, SeatShared, SeatStart,
    observe_producer, reconcile_retained_append, wire_seat_events,
};

/// One way to continue after a seat was lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconnectChoice {
    /// Resume the writer that relinquished.
    Resume {
        expected_producer_incarnation: String,
        predecessor: UiSeatControlPredecessor,
    },
    /// Replace the writer the operator observed.
    Takeover {
        expected_producer_incarnation: String,
        predecessor: UiSeatControlPredecessor,
    },
    /// Open a distinct presentation. This does not resume or take over the
    /// existing writer.
    Independent,
}

impl ReconnectChoice {
    /// The start that carries this choice out for `presentation_id`.
    pub fn start_mode(&self, presentation_id: &str) -> SeatStart {
        match self {
            Self::Resume {
                expected_producer_incarnation,
                predecessor,
            } => SeatStart::Retained {
                presentation_id: presentation_id.to_owned(),
                claim: RetainedClaim::Resume {
                    expected_producer_incarnation: expected_producer_incarnation.clone(),
                    predecessor: predecessor.clone(),
                },
            },
            Self::Takeover {
                expected_producer_incarnation,
                predecessor,
            } => SeatStart::Retained {
                presentation_id: presentation_id.to_owned(),
                claim: RetainedClaim::Takeover {
                    expected_producer_incarnation: expected_producer_incarnation.clone(),
                    predecessor: predecessor.clone(),
                },
            },
            Self::Independent => SeatStart::Fresh,
        }
    }
}

/// The replacement choices an observation supports. A relinquished writer may
/// be resumed or taken over; an active one only taken over; an unclaimed or
/// closed seat offers none (an independent presentation is always available
/// and is added by the caller).
pub fn replacement_choices(
    observation: &UiPresentationProducerObservation,
) -> Vec<ReconnectChoice> {
    let predecessor = observation.control_predecessor.clone();
    match &observation.producer {
        UiObservedPresentationProducer::Relinquished {
            producer_incarnation,
        } => vec![
            ReconnectChoice::Resume {
                expected_producer_incarnation: producer_incarnation.clone(),
                predecessor: predecessor.clone(),
            },
            ReconnectChoice::Takeover {
                expected_producer_incarnation: producer_incarnation.clone(),
                predecessor,
            },
        ],
        UiObservedPresentationProducer::Active {
            producer_incarnation,
        } => vec![ReconnectChoice::Takeover {
            expected_producer_incarnation: producer_incarnation.clone(),
            predecessor,
        }],
        UiObservedPresentationProducer::Unclaimed
        | UiObservedPresentationProducer::Closed { .. } => Vec::new(),
    }
}

fn append_failure(failure: AppendFailure) -> RuntimeError {
    RuntimeError::Fatal(failure.message().to_owned())
}

/// Settle the retained append of the current seat against the daemon.
///
/// With a live producer the retained request must be exactly the producer's
/// unresolved append and the core's unacknowledged prefix; its acknowledgement
/// is then adopted as if it had arrived on the first try, and the effects the
/// core emits are returned. Without a producer the receipt only proves the
/// record can go. Nothing retained is cleared unless the daemon confirmed it.
pub async fn reconcile_pending<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    core: &mut RyeOsCore,
    policy: RetryPolicy,
) -> Result<Vec<RyeOsEffect>, RuntimeError> {
    let platform = &*shared.platform;
    let (coordinate, root, key, session_id) = {
        let delivery = shared.delivery.borrow();
        let (Some(coordinate), Some(root), Some(key)) = (
            delivery.coordinate.clone(),
            delivery.root.clone(),
            delivery.retention_key.clone(),
        ) else {
            return Err(RuntimeError::Fatal(
                "presentation must be prepared before reconciliation".to_owned(),
            ));
        };
        (coordinate, root, key, delivery.session_id.clone())
    };
    let Some(reconciled) =
        reconcile_retained_append(platform, &session_id, &root, &coordinate, &key, policy)
            .await
            .map_err(append_failure)?
    else {
        return Ok(Vec::new());
    };
    let record = &reconciled.record;
    let Some(producer) = shared.current_producer() else {
        retention::clear_matching(
            platform.store(),
            &key,
            &record.operation_id,
            &record.producer_incarnation,
            Some(record.encoded_request.as_str()),
        )?;
        return Ok(Vec::new());
    };

    // A live producer owns this append: prove it is the core's own prefix.
    let mismatch = || {
        RuntimeError::Fatal(
            "retained receipt does not match the current producer's pending interval".to_owned(),
        )
    };
    let (captured, first) = {
        let delivery = shared.delivery.borrow();
        let pending = delivery.pending.as_ref().ok_or_else(mismatch)?;
        let first: u64 = record.first_engine_seq.parse().map_err(|_| mismatch())?;
        if pending.record != *record
            || producer.producer_incarnation != record.producer_incarnation
            || delivery.next_engine_seq != first
        {
            return Err(mismatch());
        }
        (pending.events.clone(), first)
    };
    let acknowledged = core.acknowledged_seat_prefix_len();
    let local = core.seat.events();
    if local.get(acknowledged..acknowledged + captured.len()) != Some(captured.as_slice()) {
        return Err(RuntimeError::Fatal(
            "retained receipt does not prove the exact queued prefix".to_owned(),
        ));
    }
    let plain: Vec<PlainSeatEvent> = captured
        .iter()
        .map(PlainSeatEvent::try_from)
        .collect::<Result<_, _>>()
        .map_err(|error| RuntimeError::Fatal(error.0))?;
    let wire = wire_seat_events(&plain, first).map_err(|error| RuntimeError::Fatal(error.0))?;
    if wire_events_digest(&wire)? != record.payload_digest || wire != reconciled.events {
        return Err(RuntimeError::Fatal(
            "retained receipt does not prove the exact queued prefix".to_owned(),
        ));
    }
    let last: u64 = record.last_engine_seq.parse().map_err(|_| mismatch())?;
    let effects = replay_acknowledged_events(core, &producer, captured, &reconciled.ack.rows)
        .map_err(RuntimeError::Fatal)?;
    {
        let mut delivery = shared.delivery.borrow_mut();
        delivery.next_engine_seq = last + 1;
        delivery.remember_receipt(reconciled.ack.summary.clone());
        delivery.pending = None;
        delivery.blocked = false;
    }
    retention::clear_matching(
        platform.store(),
        &key,
        &record.operation_id,
        &record.producer_incarnation,
        Some(record.encoded_request.as_str()),
    )?;
    Ok(effects)
}

/// Reconcile the retained append, then read the seat's writer: the state an
/// operator needs before choosing. Returns the observation and the
/// replacement choices it supports.
pub async fn observe_for_reconnect<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    core: &mut RyeOsCore,
    policy: RetryPolicy,
) -> Result<
    (
        UiPresentationProducerObservation,
        Vec<ReconnectChoice>,
        Vec<RyeOsEffect>,
    ),
    RuntimeError,
> {
    let effects = reconcile_pending(shared, core, policy).await?;
    let (coordinate, root) = {
        let delivery = shared.delivery.borrow();
        let (Some(coordinate), Some(root)) = (delivery.coordinate.clone(), delivery.root.clone())
        else {
            return Err(RuntimeError::Fatal(
                "producer observation requires a retained presentation".to_owned(),
            ));
        };
        (coordinate, root)
    };
    let observation = observe_producer(&*shared.platform, &coordinate, &root).await?;
    let choices = replacement_choices(&observation);
    Ok((observation, choices, effects))
}

/// [`observe_for_reconnect`] after a start that failed before any core was
/// installed. With no live producer a retained receipt only proves its record
/// can go, so no core is needed; a runtime that holds a producer must
/// reconcile through its core instead.
pub async fn observe_without_core<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    policy: RetryPolicy,
) -> Result<(UiPresentationProducerObservation, Vec<ReconnectChoice>), RuntimeError> {
    if shared.current_producer().is_some() {
        return Err(RuntimeError::Fatal(
            "a live producer reconciles through its core".to_owned(),
        ));
    }
    let (coordinate, root, key, session_id) = {
        let delivery = shared.delivery.borrow();
        let (Some(coordinate), Some(root), Some(key)) = (
            delivery.coordinate.clone(),
            delivery.root.clone(),
            delivery.retention_key.clone(),
        ) else {
            return Err(RuntimeError::Fatal(
                "producer observation requires a retained presentation".to_owned(),
            ));
        };
        (coordinate, root, key, delivery.session_id.clone())
    };
    let platform = &*shared.platform;
    if let Some(reconciled) =
        reconcile_retained_append(platform, &session_id, &root, &coordinate, &key, policy)
            .await
            .map_err(append_failure)?
    {
        let record = &reconciled.record;
        retention::clear_matching(
            platform.store(),
            &key,
            &record.operation_id,
            &record.producer_incarnation,
            Some(record.encoded_request.as_str()),
        )?;
    }
    let observation = observe_producer(platform, &coordinate, &root).await?;
    let choices = replacement_choices(&observation);
    Ok((observation, choices))
}

/// [`ensure_settled`] for a runtime that never installed a core: only the
/// unresolved append and its retained record can be unsettled.
pub fn ensure_settled_without_core<P: Platform>(
    shared: &Rc<SeatShared<P>>,
) -> Result<(), RuntimeError> {
    let delivery = shared.delivery.borrow();
    let retained = delivery
        .retention_key
        .as_deref()
        .and_then(|key| shared.platform.store().get(key))
        .is_some();
    if delivery.pending.is_some() || retained {
        return Err(RuntimeError::Fatal(
            "queued seat mutations must settle before reconnect".to_owned(),
        ));
    }
    Ok(())
}

/// A replacement may start only when nothing local is unsettled: no
/// unresolved append, no retained record and no unmirrored event. Queued
/// local work is never discarded, even for an independent presentation.
pub fn ensure_settled<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    core: &RyeOsCore,
) -> Result<(), RuntimeError> {
    let delivery = shared.delivery.borrow();
    let retained = delivery
        .retention_key
        .as_deref()
        .and_then(|key| shared.platform.store().get(key))
        .is_some();
    if delivery.pending.is_some() || retained {
        return Err(RuntimeError::Fatal(
            "queued seat mutations must settle before reconnect".to_owned(),
        ));
    }
    if core.seat.events().len() != core.acknowledged_seat_prefix_len() {
        return Err(RuntimeError::Fatal(
            "uncaptured seat mutations must settle before reconnect".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn observation(
        producer: serde_json::Value,
        predecessor_kind: &str,
    ) -> UiPresentationProducerObservation {
        serde_json::from_value(json!({
            "presentation_id": "presentation",
            "thread_id": "thread",
            "chain_root_id": "chain",
            "root_binding": {
                "binding_attachment_id": "root",
                "binding_generation": 1,
                "binding_digest": "digest",
            },
            "revision": 1,
            "control_predecessor": {"kind": predecessor_kind, "event_hash": "d".repeat(64)},
            "producer": producer,
        }))
        .expect("a well-formed observation")
    }

    fn control() -> UiSeatControlPredecessor {
        UiSeatControlPredecessor::Control {
            event_hash: "d".repeat(64),
        }
    }

    /// Only the observed producer state supplies replacement choices, and each
    /// choice carries that state's exact incarnation and the observed
    /// predecessor.
    #[test]
    fn replacement_choices_follow_the_observed_producer_state() {
        let incarnation = "c".repeat(64);
        let active = observation(
            json!({"kind": "active", "producer_incarnation": incarnation}),
            "control",
        );
        assert_eq!(
            replacement_choices(&active),
            vec![ReconnectChoice::Takeover {
                expected_producer_incarnation: incarnation.clone(),
                predecessor: control(),
            }]
        );

        let relinquished = observation(
            json!({"kind": "relinquished", "producer_incarnation": incarnation}),
            "control",
        );
        assert_eq!(
            replacement_choices(&relinquished),
            vec![
                ReconnectChoice::Resume {
                    expected_producer_incarnation: incarnation.clone(),
                    predecessor: control(),
                },
                ReconnectChoice::Takeover {
                    expected_producer_incarnation: incarnation.clone(),
                    predecessor: control(),
                },
            ]
        );

        let closed = observation(
            json!({"kind": "closed", "producer_incarnation": incarnation}),
            "control",
        );
        assert!(replacement_choices(&closed).is_empty());
        let unclaimed = observation(json!({"kind": "unclaimed"}), "initialization");
        assert!(replacement_choices(&unclaimed).is_empty());
    }

    /// A choice becomes the start that carries exactly its own claim for the
    /// given presentation; an independent choice prepares a new presentation.
    #[test]
    fn start_mode_carries_the_choice_and_its_predecessor() {
        let incarnation = "c".repeat(64);
        let resume = ReconnectChoice::Resume {
            expected_producer_incarnation: incarnation.clone(),
            predecessor: control(),
        };
        assert_eq!(
            resume.start_mode("presentation"),
            SeatStart::Retained {
                presentation_id: "presentation".to_owned(),
                claim: RetainedClaim::Resume {
                    expected_producer_incarnation: incarnation.clone(),
                    predecessor: control(),
                },
            }
        );
        let takeover = ReconnectChoice::Takeover {
            expected_producer_incarnation: incarnation.clone(),
            predecessor: control(),
        };
        assert_eq!(
            takeover.start_mode("other"),
            SeatStart::Retained {
                presentation_id: "other".to_owned(),
                claim: RetainedClaim::Takeover {
                    expected_producer_incarnation: incarnation,
                    predecessor: control(),
                },
            }
        );
        assert_eq!(
            ReconnectChoice::Independent.start_mode("presentation"),
            SeatStart::Fresh
        );
    }
}
