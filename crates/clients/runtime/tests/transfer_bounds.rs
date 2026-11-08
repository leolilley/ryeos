//! Byte and count bounds on what a seat sends: transfer and observation
//! requests against the admitted request size, and the unsaved-event limits
//! that fence a stuck writer.

use ryeos_client_base::ui::{SeatEvent, SeatEventKind, UiBindingRequestBounds};
use ryeos_client_runtime::api::{
    MAX_PENDING_SEAT_EVENTS, observe_destination_request, transfer_contact_request,
    transfer_prepare_request,
};
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::seat::PendingTracker;
use serde_json::{Value, json};

fn bounds(max_request_bytes: u64) -> UiBindingRequestBounds {
    UiBindingRequestBounds {
        max_request_bytes,
        max_input_bytes: 1,
        max_seat_append_bytes: 1,
    }
}

/// Run `encode` under a bound equal to its own output (accepted), one byte
/// less (refused) and zero (an invalid bound, refused).
fn assert_bounded(encode: impl Fn(UiBindingRequestBounds) -> Result<String, RuntimeError>) {
    let unbounded = encode(bounds(1 << 20)).expect("a generous bound accepts");
    let exact = u64::try_from(unbounded.len()).unwrap();
    assert_eq!(encode(bounds(exact)).expect("an exact fit"), unbounded);
    assert!(matches!(
        encode(bounds(exact - 1)),
        Err(RuntimeError::BindingRequest(_))
    ));
    assert!(matches!(
        encode(bounds(0)),
        Err(RuntimeError::BindingRequest(_))
    ));
}

#[test]
fn destination_observation_respects_the_admitted_request_bound() {
    assert_bounded(|bounds| observe_destination_request("presentation", "thread", "chain", bounds));
}

#[test]
fn transfer_preparation_respects_the_admitted_request_bound() {
    assert_bounded(|bounds| {
        transfer_prepare_request(
            &json!({"transfer": "t"}),
            &json!({"export": "e"}),
            &json!({"freeze": "f"}),
            bounds,
        )
    });
}

#[test]
fn transfer_contacts_respect_the_admitted_request_bound() {
    assert_bounded(|bounds| transfer_contact_request(&json!({"action": "commit"}), bounds));
}

fn facet(seq: u64, value: Value) -> SeatEvent {
    SeatEvent {
        seq,
        producer_engine_seq: None,
        kind: SeatEventKind::Facet {
            key: "draft".to_owned(),
            value,
        },
    }
}

/// More than the event limit of unsaved events is refused, and acknowledging
/// a prefix frees room again.
#[test]
fn the_pending_event_count_is_bounded() {
    let events: Vec<SeatEvent> = (0..=MAX_PENDING_SEAT_EVENTS as u64)
        .map(|seq| facet(seq, json!(1)))
        .collect();

    let mut tracker = PendingTracker::new();
    tracker
        .observe(&events[..MAX_PENDING_SEAT_EVENTS], 0)
        .unwrap();
    assert_eq!(tracker.len(), MAX_PENDING_SEAT_EVENTS);
    let refused = tracker.observe(&events, 0).unwrap_err();
    assert!(refused.contains("pending limit"), "{refused}");

    let mut tracker = PendingTracker::new();
    tracker
        .observe(&events[..MAX_PENDING_SEAT_EVENTS], 0)
        .unwrap();
    tracker
        .observe(&events, 1)
        .expect("an acknowledged prefix frees room");
    assert_eq!(tracker.len(), MAX_PENDING_SEAT_EVENTS);
}

/// The encoded size of unsaved events is bounded independently of their count.
#[test]
fn the_pending_byte_size_is_bounded() {
    let large = Value::String("x".repeat(300_000));
    let events = [facet(0, large.clone()), facet(1, large)];

    let mut tracker = PendingTracker::new();
    tracker
        .observe(&events[..1], 0)
        .expect("one large event fits");
    let refused = tracker.observe(&events, 0).unwrap_err();
    assert!(refused.contains("pending limit"), "{refused}");
}
