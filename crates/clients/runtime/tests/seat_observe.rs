//! What a read-only producer observation must prove before the runtime uses
//! it: the answer describes exactly the asked seat and root binding, and its
//! control predecessor agrees with its producer state.

mod common;

use common::sim::{FakePlatform, Reply};
use ryeos_client_base::ui::UiBindingAttachmentCoordinate;
use ryeos_client_base::ui::UiObservedPresentationProducer;
use ryeos_client_runtime::api::SEAT_OPEN_PATH;
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::history::SeatCoordinate;
use ryeos_client_runtime::seat::observe_producer;
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

/// A well-formed observation of an active writer.
fn active() -> Value {
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
        "producer": {"kind": "active", "producer_incarnation": "c".repeat(64)},
    })
}

fn observe(
    answer: Reply,
) -> (
    Result<ryeos_client_base::ui::UiPresentationProducerObservation, RuntimeError>,
    std::rc::Rc<FakePlatform>,
) {
    let platform = FakePlatform::new();
    platform.transport.script(SEAT_OPEN_PATH, vec![answer]);
    let outcome = platform
        .sim()
        .run(observe_producer(&*platform, &coordinate(), &root()));
    (outcome, platform)
}

fn refused_with(answer: Value, fragment: &str) {
    let (outcome, _) = observe(Reply::Ok(answer.to_string()));
    match outcome {
        Err(RuntimeError::Fatal(message)) => {
            assert!(message.contains(fragment), "{message} lacks {fragment}");
        }
        other => panic!("expected a refusal containing {fragment:?}, got {other:?}"),
    }
}

/// The answer may arrive bare or under one or two `result` layers.
#[test]
fn an_exact_observation_is_accepted_in_any_envelope() {
    for answer in [
        active(),
        json!({"result": active()}),
        json!({"result": {"result": active()}}),
    ] {
        let (outcome, _) = observe(Reply::Ok(answer.to_string()));
        let observation = outcome.expect("an exact observation");
        assert_eq!(
            observation.producer,
            UiObservedPresentationProducer::Active {
                producer_incarnation: "c".repeat(64)
            }
        );
    }

    // A seat nobody has claimed is described by its initialization.
    let mut unclaimed = active();
    unclaimed["producer"] = json!({"kind": "unclaimed"});
    unclaimed["control_predecessor"] =
        json!({"kind": "initialization", "event_hash": "d".repeat(64)});
    let (outcome, _) = observe(Reply::Ok(unclaimed.to_string()));
    assert_eq!(
        outcome.expect("an unclaimed seat").producer,
        UiObservedPresentationProducer::Unclaimed
    );
}

/// An answer about another seat is refused, whichever coordinate differs.
#[test]
fn an_observation_of_another_seat_is_refused() {
    for field in ["presentation_id", "thread_id", "chain_root_id"] {
        let mut answer = active();
        answer[field] = json!("another");
        refused_with(answer, "differs from its exact seat or control state");
    }
}

/// An unclaimed seat must carry its initialization as predecessor, and any
/// claimed state must carry a control event.
#[test]
fn the_predecessor_must_agree_with_the_producer_state() {
    let mut unclaimed_with_control = active();
    unclaimed_with_control["producer"] = json!({"kind": "unclaimed"});
    refused_with(
        unclaimed_with_control,
        "differs from its exact seat or control state",
    );

    for state in ["active", "relinquished", "closed"] {
        let mut answer = active();
        answer["producer"] = json!({"kind": state, "producer_incarnation": "c".repeat(64)});
        answer["control_predecessor"] =
            json!({"kind": "initialization", "event_hash": "d".repeat(64)});
        refused_with(answer, "differs from its exact seat or control state");
    }
}

/// The root binding is compared field by field with the admitted attachment.
#[test]
fn an_observation_under_another_root_binding_is_refused() {
    for (field, value) in [
        ("binding_attachment_id", json!("other")),
        ("binding_generation", json!(2)),
        ("binding_digest", json!("other")),
    ] {
        let mut answer = active();
        answer["root_binding"][field] = value;
        refused_with(
            answer,
            "prepared seat root binding differs from admitted attachment",
        );
    }
}

/// The predecessor must be an exact lowercase 64-hex event hash.
#[test]
fn a_noncanonical_predecessor_hash_is_refused() {
    for hash in ["d".repeat(63), "D".repeat(64), String::new()] {
        let mut answer = active();
        answer["control_predecessor"]["event_hash"] = json!(hash);
        refused_with(answer, "control predecessor must name an exact canonical");
    }
}

/// The record has a closed schema: extra fields, unknown producer states and
/// missing fields are refused before any coordinate is compared.
#[test]
fn a_malformed_observation_record_is_refused() {
    let mut extra = active();
    extra["extra"] = json!(true);
    refused_with(extra, "invalid exact producer observation");

    let mut unknown_state = active();
    unknown_state["producer"] = json!({"kind": "paused"});
    refused_with(unknown_state, "invalid exact producer observation");

    let mut missing = active();
    missing.as_object_mut().unwrap().remove("revision");
    refused_with(missing, "invalid exact producer observation");

    let mut active_without_incarnation = active();
    active_without_incarnation["producer"] = json!({"kind": "active"});
    refused_with(
        active_without_incarnation,
        "invalid exact producer observation",
    );

    refused_with(json!([1]), "non-object");
}

/// An observation that cannot be delivered or read is retried with the
/// identical bytes on the bounded schedule, then reported as unknown; a
/// definite refusal is not retried.
#[test]
fn observation_retries_identical_bytes_only_when_delivery_is_unknown() {
    let (outcome, platform) = observe(Reply::Network);
    assert!(
        matches!(outcome, Err(RuntimeError::Network { .. })),
        "{outcome:?}"
    );
    let bodies = platform.transport.calls_to(SEAT_OPEN_PATH);
    assert_eq!(bodies.len(), 5);
    assert!(bodies.iter().all(|body| *body == bodies[0]));
    assert_eq!(*platform.clock.sleeps.borrow(), vec![100, 200, 400, 800]);

    let (outcome, platform) = observe(Reply::Unreadable);
    assert!(
        matches!(outcome, Err(RuntimeError::UnreadableResponse { .. })),
        "{outcome:?}"
    );
    assert_eq!(platform.transport.calls_to(SEAT_OPEN_PATH).len(), 5);

    let (outcome, platform) = observe(Reply::Status(503));
    assert!(matches!(
        outcome,
        Err(RuntimeError::Daemon { status: 503, .. })
    ));
    assert_eq!(platform.transport.calls_to(SEAT_OPEN_PATH).len(), 5);

    let (outcome, platform) = observe(Reply::Status(404));
    assert!(matches!(
        outcome,
        Err(RuntimeError::Daemon { status: 404, .. })
    ));
    assert_eq!(platform.transport.calls_to(SEAT_OPEN_PATH).len(), 1);
    assert!(platform.clock.sleeps.borrow().is_empty());
}
