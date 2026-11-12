//! Recovery after a seat was lost, on the deterministic platform: a retained
//! append is reconciled before the writer is observed, an append whose
//! outcome is unknown prevents any observation, and a runtime with no
//! installed core can still inspect the seat when it holds no producer.

mod common;

use std::rc::Rc;

use common::sim::{FakePlatform, Reply};
use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{
    BrowserSession, BrowserViewport, RyeOsCore, SeatEvent, SeatEventKind,
    UiBindingAttachmentCoordinate, UiSeatControlPredecessor,
};
use ryeos_client_runtime::ClientRuntime;
use ryeos_client_runtime::api::{SEAT_APPEND_PATH, SEAT_OPEN_PATH, SEAT_REPLAY_PATH};
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::history::SeatCoordinate;
use ryeos_client_runtime::platform::Store;
use ryeos_client_runtime::recovery::{
    ReconnectChoice, ensure_settled, ensure_settled_without_core, observe_for_reconnect,
    observe_without_core,
};
use ryeos_client_runtime::seat::{
    AppendFailure, RetryPolicy, SeatShared, SeatStart, send_append,
};
use serde_json::{Value, json};

const PRESENTATION: &str = "22222222-2222-4222-8222-222222222222";
const KEY: &str = "retained-key";

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
        KEY.to_owned(),
        0,
        1_000_000,
    );
    *shared.producer.borrow_mut() = Some(producer());
    shared
}

fn core() -> RyeOsCore {
    RyeOsCore::new(
        BrowserSession::default(),
        "presentation-1",
        BrowserViewport::default(),
        0,
    )
}

fn observation(kind: &str) -> Reply {
    let mut producer = json!({"kind": kind});
    if kind != "unclaimed" {
        producer["producer_incarnation"] = json!("c".repeat(64));
    }
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
            "producer": producer,
        })
        .to_string(),
    )
}

fn control() -> UiSeatControlPredecessor {
    UiSeatControlPredecessor::Control {
        event_hash: "d".repeat(64),
    }
}

/// A seat whose one append was sent and never answered: the request is
/// retained, delivery is blocked and the outcome is unknown. The producer is
/// then dropped, as it is after a lost seat.
fn seat_with_unknown_append(platform: &Rc<FakePlatform>) -> Rc<SeatShared<FakePlatform>> {
    let shared = seat(platform);
    platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Network]);
    let event = SeatEvent {
        seq: 0,
        producer_engine_seq: None,
        kind: SeatEventKind::Facet {
            key: "draft".to_owned(),
            value: json!(1),
        },
    };
    let outcome = platform.sim().run(send_append(
        &shared,
        &producer(),
        &[event],
        RetryPolicy::WEB,
    ));
    assert!(matches!(outcome, Err(AppendFailure::Unknown(_))), "{outcome:?}");
    assert!(shared.delivery.borrow().blocked);
    assert!(shared.delivery.borrow().pending.is_some());
    assert!(platform.store.get(KEY).is_some(), "the request is retained");
    *shared.producer.borrow_mut() = None;
    shared
}

fn observe_body() -> Value {
    json!({
        "action": "observe",
        "presentation_id": PRESENTATION,
        "thread_id": "seat-thread",
        "chain_root_id": "chain-1",
    })
}

fn bodies(platform: &FakePlatform, path: &str) -> Vec<Value> {
    platform
        .transport
        .calls_to(path)
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect()
}

/// With an unresolved append of unknown outcome the daemon is asked for the
/// receipt of exactly that operation; if that cannot be settled, the seat is
/// never observed, so no replacement choice is offered, and the retained
/// request stays.
#[test]
fn an_unknown_append_prevents_observation_without_a_core() {
    let platform = FakePlatform::new();
    let shared = seat_with_unknown_append(&platform);
    let record = shared.delivery.borrow().pending.clone().unwrap().record;
    platform
        .transport
        .script(SEAT_REPLAY_PATH, vec![Reply::Network]);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation("active")]);

    let outcome = platform
        .sim()
        .run(observe_without_core(&shared, RetryPolicy::WEB));
    assert!(matches!(outcome, Err(RuntimeError::Fatal(_))), "{outcome:?}");

    assert!(platform.transport.calls_to(SEAT_OPEN_PATH).is_empty());
    let lookups = bodies(&platform, SEAT_REPLAY_PATH);
    assert_eq!(lookups.len(), 1);
    assert_eq!(lookups[0]["presentation_id"], PRESENTATION);
    assert_eq!(lookups[0]["chain_root_id"], "chain-1");
    let receipt = &lookups[0]["receipt"];
    assert_eq!(receipt["thread_id"], "seat-thread");
    assert_eq!(receipt["producer_incarnation"], "c".repeat(64));
    assert_eq!(receipt["operation_id"], record.operation_id);
    assert_eq!(receipt["first_engine_seq"], 0);
    assert_eq!(receipt["last_engine_seq"], 0);
    assert_eq!(receipt["event_count"], 1);
    assert_eq!(receipt["payload_digest"], record.payload_digest);
    assert!(platform.store.get(KEY).is_some(), "recovery stays retained");
}

/// The same holds through a core: reconciliation is attempted first and its
/// failure ends the observation before the seat is read.
#[test]
fn reconciliation_precedes_and_gates_observation_with_a_core() {
    let platform = FakePlatform::new();
    let shared = seat_with_unknown_append(&platform);
    platform
        .transport
        .script(SEAT_REPLAY_PATH, vec![Reply::Network]);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation("active")]);

    let mut core = core();
    let outcome = platform
        .sim()
        .run(observe_for_reconnect(&shared, &mut core, RetryPolicy::WEB));
    assert!(outcome.is_err());
    assert_eq!(platform.transport.calls_to(SEAT_REPLAY_PATH).len(), 1);
    assert!(platform.transport.calls_to(SEAT_OPEN_PATH).is_empty());
    assert!(platform.store.get(KEY).is_some());
}

/// A receipt lookup that is not exactly `found` with a receipt or `not_found`
/// with none, or that the daemon refuses, settles nothing: the observation is
/// not made and the retained request is kept.
#[test]
fn an_unconfirmed_receipt_blocks_observation_and_keeps_the_record() {
    let replies = [
        Reply::Ok(json!({"result": {"receipt_status": "found"}}).to_string()),
        Reply::Ok(json!({"receipt_status": "found", "receipt": null}).to_string()),
        Reply::Ok(json!({"receipt_status": "mystery", "receipt": null}).to_string()),
        Reply::Ok(
            json!({"receipt_status": "not_found", "receipt": null, "extra": 1}).to_string(),
        ),
        Reply::Status(404),
    ];
    for reply in replies {
        let platform = FakePlatform::new();
        let shared = seat_with_unknown_append(&platform);
        platform
            .transport
            .script(SEAT_REPLAY_PATH, vec![reply.clone()]);
        platform
            .transport
            .script(SEAT_OPEN_PATH, vec![observation("active")]);

        let outcome = platform
            .sim()
            .run(observe_without_core(&shared, RetryPolicy::WEB));
        assert!(outcome.is_err(), "{reply:?}: {outcome:?}");
        assert!(
            platform.transport.calls_to(SEAT_OPEN_PATH).is_empty(),
            "{reply:?}"
        );
        assert!(platform.store.get(KEY).is_some(), "{reply:?}");
        assert_eq!(
            platform.transport.calls_to(SEAT_APPEND_PATH).len(),
            5,
            "{reply:?}: no append is resent for an unconfirmed lookup"
        );
    }
}

/// A runtime that holds no live producer and has nothing retained reads the
/// seat directly: no receipt lookup is made, the observation names the choices
/// its producer state supports and no core is involved.
#[test]
fn observing_without_a_core_needs_no_receipt_when_nothing_is_retained() {
    for (kind, expected) in [
        ("active", vec!["takeover"]),
        ("relinquished", vec!["resume", "takeover"]),
        ("closed", vec![]),
    ] {
        let platform = FakePlatform::new();
        let shared = seat(&platform);
        *shared.producer.borrow_mut() = None;
        platform
            .transport
            .script(SEAT_OPEN_PATH, vec![observation(kind)]);

        let (observed, choices) = platform
            .sim()
            .run(observe_without_core(&shared, RetryPolicy::WEB))
            .expect("an observation");
        assert_eq!(observed.control_predecessor, control());
        let words: Vec<&str> = choices
            .iter()
            .map(|choice| match choice {
                ReconnectChoice::Resume { .. } => "resume",
                ReconnectChoice::Takeover { .. } => "takeover",
                ReconnectChoice::Independent => "independent",
            })
            .collect();
        assert_eq!(words, expected, "{kind}");
        assert!(platform.transport.calls_to(SEAT_REPLAY_PATH).is_empty());
        assert_eq!(bodies(&platform, SEAT_OPEN_PATH), vec![observe_body()]);
    }
}

/// Through a core the same direct read is made, and no effects come back from
/// an empty reconciliation.
#[test]
fn observing_with_a_core_reconciles_nothing_when_nothing_is_retained() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![observation("active")]);

    let mut core = core();
    let (_, choices, effects) = platform
        .sim()
        .run(observe_for_reconnect(&shared, &mut core, RetryPolicy::WEB))
        .expect("an observation");
    assert!(effects.is_empty());
    assert_eq!(
        choices,
        vec![ReconnectChoice::Takeover {
            expected_producer_incarnation: "c".repeat(64),
            predecessor: control(),
        }]
    );
    assert!(platform.transport.calls_to(SEAT_REPLAY_PATH).is_empty());
    assert_eq!(platform.transport.calls_to(SEAT_OPEN_PATH).len(), 1);
}

/// A runtime that still holds a live producer must settle through its core;
/// the no-core path refuses it before sending anything.
#[test]
fn observing_without_a_core_refuses_a_live_producer() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    let outcome = platform
        .sim()
        .run(observe_without_core(&shared, RetryPolicy::WEB));
    assert!(matches!(outcome, Err(RuntimeError::Fatal(_))), "{outcome:?}");
    assert!(platform.transport.calls.borrow().is_empty());
}

/// Observation needs a prepared presentation to name.
#[test]
fn observing_needs_a_retained_presentation() {
    let platform = FakePlatform::new();
    let shared = SeatShared::new(platform.clone());
    let outcome = platform
        .sim()
        .run(observe_without_core(&shared, RetryPolicy::WEB));
    assert!(matches!(outcome, Err(RuntimeError::Fatal(_))), "{outcome:?}");
    let mut core = core();
    let outcome = platform
        .sim()
        .run(observe_for_reconnect(&shared, &mut core, RetryPolicy::WEB));
    assert!(matches!(outcome, Err(RuntimeError::Fatal(_))), "{outcome:?}");
    assert!(platform.transport.calls.borrow().is_empty());
}

/// A replacement may start only when no append is unresolved and no record is
/// retained, with or without a core.
#[test]
fn a_replacement_waits_for_the_seat_to_settle() {
    let platform = FakePlatform::new();
    let shared = seat(&platform);
    ensure_settled_without_core(&shared).expect("a clean seat is settled");
    ensure_settled(&shared, &core()).expect("a clean seat is settled");

    // A retained record alone unsettles the seat.
    platform.store.set(KEY, "{}").unwrap();
    assert!(ensure_settled_without_core(&shared).is_err());
    assert!(ensure_settled(&shared, &core()).is_err());
    platform.store.remove(KEY);
    ensure_settled_without_core(&shared).expect("settled again");

    // So does an unresolved append, which also leaves its record behind.
    let platform = FakePlatform::new();
    let shared = seat_with_unknown_append(&platform);
    assert!(ensure_settled_without_core(&shared).is_err());
    assert!(ensure_settled(&shared, &core()).is_err());
    platform.store.remove(KEY);
    assert!(
        ensure_settled_without_core(&shared).is_err(),
        "the unresolved append itself still unsettles the seat"
    );
}

/// A runtime whose start failed before any seat was prepared has nothing to
/// reconnect: an independent presentation is still available, but a choice
/// that names the existing presentation is not.
#[test]
fn a_runtime_without_a_prepared_presentation_offers_only_an_independent_start() {
    let platform = FakePlatform::new();
    let mut runtime = ClientRuntime::new(platform.clone(), Rc::new(|| {}), RetryPolicy::WEB);
    assert!(!runtime.has_prepared_presentation());

    assert_eq!(
        runtime.reconnect_mode_without_core(&ReconnectChoice::Independent),
        Ok(SeatStart::Fresh)
    );
    let takeover = ReconnectChoice::Takeover {
        expected_producer_incarnation: "c".repeat(64),
        predecessor: control(),
    };
    assert!(matches!(
        runtime.reconnect_mode_without_core(&takeover),
        Err(RuntimeError::Fatal(_))
    ));

    let outcome = platform.sim().run(runtime.observe_reconnect_without_core());
    assert!(matches!(outcome, Err(RuntimeError::Fatal(_))), "{outcome:?}");
    assert!(platform.transport.calls.borrow().is_empty());
}
