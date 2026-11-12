//! Claiming and proving a seat on the deterministic platform: what a start
//! sends and in which order, what is frozen across retries within one
//! runtime, which replacement claims are refused before anything is sent, and
//! how an unfinished proof is retried. The scripted daemon answers are the
//! exact wire shapes; the runtime's requests are compared byte for byte or
//! field for field.

mod common;
mod seat_support;

use common::sim::{FakePlatform, Reply};
use ryeos_client_base::ui::{SeatEventKind, UiSeatControlPredecessor, UiSeatProducerClaim};
use ryeos_client_runtime::api::{
    SEAT_APPEND_PATH, SEAT_CLOSE_PATH, SEAT_OPEN_PATH, SEAT_REPLAY_PATH, SESSION_CURRENT_PATH,
};
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::platform::Store;
use ryeos_client_runtime::replay::CoreConfig;
use ryeos_client_runtime::retention::storage_key;
use ryeos_client_runtime::seat::{
    AppendFailure, ClaimIntent, RetainedClaim, RetryPolicy, SeatShared, SeatStart,
    claim_and_replay, send_append,
};
use seat_support::*;
use serde_json::{Value, json};

fn retained(claim: RetainedClaim) -> SeatStart {
    SeatStart::Retained {
        presentation_id: PRESENTATION.to_owned(),
        claim,
    }
}

fn control(position: u64) -> UiSeatControlPredecessor {
    UiSeatControlPredecessor::Control {
        event_hash: hash(position),
    }
}

/// A fresh start asks the daemon in a fixed order and installs the producer
/// only from the finished proof: prepare, observe, the one claim, then the
/// session is re-read before the history is opened and again before the core
/// is admitted. Every body is exactly what the claimed seat requires.
#[test]
fn a_fresh_start_prepares_observes_claims_and_proves_in_order() {
    let platform = FakePlatform::new();
    let world = FreshWorld::new(&platform);
    world.script(&platform);
    world.proof(&platform, true);
    let mut runtime = new_runtime(&platform);
    assert!(runtime.producer().is_none());

    let (core, _effects) =
        start_fresh(&platform, &mut runtime, &world).expect("the fresh start succeeds");

    assert_eq!(
        wire(&platform),
        [
            "open:prepare",
            "open:observe",
            "open:claim",
            "session",
            "history",
            "session"
        ]
    );
    let opens: Vec<Value> = raw(&platform, SEAT_OPEN_PATH)
        .iter()
        .map(|body| json_of(body))
        .collect();
    assert_eq!(
        opens[0],
        json!({
            "action": "prepare",
            "creation_request_id": world.creation,
            "binding_attachment_id": "root",
            "binding_generation": 1,
            "binding_digest": hex('a'),
        })
    );
    assert_eq!(
        opens[1],
        json!({
            "action": "observe",
            "presentation_id": PRESENTATION,
            "thread_id": THREAD,
            "chain_root_id": CHAIN,
        })
    );
    assert_eq!(opens[2], serde_json::to_value(&world.request).unwrap());
    assert_eq!(opens[2]["claim"], json!({"kind": "fresh"}));
    assert_eq!(
        opens[2]["predecessor"],
        json!({"kind": "initialization", "event_hash": hash(1)})
    );
    let history = raw(&platform, ryeos_client_runtime::api::SEAT_HISTORY_PATH);
    assert_eq!(
        json_of(&history[0]),
        json!({"presentation_id": PRESENTATION, "chain_root_id": CHAIN})
    );

    // The proof is exactly the initialization; the producer is the claimed
    // incarnation at the claimed frontier and delivery is open.
    let claimed = producer(&incarnation());
    assert_eq!(runtime.producer(), Some(claimed.clone()));
    let status = runtime.seat_status();
    assert!(status.ready && !status.blocked && !status.reproof_pending);
    core.require_presentation_producer_ready(&claimed, 0)
        .expect("the reducer admitted the claimed producer");
    assert_eq!(core.seat.events().len(), 1);
    assert_eq!(core.acknowledged_seat_prefix_len(), 1);
    assert!(runtime.has_prepared_presentation());
}

/// A prepare whose outcome stayed unknown is retried by the next start of the
/// same runtime with the identical bytes, so the daemon can never see two
/// different preparations from one intent; nothing after it was sent.
#[test]
fn an_unknown_prepare_is_retried_with_the_same_bytes() {
    let platform = FakePlatform::new();
    let world = FreshWorld::new(&platform);
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![Reply::Network]);
    let mut runtime = new_runtime(&platform);

    let first = start_fresh(&platform, &mut runtime, &world);
    assert!(matches!(first, Err(RuntimeError::Network { .. })));
    assert!(!runtime.has_prepared_presentation());

    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![Reply::Status(404)]);
    let second = start_fresh(&platform, &mut runtime, &world);
    assert!(matches!(second, Err(RuntimeError::Daemon { .. })));

    let bodies = raw(&platform, SEAT_OPEN_PATH);
    assert_eq!(bodies.len(), 6, "five delivery attempts, then the retry");
    assert!(
        bodies.iter().all(|body| *body == bodies[0]),
        "every send is the same bytes"
    );
    let first_body = json_of(&bodies[0]);
    assert_eq!(first_body["action"], "prepare");
    assert_eq!(first_body["creation_request_id"], world.creation);
    assert_eq!(wire(&platform), vec!["open:prepare"; 6]);
}

/// A claim whose acknowledgement never matched is resent, on the next start
/// of the same runtime, as the very same bytes and operation, without
/// preparing or observing again; when it is finally acknowledged the start
/// completes.
#[test]
fn an_unacknowledged_claim_is_resent_frozen_without_new_preparation() {
    let platform = FakePlatform::new();
    let world = FreshWorld::new(&platform);
    world.script(&platform);
    let mut wrong = claim_ack_value(&world.request, &incarnation(), 0);
    wrong["operation_id"] = json!("33333333-3333-8333-8333-333333333333");
    let wrong = Reply::Ok(wrong.to_string());
    platform.transport.script(
        SEAT_OPEN_PATH,
        vec![
            prepare_reply(),
            unclaimed(),
            wrong.clone(),
            wrong.clone(),
            wrong,
            claim_ack(&world.request, &incarnation(), 0),
            active(&incarnation()),
        ],
    );
    world.proof(&platform, true);
    let mut runtime = new_runtime(&platform);

    let first = start_fresh(&platform, &mut runtime, &world);
    let message = refusal(first);
    assert!(message.contains("claim outcome unresolved"), "{message}");
    assert_eq!(*platform.clock.sleeps.borrow(), vec![100, 200]);
    assert_eq!(open_bodies(&platform, "claim").len(), 3);
    assert_eq!(count(&platform, "history"), 0, "nothing was proven yet");
    assert!(runtime.producer().is_none());

    start_fresh(&platform, &mut runtime, &world).expect("the resent claim is acknowledged");

    let claims = open_bodies(&platform, "claim");
    assert_eq!(claims.len(), 4);
    assert!(claims.iter().all(|body| *body == claims[0]));
    assert_eq!(
        json_of(&claims[0])["operation_id"],
        world.request.operation_id()
    );
    assert_eq!(open_bodies(&platform, "prepare").len(), 1);
    assert_eq!(open_bodies(&platform, "observe").len(), 1);
    assert_eq!(runtime.producer(), Some(producer(&incarnation())));
}

/// Two runtimes never share an intent: each draws its own creation id and
/// claim nonce, so their preparations and claims (and the claim's derived
/// operation id) all differ.
#[test]
fn each_runtime_draws_its_own_presentation_and_claim_identity() {
    let platform = CountingPlatform::new();
    let before = session(None);
    let mut seen = Vec::new();
    for _ in 0..2 {
        platform.inner.transport.script(
            SEAT_OPEN_PATH,
            vec![prepare_reply(), unclaimed(), Reply::Status(404)],
        );
        platform.inner.transport.calls.borrow_mut().clear();
        let mut runtime = ryeos_client_runtime::ClientRuntime::new(
            platform.clone(),
            std::rc::Rc::new(|| {}),
            RetryPolicy::WEB,
        );
        let outcome = platform.sim().run(runtime.start(
            before.clone(),
            ryeos_client_base::ui::BrowserViewport::default(),
            SeatStart::Fresh,
            CoreConfig::default(),
        ));
        assert!(matches!(outcome, Err(RuntimeError::Daemon { .. })));
        let prepare = json_of(&open_bodies(&platform.inner, "prepare")[0]);
        let claim = json_of(&open_bodies(&platform.inner, "claim")[0]);
        seen.push((
            prepare["creation_request_id"].clone(),
            claim["intent_nonce"].clone(),
            claim["operation_id"].clone(),
        ));
    }
    assert_ne!(seen[0].0, seen[1].0, "creation ids differ");
    assert_ne!(seen[0].1, seen[1].1, "claim nonces differ");
    assert_ne!(seen[0].2, seen[1].2, "claim operations differ");
}

/// A replacement claim must carry an authenticated control predecessor and an
/// exact predecessor incarnation. One that names only the initialization, or
/// a malformed incarnation, is refused before anything is sent.
#[test]
fn a_replacement_without_an_authenticated_predecessor_sends_nothing() {
    let initialization = UiSeatControlPredecessor::Initialization {
        event_hash: hash(1),
    };
    let cases: Vec<(RetainedClaim, &str)> = vec![
        (
            RetainedClaim::Takeover {
                expected_producer_incarnation: hex('c'),
                predecessor: initialization.clone(),
            },
            "observed control predecessor",
        ),
        (
            RetainedClaim::Resume {
                expected_producer_incarnation: hex('c'),
                predecessor: initialization,
            },
            "observed control predecessor",
        ),
        (
            RetainedClaim::Takeover {
                expected_producer_incarnation: "short".to_owned(),
                predecessor: control(2),
            },
            "exact predecessor producer incarnation",
        ),
    ];
    for (claim, expected) in cases {
        let platform = FakePlatform::new();
        let mut runtime = new_runtime(&platform);
        let outcome = start_with(
            &platform,
            &mut runtime,
            &session(Some(RETAINED_CREATION)),
            retained(claim.clone()),
        );
        let message = refusal(outcome);
        assert!(message.contains(expected), "{claim:?}: {message}");
        assert!(
            platform.transport.calls.borrow().is_empty(),
            "{claim:?}: nothing was sent"
        );
    }
}

/// A resume that observes its predecessor takes the predecessor from that
/// observation and only if it is exactly relinquished; an active writer is
/// refused after the observation, with no claim sent.
#[test]
fn a_resume_claims_only_a_relinquished_writer_it_observed() {
    // Refused: the observed writer is active.
    let platform = FakePlatform::new();
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![active(&hex('c'))]);
    let mut runtime = new_runtime(&platform);
    let outcome = start_with(
        &platform,
        &mut runtime,
        &session(Some(RETAINED_CREATION)),
        retained(RetainedClaim::ResumeRelinquished),
    );
    let message = refusal(outcome);
    assert!(
        message.contains("exact Relinquished predecessor"),
        "{message}"
    );
    assert_eq!(wire(&platform), ["open:observe"]);

    // Accepted: the claim is built from the observation.
    let platform = FakePlatform::new();
    let expected = claim_request(
        &platform,
        control(2),
        UiSeatProducerClaim::Resume {
            expected_producer_incarnation: hex('c'),
        },
    );
    platform.transport.script(
        SEAT_OPEN_PATH,
        vec![relinquished(&hex('c')), claim_ack(&expected, &hex('d'), 1)],
    );
    platform.transport.script(
        SESSION_CURRENT_PATH,
        vec![session_reply(&session(Some(RETAINED_CREATION)))],
    );
    let mut runtime = new_runtime(&platform);
    // No history is scripted, so the proof stops after the claim.
    let outcome = start_with(
        &platform,
        &mut runtime,
        &session(Some(RETAINED_CREATION)),
        retained(RetainedClaim::ResumeRelinquished),
    );
    assert!(outcome.is_err());
    assert_eq!(
        wire(&platform),
        ["open:observe", "open:claim", "session", "history"]
    );
    assert_eq!(
        json_of(&open_bodies(&platform, "claim")[0]),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(
        count(&platform, "close"),
        0,
        "a retained seat is left to its lease"
    );
    assert!(runtime.producer().is_none());
}

/// A takeover that names the writer the operator saw claims with exactly that
/// predecessor and does not observe again. The proof replays the seat's whole
/// prior history (its initialization and the previous writer's facet), and
/// the new writer becomes current at its own frontier.
#[test]
fn a_takeover_keeps_its_selected_predecessor_and_replays_the_prior_history() {
    let platform = FakePlatform::new();
    let old = claim_request_with_nonce(
        "44444444-4444-4444-8444-444444444444",
        UiSeatControlPredecessor::Initialization {
            event_hash: hash(1),
        },
        UiSeatProducerClaim::Fresh,
    );
    let request = claim_request(
        &platform,
        control(2),
        UiSeatProducerClaim::Takeover {
            expected_producer_incarnation: hex('c'),
        },
    );
    let rows = vec![
        init_row(1, RETAINED_CREATION),
        claim_row(2, &old, &hex('c'), 0),
        facet_row(3, 0, "draft", json!(1)),
        claim_row(4, &request, &hex('d'), 1),
    ];
    platform
        .transport
        .script(SEAT_OPEN_PATH, vec![claim_ack(&request, &hex('d'), 1)]);
    platform.transport.script(
        SESSION_CURRENT_PATH,
        vec![session_reply(&session(Some(RETAINED_CREATION)))],
    );
    push_history(&platform, proof_chunks(&rows), true);
    let mut runtime = new_runtime(&platform);

    let (core, _effects) = start_with(
        &platform,
        &mut runtime,
        &session(Some(RETAINED_CREATION)),
        retained(RetainedClaim::Takeover {
            expected_producer_incarnation: hex('c'),
            predecessor: control(2),
        }),
    )
    .expect("the takeover succeeds");

    assert_eq!(
        wire(&platform),
        ["open:claim", "session", "history", "session"]
    );
    let claim = json_of(&open_bodies(&platform, "claim")[0]);
    assert_eq!(claim, serde_json::to_value(&request).unwrap());
    assert_eq!(
        claim["predecessor"],
        json!({"kind": "control", "event_hash": hash(2)})
    );
    assert_eq!(
        claim["claim"],
        json!({"kind": "takeover", "expected_producer_incarnation": hex('c')})
    );

    // The whole prior history reached the private core.
    let events = core.seat.events();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].kind,
        SeatEventKind::PresentationInitialized(_)
    ));
    assert!(matches!(&events[1].kind, SeatEventKind::Facet { key, .. } if key == "draft"));
    assert_eq!(core.acknowledged_seat_prefix_len(), 2);

    let successor = producer(&hex('d'));
    assert_eq!(runtime.producer(), Some(successor.clone()));
    core.require_presentation_producer_ready(&successor, 1)
        .expect("the successor is ready at its claimed frontier");
}

/// A previous life's append that cannot be settled stops a retained start
/// before it claims anything: no control request and no history are sent, and
/// the retained record is kept for the next start.
#[test]
fn an_unsettled_previous_append_stops_a_retained_start_before_the_claim() {
    let platform = FakePlatform::new();
    let key = storage_key(SESSION_ID, PRESENTATION);
    let previous = SeatShared::new(platform.clone());
    previous.delivery.borrow_mut().reset_for(
        coordinate(),
        SESSION_ID,
        root_coordinate(),
        key.clone(),
        0,
        BOUND,
    );
    *previous.producer.borrow_mut() = Some(producer(&hex('c')));
    platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Network]);
    let appended = platform.sim().run(send_append(
        &previous,
        &producer(&hex('c')),
        &[ryeos_client_base::ui::SeatEvent {
            seq: 0,
            producer_engine_seq: None,
            kind: SeatEventKind::Facet {
                key: "draft".to_owned(),
                value: json!(1),
            },
        }],
        RetryPolicy::WEB,
    ));
    assert!(matches!(appended, Err(AppendFailure::Unknown(_))));
    assert!(
        platform.store.get(&key).is_some(),
        "the request is retained"
    );

    platform
        .transport
        .script(SEAT_REPLAY_PATH, vec![Reply::Network]);
    let mut runtime = new_runtime(&platform);
    let outcome = start_with(
        &platform,
        &mut runtime,
        &session(Some(RETAINED_CREATION)),
        retained(RetainedClaim::Takeover {
            expected_producer_incarnation: hex('c'),
            predecessor: control(2),
        }),
    );
    let message = refusal(outcome);
    assert!(message.contains("could not be settled"), "{message}");
    assert_eq!(count(&platform, "replay"), 1, "the receipt was asked for");
    assert_eq!(count(&platform, "history"), 0);
    assert!(
        !wire(&platform).iter().any(|name| name.starts_with("open:")),
        "no control request was sent"
    );
    assert!(platform.store.get(&key).is_some(), "the record stays");
    assert!(runtime.producer().is_none());
}

/// A fresh claim's proof must be exactly the initialization and the claim. A
/// stream carrying a mutation, or a claim that reports retained history, is
/// refused and the freshly claimed seat is handed back.
#[test]
fn a_fresh_claim_refuses_provisional_mutations_and_gives_the_seat_back() {
    // A facet row between the initialization and the claim.
    let platform = FakePlatform::new();
    let world = FreshWorld::new(&platform);
    world.script(&platform);
    let rows = vec![
        init_row(1, &world.creation),
        facet_row(2, 0, "draft", json!(1)),
        claim_row(3, &world.request, &incarnation(), 0),
    ];
    push_history(&platform, proof_chunks(&rows), true);
    let mut runtime = new_runtime(&platform);
    let message = refusal(start_fresh(&platform, &mut runtime, &world));
    assert!(message.contains("before bootstrap"), "{message}");
    assert_eq!(
        wire(&platform),
        [
            "open:prepare",
            "open:observe",
            "open:claim",
            "session",
            "history",
            "open:observe",
            "close"
        ],
        "a refused stream is not retried and the claim is given back"
    );
    let give_back = raw(&platform, SEAT_CLOSE_PATH);
    assert_eq!(
        json_of(&give_back[0]),
        serde_json::to_value(relinquish_request(&platform)).unwrap()
    );
    assert!(runtime.producer().is_none());

    // A claim that already reports engine history.
    let platform = FakePlatform::new();
    let world = FreshWorld::new(&platform);
    world.script(&platform);
    platform.transport.script(
        SEAT_OPEN_PATH,
        vec![
            prepare_reply(),
            unclaimed(),
            claim_ack(&world.request, &incarnation(), 3),
            active(&incarnation()),
        ],
    );
    let mut runtime = new_runtime(&platform);
    let message = refusal(start_fresh(&platform, &mut runtime, &world));
    assert!(message.contains("retained history"), "{message}");
    assert_eq!(
        wire(&platform),
        [
            "open:prepare",
            "open:observe",
            "open:claim",
            "open:observe",
            "close"
        ]
    );
    assert!(runtime.producer().is_none());
}

/// A proof stream that ends before its fence is discarded and read again on
/// a fresh session read and a fresh stream, three times at most; the claim is
/// never repeated, nothing is installed, and the fresh seat is handed back.
#[test]
fn an_incomplete_proof_is_retried_three_times_then_the_seat_is_given_back() {
    let platform = FakePlatform::new();
    let world = FreshWorld::new(&platform);
    world.script(&platform);
    for _ in 0..3 {
        push_history(&platform, truncated_chunks(&world.rows), false);
    }
    let mut runtime = new_runtime(&platform);

    let message = refusal(start_fresh(&platform, &mut runtime, &world));
    assert!(message.contains("three attempts"), "{message}");
    assert_eq!(
        wire(&platform),
        [
            "open:prepare",
            "open:observe",
            "open:claim",
            "session",
            "history",
            "session",
            "history",
            "session",
            "history",
            "open:observe",
            "close"
        ]
    );
    assert_eq!(open_bodies(&platform, "claim").len(), 1);
    assert!(runtime.producer().is_none());
    assert!(!runtime.has_prepared_presentation());
}

/// A fixed anchor that is overtaken while the proof is read restarts the
/// proof (fresh session read, fresh stream) under the same acknowledged claim:
/// one claim is sent and the producer is the one claimed.
#[test]
fn an_overtaken_anchor_restarts_the_proof_without_a_new_claim() {
    let platform = FakePlatform::new();
    let world = FreshWorld::new(&platform);
    world.script(&platform);
    for _ in 0..2 {
        push_history(&platform, invalidated_chunks(&world.rows), true);
    }
    world.proof(&platform, true);
    let mut runtime = new_runtime(&platform);

    start_fresh(&platform, &mut runtime, &world).expect("the third read completes");

    assert_eq!(
        wire(&platform),
        [
            "open:prepare",
            "open:observe",
            "open:claim",
            "session",
            "history",
            "session",
            "history",
            "session",
            "history",
            "session"
        ]
    );
    assert_eq!(open_bodies(&platform, "claim").len(), 1);
    assert_eq!(runtime.producer(), Some(producer(&incarnation())));
}

/// Nothing is prepared or claimed while delivery is blocked: the claim is
/// refused before any request.
#[test]
fn a_blocked_delivery_prevents_preparing_another_presentation() {
    let platform = FakePlatform::new();
    let world = FreshWorld::new(&platform);
    let shared = SeatShared::new(platform.clone());
    shared.delivery.borrow_mut().blocked = true;
    let mut intent = ClaimIntent::new(&*platform);

    let outcome = platform.sim().run(claim_and_replay(
        &shared,
        &mut intent,
        &world.before,
        ryeos_client_base::ui::BrowserViewport::default(),
        &SeatStart::Fresh,
        CoreConfig::default(),
    ));
    let message = refusal(outcome);
    assert!(message.contains("unresolved append"), "{message}");
    assert!(platform.transport.calls.borrow().is_empty());
    assert!(intent.prepared().is_none());
    assert!(shared.current_producer().is_none());
}
