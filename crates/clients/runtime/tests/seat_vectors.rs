//! Replays the protocol groups of `pending-append.json` and `seat-touch.json`.
//!
//! Where the browser's JavaScript number type decides a case (an integer
//! beyond 2^53 held in a `Number`, a value above `u64::MAX` held in a
//! `BigInt`) the case is skipped by name: a JSON document has only one
//! integer type and the runtime's domain is `u64`.

mod common;

use common::sim::{FakePlatform, Reply};
use common::{group, load, name_of, typed};
use ryeos_client_base::ui::UiBindingAttachmentCoordinate;
use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_runtime::api::{AppendRequest, WireSeatEvent, encode_append_request};
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::history::SeatCoordinate;
use ryeos_client_runtime::seat::{
    AppendFailure, AuthContext, ExpectedAck, PlainSeatEvent, RetryPolicy, TouchOutcome,
    append_prefix_len, classify_touch_error, classify_touch_response, encoded_event_bytes,
    parse_exact_integer, plan_append_len, post_append_exactly, require_append_acknowledgement,
    retained_append_events, unwrap_result, wire_seat_events,
};
use serde_json::{Value, json};

fn text(value: &Value, field: &str) -> String {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field}"))
        .to_owned()
}

fn u64_of(value: &Value) -> u64 {
    typed(value).unwrap().as_u64().expect("u64")
}

fn coordinate_of(value: &Value) -> SeatCoordinate {
    SeatCoordinate {
        presentation_id: text(value, "presentation_id"),
        thread_id: text(value, "thread_id"),
        chain_root_id: text(value, "chain_root_id"),
    }
}

fn expected_ack_of(value: &Value) -> ExpectedAck {
    ExpectedAck {
        producer: text(value, "producer"),
        operation_id: text(value, "operationId"),
        first_engine_seq: u64_of(&value["firstEngineSeq"]),
        last_engine_seq: u64_of(&value["lastEngineSeq"]),
        event_count: value["eventCount"].as_u64().expect("eventCount"),
        payload_digest: text(value, "payloadDigest"),
    }
}

fn wire_of(events: &Value) -> Vec<WireSeatEvent> {
    typed(events)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|event| WireSeatEvent {
            engine_seq: event["engine_seq"].as_u64().unwrap(),
            event_type: text(event, "event_type"),
            payload: event["payload"].clone(),
        })
        .collect()
}

fn expected_message(case: &Value) -> Option<&str> {
    case["expected"]["error"]["message"].as_str()
}

// ---------------------------------------------------------------------------
// seat-touch
// ---------------------------------------------------------------------------

fn touch_word(outcome: TouchOutcome) -> &'static str {
    match outcome {
        TouchOutcome::Renewed => "renewed",
        TouchOutcome::Retry => "retry",
        TouchOutcome::Refused => "refused",
    }
}

#[test]
fn seat_touch_classification_vectors() {
    let cases = load("seat-touch");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case);
        let input = &case["input"];
        let outcome = match case["function"].as_str().unwrap() {
            "classifySeatTouchResponse" => classify_touch_response(
                &input["response"],
                input["presentation_id"].as_str().unwrap(),
                input["thread_id"].as_str().unwrap(),
            ),
            _ => {
                let error = match input["http_status"].as_u64() {
                    Some(status) => RuntimeError::Daemon {
                        path: "/ui/api/session/seat/touch".into(),
                        status: status as u16,
                        message: "refused".into(),
                    },
                    None => RuntimeError::Network {
                        path: "/ui/api/session/seat/touch".into(),
                        message: "offline".into(),
                    },
                };
                classify_touch_error(&error)
            }
        };
        assert_eq!(
            touch_word(outcome),
            case["expected"].as_str().unwrap(),
            "{name}"
        );
    }
}

#[test]
fn touch_and_close_predicates() {
    use ryeos_client_runtime::seat::{may_close_seat, may_start_seat_touch};
    assert!(may_start_seat_touch(false, false, false, false));
    for blocked in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        assert!(!may_start_seat_touch(
            blocked.0, blocked.1, blocked.2, blocked.3
        ));
    }
    assert!(may_close_seat(3, 3, false, false, false));
    assert!(!may_close_seat(4, 3, false, false, false));
    assert!(!may_close_seat(3, 3, true, false, false));
    assert!(!may_close_seat(3, 3, false, true, false));
    assert!(!may_close_seat(3, 3, false, false, true));
}

// ---------------------------------------------------------------------------
// Acknowledgement
// ---------------------------------------------------------------------------

#[test]
fn append_acknowledgement_vectors() {
    let cases = group("pending-append", "append_acknowledgement");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case).to_owned();
        let input = &case["input"];
        let response = typed(&input["response"]).unwrap();
        let expected = expected_ack_of(&input["expected"]);
        let staged = wire_of(&input["staged_events"]);
        let coordinate = coordinate_of(&input["coordinate"]);
        let outcome = require_append_acknowledgement(&response, &expected, &staged, &coordinate);
        match (outcome, expected_message(&case)) {
            (Err(error), Some(message)) => assert_eq!(error.0, message, "{name}"),
            (Ok(ack), None) => {
                let ok = &case["expected"]["ok"];
                assert_eq!(
                    ack.summary.receipt_event_hash,
                    text(ok, "event_hash"),
                    "{name}"
                );
                assert_eq!(
                    ack.summary.receipt_thread_seq,
                    u64_of(&ok["thread_seq"]),
                    "{name}"
                );
                assert_eq!(
                    ack.rows.len() as u64,
                    ok["events_count"].as_u64().unwrap(),
                    "{name}"
                );
            }
            (other, wanted) => panic!("{name}: got {other:?}, wanted {wanted:?}"),
        }
    }
}

#[test]
fn receipt_hash_equal_to_the_last_event_hash_is_refused() {
    let cases = group("pending-append", "append_acknowledgement");
    let case = &cases[0];
    let input = &case["input"];
    let mut response = typed(&input["response"]).unwrap();
    let last_hash = response["events"][0]["event_hash"].clone();
    response["receipt_event_hash"] = last_hash;
    let outcome = require_append_acknowledgement(
        &response,
        &expected_ack_of(&input["expected"]),
        &wire_of(&input["staged_events"]),
        &coordinate_of(&input["coordinate"]),
    );
    assert_eq!(
        outcome.unwrap_err().0,
        "seat append acknowledgement receipt is not adjacent to its event interval"
    );
}

#[test]
fn retained_append_events_vectors() {
    let cases = group("pending-append", "retained_append_events");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case).to_owned();
        let input = &case["input"];
        let outcome = retained_append_events(
            input["encoded_request"].as_str().unwrap(),
            &expected_ack_of(&input["expected"]),
            &coordinate_of(&input["coordinate"]),
        );
        match (outcome, expected_message(&case)) {
            (Err(error), Some(message)) if message.starts_with("invalid JSON at offset") => {
                assert!(
                    error.0.starts_with("invalid JSON at offset"),
                    "{name}: {}",
                    error.0
                );
            }
            (Err(error), Some(message)) => assert_eq!(error.0, message, "{name}"),
            (Ok(events), None) => {
                let wanted = typed(&case["expected"]["ok"]).unwrap();
                let actual: Vec<Value> = events.iter().map(WireSeatEvent::to_value).collect();
                assert_eq!(Value::Array(actual), wanted, "{name}");
            }
            (other, wanted) => panic!("{name}: got {other:?}, wanted {wanted:?}"),
        }
    }
}

#[test]
fn parse_exact_integer_vectors() {
    let cases = group("pending-append", "parse_exact_integer");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case).to_owned();
        if matches!(
            name.as_str(),
            "unsafe number" | "string above u64 (unbounded here)"
        ) {
            continue;
        }
        let Ok(input) = typed(&case["input"]) else {
            continue;
        };
        let outcome = parse_exact_integer(&input, "f");
        match (outcome, expected_message(&case)) {
            (Err(error), Some(message)) => assert_eq!(error.0, message, "{name}"),
            (Ok(value), None) => assert_eq!(value, u64_of(&case["expected"]["ok"]), "{name}"),
            (other, wanted) => panic!("{name}: got {other:?}, wanted {wanted:?}"),
        }
    }
}

#[test]
fn unwrap_result_vectors() {
    let cases = group("pending-append", "unwrap_result");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case).to_owned();
        let input = typed(&case["input"]).unwrap();
        match (unwrap_result(&input), expected_message(&case)) {
            (Err(error), Some(message)) => assert_eq!(error.0, message, "{name}"),
            (Ok(value), None) => {
                assert_eq!(*value, typed(&case["expected"]["ok"]).unwrap(), "{name}")
            }
            (other, wanted) => panic!("{name}: got {other:?}, wanted {wanted:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire events and sizing
// ---------------------------------------------------------------------------

fn plain_of(event: &Value) -> PlainSeatEvent {
    let event = typed(event).unwrap();
    PlainSeatEvent {
        seq: event.get("seq").and_then(Value::as_u64).unwrap_or(0),
        producer_engine_seq: event.get("producer_engine_seq").and_then(Value::as_u64),
        event_type: text(&event, "event_type"),
        payload: event["payload"].clone(),
    }
}

#[test]
fn wire_seat_events_vectors() {
    let cases = group("pending-append", "wire_seat_events");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case).to_owned();
        let input = &case["input"];
        let events: Vec<PlainSeatEvent> = input["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(plain_of)
            .collect();
        let first = u64_of(&input["first_engine_seq"]);
        match (wire_seat_events(&events, first), expected_message(&case)) {
            (Err(error), Some(message)) => assert_eq!(error.0, message, "{name}"),
            (Ok(wire), None) => {
                let actual: Vec<Value> = wire.iter().map(WireSeatEvent::to_value).collect();
                assert_eq!(
                    Value::Array(actual),
                    typed(&case["expected"]["ok"]).unwrap(),
                    "{name}"
                );
            }
            (other, wanted) => panic!("{name}: got {other:?}, wanted {wanted:?}"),
        }
    }
}

#[test]
fn encoded_seat_event_bytes_vectors() {
    let cases = group("pending-append", "encoded_seat_event_bytes");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case).to_owned();
        if expected_message(&case).is_some() {
            continue;
        }
        let event = plain_of(&case["input"]);
        let bytes = encoded_event_bytes(&event).unwrap();
        assert_eq!(
            bytes as u64,
            case["expected"]["ok"].as_u64().unwrap(),
            "{name}"
        );
    }
}

fn producer_of(wrapper: &Value) -> UiPresentationProducer {
    UiPresentationProducer {
        presentation_id: text(wrapper, "presentation_id"),
        thread_id: text(wrapper, "thread_id"),
        producer_incarnation: text(wrapper, "producer_incarnation"),
    }
}

/// Size of the request for `events` at their real positions.
fn real_request_len(
    producer: &UiPresentationProducer,
    events: &[PlainSeatEvent],
    first: u64,
) -> usize {
    let wire = wire_seat_events(events, first).unwrap();
    let last = first + wire.len() as u64 - 1;
    encode_append_request(&AppendRequest {
        presentation_id: &producer.presentation_id,
        thread_id: &producer.thread_id,
        producer_incarnation: &producer.producer_incarnation,
        operation_id: "00000000-0000-8000-8000-000000000000",
        first_engine_seq: first,
        last_engine_seq: last,
        event_count: wire.len() as u64,
        payload_digest: &"0".repeat(64),
        events: &wire,
    })
    .unwrap()
    .len()
}

#[test]
fn batch_plan_vectors() {
    let cases = group("pending-append", "batch_plan");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case).to_owned();
        let input = &case["input"];
        let producer = producer_of(&input["wrapper"]);
        let limit = u64_of(&input["max_seat_append_bytes"]);
        let mut remaining: Vec<PlainSeatEvent> = input["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(plain_of)
            .collect();
        let mut first = u64_of(&input["first_engine_seq"]);
        let expected = case["expected"]["batches"].as_array().unwrap();

        let mut planned: Vec<usize> = Vec::new();
        let mut failure: Option<String> = None;
        while !remaining.is_empty() {
            match plan_append_len(&remaining, &producer, first, limit) {
                Ok(count) => {
                    assert!((1..=64).contains(&count), "{name}");
                    // The conservative plan can never exceed the bound once
                    // the real positions are substituted.
                    assert!(
                        real_request_len(&producer, &remaining[..count], first) as u64 <= limit,
                        "{name}"
                    );
                    planned.push(count);
                    remaining.drain(..count);
                    first += count as u64;
                }
                Err(error) => {
                    failure = Some(error.0);
                    break;
                }
            }
        }

        let wanted_error = expected.iter().find_map(|batch| batch["error"].as_str());
        assert_eq!(failure.as_deref(), wanted_error, "{name}");
        // The browser sizes with real positions and the plan with the widest
        // ones, so a batch may be smaller, never larger; when the widest
        // request for the browser's count fits, the counts are equal.
        for (index, count) in planned.iter().enumerate() {
            let Some(wanted) = expected
                .get(index)
                .and_then(|batch| batch["count"].as_u64())
            else {
                break;
            };
            assert!(*count as u64 <= wanted || index > 0, "{name}");
            if index == 0 {
                let events: Vec<PlainSeatEvent> = input["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(plain_of)
                    .collect();
                let widest = append_prefix_len(&events[..wanted as usize], &producer, limit);
                if widest == wanted as usize {
                    assert_eq!(*count as u64, wanted, "{name}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

fn ack_for(request: &Value) -> Value {
    let events = request["events"].as_array().unwrap();
    let count = events.len() as u64;
    let rows: Vec<Value> = events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            let index = index as u64;
            let hash = format!("{:064x}", index + 1);
            let previous = if index == 0 {
                "f".repeat(64)
            } else {
                format!("{:064x}", index)
            };
            json!({
                "event_hash": hash,
                "chain_root_id": "chain-A",
                "chain_seq": (40 + index).to_string(),
                "thread_id": "thread-A",
                "thread_seq": (3 + index).to_string(),
                "event_type": event["event_type"],
                "durability": "durable",
                "ts": "2026-10-06T00:00:00Z",
                "prev_chain_event_hash": previous,
                "prev_thread_event_hash": previous,
                "payload": {"seq": event["engine_seq"].as_u64().unwrap().to_string(), "payload": event["payload"]},
            })
        })
        .collect();
    json!({"result": {
        "producer_incarnation": request["producer_incarnation"],
        "operation_id": request["operation_id"],
        "first_engine_seq": request["first_engine_seq"].as_u64().unwrap().to_string(),
        "last_engine_seq": request["last_engine_seq"].as_u64().unwrap().to_string(),
        "event_count": count,
        "appended": count,
        "payload_digest": request["payload_digest"],
        "chain_seq": (40 + count).to_string(),
        "receipt_thread_seq": (3 + count).to_string(),
        "receipt_event_hash": "e".repeat(64),
        "events_authority": "authoritative",
        "events": rows,
    }})
}

#[test]
fn append_retry_vectors() {
    let cases = group("pending-append", "append_retry");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case).to_owned();
        let input = &case["input"];
        let encoded = text(input, "request_body_each_attempt");
        let request: Value = serde_json::from_str(&encoded).unwrap();
        let platform = FakePlatform::new();
        let replies: Vec<Reply> = input["transport_script"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| match entry {
                Value::String(word) if word == "ack" => Reply::Ok(ack_for(&request).to_string()),
                Value::String(word) if word == "network" => Reply::Network,
                Value::String(word) if word == "unreadable" => Reply::Unreadable,
                Value::Number(status) => Reply::Status(status.as_u64().unwrap() as u16),
                other => panic!("{name}: script entry {other}"),
            })
            .collect();
        platform
            .transport
            .script("/ui/api/session/seat/append", replies);

        let coordinate = SeatCoordinate {
            presentation_id: text(&request, "presentation_id"),
            thread_id: text(&request, "thread_id"),
            chain_root_id: "chain-A".to_owned(),
        };
        let expected = ExpectedAck {
            producer: text(&request, "producer_incarnation"),
            operation_id: text(&request, "operation_id"),
            first_engine_seq: request["first_engine_seq"].as_u64().unwrap(),
            last_engine_seq: request["last_engine_seq"].as_u64().unwrap(),
            event_count: request["event_count"].as_u64().unwrap(),
            payload_digest: text(&request, "payload_digest"),
        };
        let staged = wire_of(&request["events"]);
        let context = AuthContext {
            session_id: "session".to_owned(),
            root: UiBindingAttachmentCoordinate {
                binding_attachment_id: "root".into(),
                binding_generation: 1,
                binding_digest: "digest".into(),
            },
            coordinate,
        };
        let outcome = platform.sim().run(post_append_exactly(
            &*platform,
            &context,
            &encoded,
            &expected,
            &staged,
            RetryPolicy::WEB,
            &|| true,
        ));

        let expected_result = &case["expected"];
        match (&outcome, expected_result["result"].get("ok")) {
            (Ok(ack), Some(ok)) => {
                assert_eq!(
                    ack.summary.receipt_event_hash,
                    text(ok, "event_hash"),
                    "{name}"
                );
                assert_eq!(
                    ack.summary.receipt_thread_seq,
                    u64_of(&ok["thread_seq"]),
                    "{name}"
                );
            }
            (Err(failure), None) => {
                let class = expected_result["result"]["error"]["class"]
                    .as_str()
                    .unwrap();
                match (failure, class) {
                    (AppendFailure::Unknown(_), "UnknownDeliveryError") => {}
                    (AppendFailure::Refused(_), "HttpResponseError") => {}
                    other => panic!("{name}: {other:?}"),
                }
            }
            (other, _) => panic!("{name}: {other:?}"),
        }
        let calls = platform.transport.calls_to("/ui/api/session/seat/append");
        assert_eq!(
            calls.len() as u64,
            expected_result["fetch_calls"].as_u64().unwrap(),
            "{name}"
        );
        assert!(
            calls.iter().all(|body| *body == encoded),
            "{name}: bodies differ"
        );
        let delays: Vec<u64> = platform.clock.sleeps.borrow().clone();
        let wanted: Vec<u64> = expected_result["delays_ms_between_attempts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|delay| delay.as_u64().unwrap())
            .collect();
        assert_eq!(delays, wanted, "{name}");
    }
}

#[test]
fn the_terminal_policy_never_gives_up_and_caps_its_backoff() {
    let policy = RetryPolicy::TERMINAL;
    assert!(policy.allows_another(1_000));
    let delays: Vec<u64> = (1..=8)
        .map(|n| policy.delay(n).as_millis() as u64)
        .collect();
    assert_eq!(delays, [100, 200, 400, 800, 1600, 3200, 5000, 5000]);
    let web: Vec<u64> = (1..=6)
        .map(|n| RetryPolicy::WEB.delay(n).as_millis() as u64)
        .collect();
    assert_eq!(web, [100, 200, 400, 800, 1600, 2000]);
    assert!(!RetryPolicy::WEB.allows_another(5));
}
