//! Presence renewal and giving the seat back on the deterministic platform:
//! how every touch answer is classified, that a touch names only the current
//! producer, that a relinquish resends one immutable request, and that local
//! mutation stays fenced while the writer is being observed. The flows that
//! need the runtime's private state (overlapping ticks, folding a late
//! refusal) live beside it in `src/runtime/tests.rs`.

mod common;

use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use common::sim::{FakePlatform, Reply};
use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{
    SeatEvent, SeatEventKind, UiBindingAttachmentCoordinate, UiSeatControlPredecessor,
    UiSeatControlRequest,
};
use ryeos_client_runtime::api::{
    SEAT_APPEND_PATH, SEAT_CLOSE_PATH, SEAT_OPEN_PATH, SEAT_TOUCH_PATH,
};
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::history::SeatCoordinate;
use ryeos_client_runtime::seat::{
    AppendFailure, RetryPolicy, SeatShared, TouchOutcome, random_uuid, relinquish_current,
    send_append, touch_seat,
};
use serde_json::{Value, json};

const PRESENTATION: &str = "22222222-2222-4222-8222-222222222222";

fn coordinate() -> SeatCoordinate {
    SeatCoordinate {
        presentation_id: PRESENTATION.to_owned(),
        thread_id: "seat-thread".to_owned(),
        chain_root_id: "chain-1".to_owned(),
    }
}

fn root() -> UiBindingAttachmentCoordinate {
    UiBindingAttachmentCoordinate {
        binding_attachment_id: "root".to_owned(),
        binding_generation: 1,
        binding_digest: "digest".to_owned(),
    }
}

fn producer() -> UiPresentationProducer {
    UiPresentationProducer {
        presentation_id: PRESENTATION.to_owned(),
        thread_id: "seat-thread".to_owned(),
        producer_incarnation: "c".repeat(64),
    }
}

fn seat(platform: &Rc<FakePlatform>) -> Rc<SeatShared<FakePlatform>> {
    let shared = SeatShared::new(platform.clone());
    shared.delivery.borrow_mut().reset_for(
        coordinate(),
        "session",
        root(),
        "retained-key".to_owned(),
        0,
        1_000_000,
    );
    *shared.producer.borrow_mut() = Some(producer());
    shared
}

fn touched() -> Value {
    json!({
        "presentation_id": PRESENTATION,
        "thread_id": "seat-thread",
        "touched": true,
    })
}

/// The daemon's answer to `observe`: `incarnation` is the active writer.
fn observation(incarnation: &str) -> Reply {
    Reply::Ok(
        json!({
            "presentation_id": PRESENTATION,
            "thread_id": "seat-thread",
            "chain_root_id": "chain-1",
            "root_binding": {
                "binding_attachment_id": "root",
                "binding_generation": 1,
                "binding_digest": "digest",
            },
            "revision": 1,
            "control_predecessor": {"kind": "control", "event_hash": "d".repeat(64)},
            "producer": {"kind": "active", "producer_incarnation": incarnation},
        })
        .to_string(),
    )
}

/// The relinquish request the runtime must send for the observation above,
/// rebuilt from the platform's deterministic entropy.
fn expected_request(platform: &FakePlatform) -> Value {
    let mut request = UiSeatControlRequest::Relinquish {
        schema: 2,
        operation_id: String::new(),
        presentation_id: PRESENTATION.to_owned(),
        thread_id: "seat-thread".to_owned(),
        chain_root_id: "chain-1".to_owned(),
        predecessor: UiSeatControlPredecessor::Control {
            event_hash: "d".repeat(64),
        },
        intent_nonce: random_uuid(platform),
        producer_incarnation: "c".repeat(64),
    };
    request.assign_identity().unwrap();
    serde_json::to_value(&request).unwrap()
}

fn relinquished(request: &Value) -> Reply {
    Reply::Ok(
        json!({
            "presentation_id": request["presentation_id"],
            "thread_id": request["thread_id"],
            "producer_incarnation": request["producer_incarnation"],
            "operation_id": request["operation_id"],
            "state": "relinquished",
        })
        .to_string(),
    )
}

fn facet() -> SeatEvent {
    SeatEvent {
        seq: 0,
        producer_engine_seq: None,
        kind: SeatEventKind::Facet {
            key: "draft".to_owned(),
            value: json!(1),
        },
    }
}

/// A touch sends exactly the producer's three identity fields and renews only
/// on the exact acknowledgement; a definite loss refuses, and anything else
/// is retried without fencing. The touch itself never changes seat state.
#[test]
fn a_touch_sends_the_producer_and_classifies_every_answer() {
    let wrapped = json!({"result": touched()}).to_string();
    let mut false_ack = touched();
    false_ack["touched"] = json!(false);
    let mut extra = touched();
    extra["lease"] = json!(1);
    let mut other_thread = touched();
    other_thread["thread_id"] = json!("another");

    let cases: Vec<(&str, Reply, TouchOutcome)> = vec![
        (
            "exact",
            Reply::Ok(touched().to_string()),
            TouchOutcome::Renewed,
        ),
        ("wrapped", Reply::Ok(wrapped), TouchOutcome::Renewed),
        (
            "not touched",
            Reply::Ok(false_ack.to_string()),
            TouchOutcome::Retry,
        ),
        (
            "extra field",
            Reply::Ok(extra.to_string()),
            TouchOutcome::Retry,
        ),
        (
            "another seat",
            Reply::Ok(other_thread.to_string()),
            TouchOutcome::Retry,
        ),
        ("server error", Reply::Status(500), TouchOutcome::Retry),
        ("lost network", Reply::Network, TouchOutcome::Retry),
        ("unreadable", Reply::Unreadable, TouchOutcome::Retry),
        ("forbidden", Reply::Status(403), TouchOutcome::Refused),
        ("not found", Reply::Status(404), TouchOutcome::Refused),
        ("conflict", Reply::Status(409), TouchOutcome::Refused),
        ("gone", Reply::Status(410), TouchOutcome::Refused),
    ];
    for (name, reply, expected) in cases {
        let platform = FakePlatform::new();
        let shared = seat(&platform);
        platform.transport.script(SEAT_TOUCH_PATH, vec![reply]);

        let outcome = platform.sim().run(touch_seat(&shared, &producer()));
        assert_eq!(outcome, expected, "{name}");

        let sent = platform.transport.calls_to(SEAT_TOUCH_PATH);
        assert_eq!(sent.len(), 1, "{name}");
        let body: Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(
            body,
            json!({
                "presentation_id": PRESENTATION,
                "thread_id": "seat-thread",
                "producer_incarnation": "c".repeat(64),
            }),
            "{name}"
        );
        assert_eq!(shared.current_producer(), Some(producer()), "{name}");
        assert!(!shared.delivery.borrow().blocked, "{name}");
    }
}

/// A touch for a producer that is no longer the current one is refused
/// locally, without a request, and leaves the current producer (the
/// successor) untouched.
#[test]
fn a_touch_for_a_replaced_producer_sends_nothing_and_spares_the_successor() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    let successor = UiPresentationProducer {
        producer_incarnation: "d".repeat(64),
        ..producer()
    };
    *shared.producer.borrow_mut() = Some(successor.clone());

    let outcome = platform.sim().run(touch_seat(&shared, &producer()));
    assert_eq!(outcome, TouchOutcome::Refused);
    assert!(platform.transport.calls.borrow().is_empty());
    assert_eq!(shared.current_producer(), Some(successor));
    assert!(!shared.delivery.borrow().blocked);
}

/// A lost relinquish reply is resent byte-for-byte: one operation id and one
/// intent nonce for the whole retry. The writer stays fenced locally until
/// the acknowledgement is verified.
#[test]
fn an_unknown_relinquish_resends_one_immutable_request() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    let request = expected_request(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation(&"c".repeat(64))]);
    platform.transport.script(
        SEAT_CLOSE_PATH,
        vec![Reply::Network, Reply::Status(502), relinquished(&request)],
    );

    let outcome = platform.sim().run(relinquish_current(&shared));
    assert!(outcome.is_ok(), "{outcome:?}");

    let sent = platform.transport.calls_to(SEAT_CLOSE_PATH);
    assert_eq!(sent.len(), 3);
    assert!(
        sent.iter().all(|body| *body == sent[0]),
        "every resend is the identical body"
    );
    let body: Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(body, request);
    assert_eq!(body["intent_nonce"], random_uuid(&*platform));
    assert_eq!(platform.transport.calls_to(SEAT_OPEN_PATH).len(), 1);
    assert_eq!(*platform.clock.sleeps.borrow(), vec![100, 200]);
    assert!(shared.current_producer().is_none());
    assert!(shared.delivery.borrow().blocked);
}

/// When no resend is ever answered the retry stops after the bounded
/// attempts with the same bytes each time, and the writer stays fenced: a
/// request that may have been applied must not be followed by more writes.
#[test]
fn a_relinquish_that_is_never_answered_stays_fenced() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation(&"c".repeat(64))]);
    platform
        .transport
        .script(SEAT_CLOSE_PATH, vec![Reply::Network]);

    let outcome = platform.sim().run(relinquish_current(&shared));
    assert!(
        matches!(outcome, Err(RuntimeError::Network { .. })),
        "{outcome:?}"
    );

    let sent = platform.transport.calls_to(SEAT_CLOSE_PATH);
    assert_eq!(sent.len(), 5);
    assert!(sent.iter().all(|body| *body == sent[0]));
    assert_eq!(*platform.clock.sleeps.borrow(), vec![100, 200, 400, 800]);
    assert!(shared.current_producer().is_none());
    assert!(shared.delivery.borrow().blocked);
}

/// A definite refusal of the relinquish is not retried.
#[test]
fn a_definite_relinquish_refusal_is_not_retried() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation(&"c".repeat(64))]);
    platform
        .transport
        .script(SEAT_CLOSE_PATH, vec![Reply::Status(409)]);

    let outcome = platform.sim().run(relinquish_current(&shared));
    assert!(
        matches!(outcome, Err(RuntimeError::Daemon { status: 409, .. })),
        "{outcome:?}"
    );
    assert_eq!(platform.transport.calls_to(SEAT_CLOSE_PATH).len(), 1);
    assert!(platform.clock.sleeps.borrow().is_empty());
    assert!(shared.current_producer().is_none());
    assert!(shared.delivery.borrow().blocked);
}

/// While the writer is being observed for relinquishment the producer is
/// already gone locally: an append or a touch under it is refused without a
/// request, so nothing can be written between the freeze and the control
/// request. The observation and the control request are each sent once per
/// attempt, with identical bytes.
#[test]
fn local_mutation_stays_fenced_while_the_observation_is_awaited() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    let request = expected_request(&platform);
    platform.transport.script(
        SEAT_OPEN_PATH,
        vec![Reply::Network, observation(&"c".repeat(64))],
    );
    platform
        .transport
        .script(SEAT_CLOSE_PATH, vec![relinquished(&request)]);

    let mut relinquish = pin!(relinquish_current(&shared));
    let mut context = Context::from_waker(Waker::noop());
    // The first observation is lost; the retry waits on its backoff timer.
    assert!(relinquish.as_mut().poll(&mut context).is_pending());
    assert!(shared.current_producer().is_none());
    assert!(shared.delivery.borrow().blocked);

    let sent_so_far = platform.transport.calls.borrow().len();
    let appended = platform.sim().run(send_append(
        &shared,
        &producer(),
        &[facet()],
        RetryPolicy::WEB,
    ));
    assert!(
        matches!(appended, Err(AppendFailure::Refused(_))),
        "{appended:?}"
    );
    let touch = platform.sim().run(touch_seat(&shared, &producer()));
    assert_eq!(touch, TouchOutcome::Refused);
    assert_eq!(platform.transport.calls.borrow().len(), sent_so_far);
    assert!(platform.transport.calls_to(SEAT_APPEND_PATH).is_empty());
    assert!(platform.transport.calls_to(SEAT_TOUCH_PATH).is_empty());

    let outcome = loop {
        if let Poll::Ready(outcome) = relinquish.as_mut().poll(&mut context) {
            break outcome;
        }
        assert!(
            platform.clock.advance_to_next(None),
            "the relinquish waits on a timer"
        );
    };
    assert!(outcome.is_ok(), "{outcome:?}");
    let observed = platform.transport.calls_to(SEAT_OPEN_PATH);
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[0], observed[1]);
    assert_eq!(platform.transport.calls_to(SEAT_CLOSE_PATH).len(), 1);
    assert!(shared.current_producer().is_none());
}

/// An invalidation that fences the producer while its observation is awaited
/// holds: when the observation then fails, the writer is not restored.
#[test]
fn a_fence_during_the_observation_wait_is_not_undone_by_a_failed_observation() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![Reply::Network, Reply::Status(404)]);

    let mut relinquish = pin!(relinquish_current(&shared));
    let mut context = Context::from_waker(Waker::noop());
    assert!(relinquish.as_mut().poll(&mut context).is_pending());
    assert!(
        shared.fence(&producer()),
        "the producer being relinquished is fenced"
    );

    let outcome = loop {
        if let Poll::Ready(outcome) = relinquish.as_mut().poll(&mut context) {
            break outcome;
        }
        assert!(platform.clock.advance_to_next(None));
    };
    assert!(
        matches!(outcome, Err(RuntimeError::Daemon { status: 404, .. })),
        "{outcome:?}"
    );
    assert!(shared.current_producer().is_none());
    assert!(shared.delivery.borrow().blocked);
    assert!(platform.transport.calls_to(SEAT_CLOSE_PATH).is_empty());
}

/// After a relinquish whose outcome is unknown, asking again reports the
/// unresolved earlier attempt instead of success, and sends nothing.
#[test]
fn a_second_relinquish_after_an_unknown_outcome_reports_it_and_sends_nothing() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation(&"c".repeat(64))]);
    platform
        .transport
        .script(SEAT_CLOSE_PATH, vec![Reply::Network]);
    let first = platform.sim().run(relinquish_current(&shared));
    assert!(
        matches!(first, Err(RuntimeError::Network { .. })),
        "{first:?}"
    );
    let calls = platform.transport.calls.borrow().len();

    let second = platform.sim().run(relinquish_current(&shared));
    assert!(
        matches!(second, Err(RuntimeError::Fatal(ref message)) if message.contains("earlier relinquishment is unresolved")),
        "{second:?}"
    );
    assert_eq!(platform.transport.calls.borrow().len(), calls);
    assert!(shared.current_producer().is_none());
}
