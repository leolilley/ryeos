//! Observe and relinquish on the deterministic platform: the exact request
//! fields that go on the wire, and the acknowledgement that must echo them.

mod common;

use std::rc::Rc;

use common::sim::{FakePlatform, Reply};
use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{
    UiBindingAttachmentCoordinate, UiSeatControlPredecessor, UiSeatControlRequest,
};
use ryeos_client_runtime::api::{SEAT_CLOSE_PATH, SEAT_OPEN_PATH};
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::history::SeatCoordinate;
use ryeos_client_runtime::seat::{
    SeatShared, random_uuid, relinquish_claimed_seat, relinquish_current,
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

/// The acknowledgement of `request`, with one field replaced.
fn acknowledgement(request: &Value, field: &str, value: Value) -> Reply {
    let mut ack = json!({
        "presentation_id": request["presentation_id"],
        "thread_id": request["thread_id"],
        "producer_incarnation": request["producer_incarnation"],
        "operation_id": request["operation_id"],
        "state": "relinquished",
    });
    ack[field] = value;
    Reply::Ok(ack.to_string())
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

fn sent(platform: &FakePlatform, path: &str) -> Vec<Value> {
    platform
        .transport
        .calls_to(path)
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect()
}

/// A relinquish carries exactly the seat coordinate, the observed control
/// predecessor and the producer being given up, under a derived operation id
/// and a fresh intent nonce; nothing else. The observation before it names
/// only the seat.
#[test]
fn relinquish_sends_exactly_the_required_fields() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    let request = expected_request(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation(&"c".repeat(64))]);
    platform.transport.script(
        SEAT_CLOSE_PATH,
        vec![acknowledgement(&request, "state", json!("relinquished"))],
    );

    let outcome = platform.sim().run(relinquish_current(&shared));
    assert!(outcome.is_ok(), "{outcome:?}");

    assert_eq!(
        sent(&platform, SEAT_OPEN_PATH),
        vec![json!({
            "action": "observe",
            "presentation_id": PRESENTATION,
            "thread_id": "seat-thread",
            "chain_root_id": "chain-1",
        })]
    );
    let bodies = sent(&platform, SEAT_CLOSE_PATH);
    assert_eq!(bodies, vec![request.clone()]);
    let mut keys: Vec<&str> = request
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "action",
            "chain_root_id",
            "intent_nonce",
            "operation_id",
            "predecessor",
            "presentation_id",
            "producer_incarnation",
            "schema",
            "thread_id",
        ]
    );
    assert_eq!(request["action"], "relinquish");
    assert_eq!(request["schema"], 2);
    assert_eq!(request["producer_incarnation"], "c".repeat(64));
    assert_eq!(
        request["predecessor"],
        json!({"kind": "control", "event_hash": "d".repeat(64)})
    );

    // The writer is given up locally and stays fenced.
    assert!(shared.current_producer().is_none());
    assert!(shared.delivery.borrow().blocked);
}

/// An acknowledgement that does not echo the request exactly, or does not say
/// `relinquished`, is not accepted as a relinquishment.
#[test]
fn relinquish_requires_an_exact_acknowledgement() {
    let wrong: [(&str, Value); 5] = [
        ("producer_incarnation", json!("e".repeat(64))),
        (
            "operation_id",
            json!("33333333-3333-8333-8333-333333333333"),
        ),
        ("presentation_id", json!("another")),
        ("thread_id", json!("another")),
        ("state", json!("closed")),
    ];
    for (field, value) in wrong {
        let platform = FakePlatform::new();
        let shared = seat(&platform);
        let request = expected_request(&platform);
        platform
            .transport
            .script(SEAT_OPEN_PATH, vec![observation(&"c".repeat(64))]);
        platform.transport.script(
            SEAT_CLOSE_PATH,
            vec![acknowledgement(&request, field, value)],
        );

        let outcome = platform.sim().run(relinquish_current(&shared));
        assert!(
            matches!(outcome, Err(RuntimeError::UnreadableResponse { .. })),
            "{field}: {outcome:?}"
        );
    }
}

/// When the observed active writer is not this producer, nothing is sent to
/// give it up.
#[test]
fn relinquish_refuses_a_writer_that_is_not_the_observed_one() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation(&"e".repeat(64))]);

    let outcome = platform.sim().run(relinquish_current(&shared));
    assert!(
        matches!(outcome, Err(RuntimeError::Fatal(_))),
        "{outcome:?}"
    );
    assert!(platform.transport.calls_to(SEAT_CLOSE_PATH).is_empty());

    // A claimed-but-unadopted seat is simply left alone.
    let platform = FakePlatform::new();
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation(&"e".repeat(64))]);
    let outcome = platform.sim().run(relinquish_claimed_seat(
        &*platform,
        &producer(),
        &coordinate(),
        &root(),
    ));
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(platform.transport.calls_to(SEAT_CLOSE_PATH).is_empty());
}

/// A failed observation sends no control request and lifts the local fence
/// again, so an acknowledged writer is not stranded.
#[test]
fn a_failed_observation_restores_the_writer() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![Reply::Status(404)]);

    let outcome = platform.sim().run(relinquish_current(&shared));
    assert!(outcome.is_err());
    assert!(platform.transport.calls_to(SEAT_CLOSE_PATH).is_empty());
    assert_eq!(shared.current_producer(), Some(producer()));
    assert!(!shared.delivery.borrow().blocked);
}

/// Relinquishing is refused, without any request, while delivery is blocked.
#[test]
fn relinquish_waits_for_the_seat_to_settle() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    shared.delivery.borrow_mut().blocked = true;

    let outcome = platform.sim().run(relinquish_current(&shared));
    assert!(
        matches!(outcome, Err(RuntimeError::Fatal(_))),
        "{outcome:?}"
    );
    assert!(platform.transport.calls.borrow().is_empty());
    assert_eq!(shared.current_producer(), Some(producer()));
}
