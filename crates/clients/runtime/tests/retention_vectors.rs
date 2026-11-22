//! Replays the retention groups of `pending-append.json`. The protocol groups
//! (acknowledgement, retry, batch planning, wire events) are in `seat_vectors`.

mod common;

use common::{group, name_of, typed};
use ryeos_client_runtime::api::{
    AppendRequest, WireSeatEvent, encode_append_request, wire_events_digest,
};
use ryeos_client_runtime::platform::{MemoryStore, Store};
use ryeos_client_runtime::retention::{
    PENDING_APPEND_SCHEMA, RetainedPendingAppend, load, storage_key,
};
use serde_json::Value;

#[test]
fn storage_key_vectors() {
    let cases = group("pending-append", "storage_key");
    assert!(!cases.is_empty());
    for case in cases {
        let input = &case["input"];
        let key = storage_key(
            input["session_id"].as_str().unwrap(),
            input["presentation_id"].as_str().unwrap(),
        );
        assert_eq!(
            Value::String(key),
            case["expected"]["ok"],
            "{}",
            name_of(&case)
        );
    }
}

fn wire_events(batch: &Value) -> Vec<WireSeatEvent> {
    batch
        .as_array()
        .expect("batch events")
        .iter()
        .map(|event| WireSeatEvent {
            engine_seq: event["engine_seq"].as_u64().expect("engine_seq"),
            event_type: event["event_type"].as_str().expect("event_type").to_owned(),
            payload: event["payload"].clone(),
        })
        .collect()
}

#[test]
fn retention_record_vector() {
    let cases = group("pending-append", "retention_record");
    assert!(!cases.is_empty());
    for case in cases {
        let input = typed(&case["input"]).expect("typed input");
        let expected = &case["expected"];
        let events = wire_events(&input["batch_events"]);
        let digest = wire_events_digest(&events).expect("digest");
        assert_eq!(Value::String(digest.clone()), expected["payload_digest"]);

        let first: u64 = input["first_engine_seq"].as_str().unwrap().parse().unwrap();
        let last: u64 = input["last_engine_seq"].as_str().unwrap().parse().unwrap();
        let text = |field: &str| input[field].as_str().unwrap().to_owned();
        let encoded = encode_append_request(&AppendRequest {
            presentation_id: &text("presentation_id"),
            thread_id: &text("thread_id"),
            producer_incarnation: &text("producer_incarnation"),
            operation_id: &text("operation_id"),
            first_engine_seq: first,
            last_engine_seq: last,
            event_count: events.len() as u64,
            payload_digest: &digest,
            events: &events,
        })
        .expect("encoded request");
        assert_eq!(Value::String(encoded.clone()), expected["encoded_request"]);

        let record = RetainedPendingAppend {
            schema_version: PENDING_APPEND_SCHEMA.to_owned(),
            session_id: text("session_id"),
            presentation_id: text("presentation_id"),
            chain_root_id: text("chain_root_id"),
            thread_id: text("thread_id"),
            producer_incarnation: text("producer_incarnation"),
            operation_id: text("operation_id"),
            first_engine_seq: text("first_engine_seq"),
            last_engine_seq: text("last_engine_seq"),
            event_count: events.len() as u64,
            payload_digest: digest,
            encoded_request: encoded,
        };
        assert_eq!(
            Value::String(record.to_stored().expect("stored")),
            expected["stored_value"]
        );
        assert_eq!(
            storage_key(&record.session_id, &record.presentation_id),
            input["storage_key"].as_str().unwrap()
        );
    }
}

#[test]
fn load_retained_append_vectors() {
    let cases = group("pending-append", "load_retained_append");
    assert!(!cases.is_empty());
    for case in cases {
        let name = name_of(&case);
        let store = MemoryStore::new();
        if let Some(stored) = case["input"]["stored_value"].as_str() {
            store.set("k", stored).unwrap();
        }
        let outcome = load(&store, "k");
        let expected = &case["expected"];
        if let Some(message) = expected["error"]["message"].as_str() {
            let text = outcome.expect_err(name).to_string();
            if message.starts_with("retained seat append is malformed: ") {
                assert!(
                    text.starts_with("retained seat append is malformed: "),
                    "{name}: {text}"
                );
            } else {
                assert_eq!(text, message, "{name}");
            }
        } else {
            let loaded = outcome.unwrap_or_else(|error| panic!("{name}: {error}"));
            let actual = loaded.map(|record| serde_json::to_value(record).unwrap());
            // The browser loader hands back the parsed object as-is, so an
            // ignored unknown field still appears in its output; the typed
            // record drops it. Compare the record's own fields only.
            let mut expected_ok = expected["ok"].clone();
            if let (Some(Value::Object(actual)), Value::Object(expected)) =
                (actual.as_ref(), &mut expected_ok)
            {
                expected.retain(|key, _| actual.contains_key(key));
            }
            assert_eq!(actual.unwrap_or(Value::Null), expected_ok, "{name}");
        }
    }
}
