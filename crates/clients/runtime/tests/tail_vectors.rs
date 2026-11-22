//! Replays `thread-tail.json` against the focused-tail predicate.

mod common;

use common::{load, name_of};
use ryeos_client_runtime::streams::focused_tail_payload;
use serde_json::{Value, json};

#[test]
fn thread_tail_vectors() {
    let cases = load("thread-tail");
    assert!(!cases.is_empty());
    for case in cases {
        let input = &case["input"];
        let chain = input["chain_root_id"].as_str();
        let thread = input["thread_id"].as_str();
        let actual = focused_tail_payload(&input["frame"], chain, thread)
            .map(|(event_type, payload)| json!({ "eventType": event_type, "payload": payload }))
            .unwrap_or(Value::Null);
        assert_eq!(actual, case["expected"], "{}", name_of(&case));
    }
}
