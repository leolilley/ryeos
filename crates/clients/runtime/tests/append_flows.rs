//! Append delivery on the deterministic platform: what an unknown, malformed,
//! throttled or storage-refused append leaves behind (blocked delivery, the
//! retained request, the frontier), and how an authentication response is
//! reconciled before the identical request is resent.

mod common;

use std::rc::Rc;

use common::sim::{FakePlatform, Reply};
use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{SeatEvent, SeatEventKind, UiBindingAttachmentCoordinate};
use ryeos_client_runtime::api::{
    AppendRequest, SEAT_APPEND_PATH, SEAT_CLOSE_PATH, SEAT_OPEN_PATH, SESSION_CURRENT_PATH,
    WireSeatEvent, encode_append_request, wire_events_digest,
};
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::history::SeatCoordinate;
use ryeos_client_runtime::platform::Store;
use ryeos_client_runtime::retention::RetainedPendingAppend;
use ryeos_client_runtime::seat::{
    AppendFailure, AuthContext, ExpectedAck, RetryPolicy, SeatShared, post_append_exactly,
    relinquish_current, send_append,
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

/// A seat whose producer frontier is `next_engine_seq`.
fn seat(platform: &Rc<FakePlatform>, next_engine_seq: u64) -> Rc<SeatShared<FakePlatform>> {
    let shared = SeatShared::new(platform.clone());
    shared.delivery.borrow_mut().reset_for(
        coordinate(),
        "session",
        root(),
        KEY.to_owned(),
        next_engine_seq,
        1_000_000,
    );
    *shared.producer.borrow_mut() = Some(producer());
    shared
}

/// A local seat event. `seq` is the core's local sequence, which is never the
/// producer position it is sent at.
fn event(seq: u64, value: u64) -> SeatEvent {
    SeatEvent {
        seq,
        producer_engine_seq: None,
        kind: SeatEventKind::Facet {
            key: "draft".to_owned(),
            value: json!(value),
        },
    }
}

/// The acknowledgement the daemon would give `request`: every row echoes the
/// staged event at its staged position, rows are linked, and the receipt
/// follows the last row.
fn ack_for(request: &Value, chain_root_id: &str) -> Value {
    let events = request["events"].as_array().expect("request events");
    let count = u64::try_from(events.len()).unwrap();
    let hash = |index: u64| format!("{index:064x}");
    let (first_chain_seq, first_thread_seq) = (10_u64, 20_u64);
    let rows: Vec<Value> = events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            let index = u64::try_from(index).unwrap();
            let previous = if index == 0 {
                Value::Null
            } else {
                json!(hash(index))
            };
            json!({
                "event_hash": hash(index + 1),
                "chain_root_id": chain_root_id,
                "chain_seq": first_chain_seq + index,
                "thread_id": request["thread_id"].clone(),
                "thread_seq": first_thread_seq + index,
                "event_type": event["event_type"].clone(),
                "durability": "durable",
                "ts": "2026-01-01T00:00:00Z",
                "prev_chain_event_hash": previous.clone(),
                "prev_thread_event_hash": previous,
                "payload": {
                    "seq": event["engine_seq"].clone(),
                    "payload": event["payload"].clone(),
                },
            })
        })
        .collect();
    json!({
        "producer_incarnation": request["producer_incarnation"].clone(),
        "operation_id": request["operation_id"].clone(),
        "first_engine_seq": request["first_engine_seq"].clone(),
        "last_engine_seq": request["last_engine_seq"].clone(),
        "event_count": request["event_count"].clone(),
        "payload_digest": request["payload_digest"].clone(),
        "appended": count,
        "chain_seq": first_chain_seq + count,
        "receipt_thread_seq": first_thread_seq + count,
        "receipt_event_hash": hash(0xabc),
        "events_authority": "authoritative",
        "events": rows,
    })
}

/// The exact request `send_append` builds for `events` at `frontier`, read
/// from a throwaway seat whose daemon never answers.
fn request_for(events: &[SeatEvent], frontier: u64) -> Value {
    let platform = FakePlatform::new();
    let shared = seat(&platform, frontier);
    platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Network]);
    let _ = platform
        .sim()
        .run(send_append(&shared, &producer(), events, RetryPolicy::WEB));
    let sent = platform.transport.calls_to(SEAT_APPEND_PATH);
    serde_json::from_str(&sent[0]).expect("a JSON request")
}

/// An unknown outcome blocks the seat, keeps the exact request that was sent
/// under its storage key, and allows no second send: neither the same events
/// nor new ones, and never a claim.
#[test]
fn an_unknown_outcome_blocks_the_seat_and_keeps_the_exact_retained_request() {
    let platform = FakePlatform::new();
    let shared = seat(&platform, 3);
    platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Network]);
    let events = [event(7, 1)];

    let outcome = platform
        .sim()
        .run(send_append(&shared, &producer(), &events, RetryPolicy::WEB));
    assert!(
        matches!(outcome, Err(AppendFailure::Unknown(_))),
        "{outcome:?}"
    );

    let sent = platform.transport.calls_to(SEAT_APPEND_PATH);
    assert_eq!(sent.len(), 5);
    assert!(
        sent.iter().all(|body| *body == sent[0]),
        "every retry is the identical request"
    );
    let body: Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(body["first_engine_seq"], 3);
    assert_eq!(body["events"][0]["engine_seq"], 3);

    let (blocked, pending, frontier) = {
        let delivery = shared.delivery.borrow();
        (
            delivery.blocked,
            delivery.pending.clone(),
            delivery.next_engine_seq,
        )
    };
    assert!(blocked);
    assert_eq!(frontier, 3, "an unacknowledged append moves no frontier");
    let pending = pending.expect("the request stays pinned");
    assert_eq!(pending.record.encoded_request, sent[0]);
    assert_eq!(
        pending.events,
        events.to_vec(),
        "the captured local prefix is kept with its local sequence"
    );
    let stored = platform.store.get(KEY).expect("the request is retained");
    assert_eq!(
        RetainedPendingAppend::parse(&stored).unwrap(),
        pending.record
    );

    for again in [&events[..], &[event(7, 1), event(8, 2)][..]] {
        let outcome =
            platform
                .sim()
                .run(send_append(&shared, &producer(), again, RetryPolicy::WEB));
        assert!(
            matches!(outcome, Err(AppendFailure::Unknown(ref message)) if message.contains("blocked")),
            "{outcome:?}"
        );
    }
    assert_eq!(platform.transport.calls_to(SEAT_APPEND_PATH).len(), 5);
    assert!(
        platform.transport.calls_to(SEAT_OPEN_PATH).is_empty(),
        "no claim is made for an unknown append"
    );
    assert_eq!(platform.store.get(KEY), Some(stored));
}

/// A 2xx answer that is not the acknowledgement proves nothing. It is retried
/// like a lost reply, but at most five times whatever the policy allows, and
/// then the outcome is unknown with the request still retained.
#[test]
fn malformed_acknowledgements_are_unknown_after_the_reply_limit() {
    let platform = FakePlatform::new();
    let shared = seat(&platform, 0);
    platform.transport.script(
        SEAT_APPEND_PATH,
        vec![Reply::Ok(
            json!({"result": {"unexpected": true}}).to_string(),
        )],
    );

    let outcome = platform.sim().run(send_append(
        &shared,
        &producer(),
        &[event(0, 1)],
        RetryPolicy::TERMINAL,
    ));
    assert!(
        matches!(outcome, Err(AppendFailure::Unknown(ref message)) if message.contains("acknowledgement is invalid")),
        "{outcome:?}"
    );
    assert_eq!(platform.transport.calls_to(SEAT_APPEND_PATH).len(), 5);
    assert_eq!(*platform.clock.sleeps.borrow(), vec![100, 200, 400, 800]);
    assert!(shared.delivery.borrow().blocked);
    assert!(shared.delivery.borrow().pending.is_some());
    assert!(platform.store.get(KEY).is_some());
}

/// A throttled answer says nothing about whether the request committed, so the
/// identical request is resent. The acknowledgement that follows assigns the
/// wire positions from the producer frontier (never the core's local
/// sequence), clears the stored request, advances the frontier past the
/// interval and leaves the next append continuing there.
#[test]
fn a_throttled_answer_then_the_acknowledgement_clears_storage_and_moves_the_frontier() {
    let events = [event(7, 1), event(8, 2)];
    let ack = ack_for(&request_for(&events, 3), "chain-1");

    let platform = FakePlatform::new();
    let shared = seat(&platform, 3);
    platform.transport.script(
        SEAT_APPEND_PATH,
        vec![Reply::Status(429), Reply::Ok(ack.to_string())],
    );
    let verified = platform
        .sim()
        .run(send_append(&shared, &producer(), &events, RetryPolicy::WEB))
        .expect("the acknowledgement settles the append");

    let sent = platform.transport.calls_to(SEAT_APPEND_PATH);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1], "the resend is byte-identical");
    assert_eq!(*platform.clock.sleeps.borrow(), vec![100]);
    let body: Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(body["first_engine_seq"], 3);
    assert_eq!(body["last_engine_seq"], 4);
    assert_eq!(body["event_count"], 2);
    assert_eq!(body["events"][0]["engine_seq"], 3);
    assert_eq!(body["events"][1]["engine_seq"], 4);

    assert_eq!(verified.rows.len(), 2);
    assert_eq!(verified.summary.first_engine_seq, 3);
    assert_eq!(verified.summary.last_engine_seq, 4);
    assert_eq!(verified.summary.event_count, 2);
    assert_eq!(verified.summary.receipt_thread_seq, 22);

    {
        let delivery = shared.delivery.borrow();
        assert!(delivery.pending.is_none());
        assert!(!delivery.blocked);
        assert_eq!(delivery.next_engine_seq, 5);
    }
    assert!(platform.store.get(KEY).is_none(), "storage is cleared");
    assert_eq!(shared.current_producer(), Some(producer()));

    // The next append continues at the new frontier.
    platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Network]);
    let _ = platform.sim().run(send_append(
        &shared,
        &producer(),
        &[event(9, 3)],
        RetryPolicy::WEB,
    ));
    let next: Value = serde_json::from_str(
        platform
            .transport
            .calls_to(SEAT_APPEND_PATH)
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(next["first_engine_seq"], 5);
}

/// A store that refuses the write fences the seat before anything is sent:
/// nothing reaches the daemon, nothing is retained, and later appends are
/// refused outright.
#[test]
fn a_refused_storage_write_fences_before_any_send() {
    let platform = FakePlatform::new();
    let shared = seat(&platform, 0);
    platform.store.fail_writes(true);

    let outcome = platform.sim().run(send_append(
        &shared,
        &producer(),
        &[event(0, 1)],
        RetryPolicy::WEB,
    ));
    assert!(
        matches!(outcome, Err(AppendFailure::Fatal(_))),
        "{outcome:?}"
    );
    assert!(platform.transport.calls.borrow().is_empty());
    assert!(shared.delivery.borrow().blocked);
    assert!(shared.delivery.borrow().pending.is_none());
    assert!(platform.store.get(KEY).is_none());

    platform.store.fail_writes(false);
    let outcome = platform.sim().run(send_append(
        &shared,
        &producer(),
        &[event(0, 1)],
        RetryPolicy::WEB,
    ));
    assert!(
        matches!(outcome, Err(AppendFailure::Refused(_))),
        "{outcome:?}"
    );
    assert!(platform.transport.calls.borrow().is_empty());
}

/// Giving the seat back while an append is unresolved is refused before any
/// request: the writer stays, and the retained request stays for recovery.
#[test]
fn relinquishing_with_an_unresolved_append_keeps_the_retained_request() {
    let platform = FakePlatform::new();
    let shared = seat(&platform, 0);
    platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Network]);
    let outcome = platform.sim().run(send_append(
        &shared,
        &producer(),
        &[event(0, 1)],
        RetryPolicy::WEB,
    ));
    assert!(matches!(outcome, Err(AppendFailure::Unknown(_))));
    let stored = platform.store.get(KEY).expect("retained");

    let outcome = platform.sim().run(relinquish_current(&shared));
    assert!(
        matches!(outcome, Err(RuntimeError::Fatal(_))),
        "{outcome:?}"
    );
    assert!(platform.transport.calls_to(SEAT_OPEN_PATH).is_empty());
    assert!(platform.transport.calls_to(SEAT_CLOSE_PATH).is_empty());
    assert_eq!(platform.store.get(KEY), Some(stored));
    assert_eq!(shared.current_producer(), Some(producer()));
    assert!(shared.delivery.borrow().pending.is_some());
}

// ---------------------------------------------------------------------------
// Authentication reconciliation
// ---------------------------------------------------------------------------

fn auth_context() -> AuthContext {
    AuthContext {
        session_id: "session".to_owned(),
        root: root(),
        coordinate: SeatCoordinate {
            presentation_id: "presentation".to_owned(),
            thread_id: "thread".to_owned(),
            chain_root_id: "chain".to_owned(),
        },
    }
}

/// The session record that still names the seat under the same root.
fn session_record() -> Value {
    json!({
        "session_id": "session",
        "binding_attachments": [{
            "binding_attachment_id": "root",
            "binding_generation": 1,
            "binding_digest": "digest",
            "subject": {"kind": "projectless"},
            "project_path": null,
        }],
        "presentations": [{
            "presentation_id": "presentation",
            "thread_id": "thread",
            "chain_root_id": "chain",
            "root_binding": {
                "binding_attachment_id": "root",
                "binding_generation": 1,
                "binding_digest": "digest",
            },
        }],
    })
}

/// One staged event, its encoded request and what the acknowledgement must
/// echo.
fn staged_request() -> (Vec<WireSeatEvent>, String, ExpectedAck) {
    let staged = vec![WireSeatEvent {
        engine_seq: 0,
        event_type: "seat.facet".to_owned(),
        payload: json!({"key": "draft", "value": 1}),
    }];
    let producer_incarnation = "p".repeat(64);
    let digest = wire_events_digest(&staged).unwrap();
    let encoded = encode_append_request(&AppendRequest {
        presentation_id: "presentation",
        thread_id: "thread",
        producer_incarnation: &producer_incarnation,
        operation_id: "operation-1",
        first_engine_seq: 0,
        last_engine_seq: 0,
        event_count: 1,
        payload_digest: &digest,
        events: &staged,
    })
    .unwrap();
    let expected = ExpectedAck {
        producer: producer_incarnation,
        operation_id: "operation-1".to_owned(),
        first_engine_seq: 0,
        last_engine_seq: 0,
        event_count: 1,
        payload_digest: digest,
    };
    (staged, encoded, expected)
}

/// A 401 or 403 is first treated as a possibly rotated credential: the
/// session is re-read once, and when it still names the same seat under the
/// same root the identical request is resent at once (no backoff).
#[test]
fn an_authentication_response_rereads_the_membership_once_then_resends_identically() {
    for status in [401_u16, 403] {
        let (staged, encoded, expected) = staged_request();
        let ack = ack_for(&serde_json::from_str(&encoded).unwrap(), "chain");
        let platform = FakePlatform::new();
        platform.transport.script(
            SEAT_APPEND_PATH,
            vec![Reply::Status(status), Reply::Ok(ack.to_string())],
        );
        platform.transport.script(
            SESSION_CURRENT_PATH,
            vec![Reply::Ok(session_record().to_string())],
        );

        let verified = platform
            .sim()
            .run(post_append_exactly(
                &*platform,
                &auth_context(),
                &encoded,
                &expected,
                &staged,
                RetryPolicy::WEB,
                &|| true,
            ))
            .expect("the resend is acknowledged");

        assert_eq!(
            platform.transport.calls_to(SEAT_APPEND_PATH),
            vec![encoded.clone(), encoded.clone()],
            "{status}"
        );
        assert_eq!(
            platform.transport.calls_to(SESSION_CURRENT_PATH).len(),
            1,
            "{status}"
        );
        assert!(platform.clock.sleeps.borrow().is_empty(), "{status}");
        assert_eq!(verified.summary.operation_id, "operation-1");
    }
}

/// Only one re-read is allowed: a second authentication response is a
/// definite refusal, not another re-read.
#[test]
fn a_second_authentication_response_is_a_definite_refusal() {
    let (staged, encoded, expected) = staged_request();
    let platform = FakePlatform::new();
    platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Status(401)]);
    platform.transport.script(
        SESSION_CURRENT_PATH,
        vec![Reply::Ok(session_record().to_string())],
    );

    let outcome = platform.sim().run(post_append_exactly(
        &*platform,
        &auth_context(),
        &encoded,
        &expected,
        &staged,
        RetryPolicy::TERMINAL,
        &|| true,
    ));
    assert!(
        matches!(outcome, Err(AppendFailure::Refused(ref message)) if message.contains("401")),
        "{outcome:?}"
    );
    assert_eq!(platform.transport.calls_to(SEAT_APPEND_PATH).len(), 2);
    assert_eq!(platform.transport.calls_to(SESSION_CURRENT_PATH).len(), 1);
}

/// When the re-read session no longer says exactly what the append was
/// admitted under (another session, a foreign root, a vanished membership, a
/// missing or contradictory mandatory subject, or no readable session at all)
/// the append is refused and never resent.
#[test]
fn a_changed_session_refuses_the_append_without_resending() {
    type Mutation = fn(&mut Value);
    let cases: [(&str, Mutation, &str); 6] = [
        (
            "another session",
            |session| session["session_id"] = json!("other"),
            "browser session changed while reconciling a seat append",
        ),
        (
            "foreign root attachment",
            |session| session["binding_attachments"][0]["binding_digest"] = json!("foreign"),
            "browser root attachment changed while reconciling a seat append",
        ),
        (
            "foreign root on the membership",
            |session| {
                session["presentations"][0]["root_binding"]["binding_digest"] = json!("foreign");
            },
            "prepared seat root binding differs from admitted attachment",
        ),
        (
            "membership gone",
            |session| session["presentations"] = json!([]),
            "browser presentation membership changed while reconciling a seat append",
        ),
        (
            "missing subject",
            |session| {
                session["binding_attachments"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("subject");
            },
            "binding attachment subject is required",
        ),
        (
            "contradictory subject",
            |session| {
                session["binding_attachments"][0]["subject"] =
                    json!({"kind": "unregistered_project"});
            },
            "binding attachment subject contradicts its project metadata or closed schema",
        ),
    ];
    for (name, mutate, message) in cases {
        let (staged, encoded, expected) = staged_request();
        let platform = FakePlatform::new();
        platform
            .transport
            .script(SEAT_APPEND_PATH, vec![Reply::Status(401)]);
        let mut session = session_record();
        mutate(&mut session);
        platform
            .transport
            .script(SESSION_CURRENT_PATH, vec![Reply::Ok(session.to_string())]);

        let outcome = platform.sim().run(post_append_exactly(
            &*platform,
            &auth_context(),
            &encoded,
            &expected,
            &staged,
            RetryPolicy::TERMINAL,
            &|| true,
        ));
        assert!(
            matches!(outcome, Err(AppendFailure::Refused(ref text)) if text.contains(message)),
            "{name}: {outcome:?}"
        );
        assert_eq!(
            platform.transport.calls_to(SEAT_APPEND_PATH).len(),
            1,
            "{name}: nothing is resent"
        );
        assert_eq!(
            platform.transport.calls_to(SESSION_CURRENT_PATH).len(),
            1,
            "{name}"
        );
    }

    // No readable session at all (the read itself fails): also a refusal.
    let (staged, encoded, expected) = staged_request();
    let platform = FakePlatform::new();
    platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Status(403)]);
    let outcome = platform.sim().run(post_append_exactly(
        &*platform,
        &auth_context(),
        &encoded,
        &expected,
        &staged,
        RetryPolicy::TERMINAL,
        &|| true,
    ));
    assert!(
        matches!(outcome, Err(AppendFailure::Refused(_))),
        "{outcome:?}"
    );
    assert_eq!(platform.transport.calls_to(SEAT_APPEND_PATH).len(), 1);
}
