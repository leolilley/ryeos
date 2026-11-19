//! Prompt-cache marks and cache usage, end to end against the scripted chat
//! stub. The provider entry declares where a request marks its cacheable
//! prefix; the stub records every request body, so the assertions are about
//! the bytes on the wire: marks at the declared positions, none on assistant
//! messages, a prefix that stays byte-equal from one turn to the next, and
//! cached read and write counts landing on the turn record.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

#[path = "common/stub_rig.rs"]
mod stub_rig;

use std::time::Duration;

use serde_json::{Value, json};
use stub_rig::{
    Rig, RigOptions, chain_event_list, launch_on, start_rig, thread_detail, wait_terminal,
};

const MAX_WAIT: Duration = Duration::from_secs(150);

const MARK: &str = "cache_control";

/// The provider entry of a route that caches only at explicit marks.
const CACHE_MARKS: &str = "cache_marks:\n  mark: {cache_control: {type: ephemeral}}\n  text_part_template: {type: text, text: \"{text}\"}\n  max_marks: 4\n  positions: [system, newest, previous_tail]\n";

/// The usage schema of a route whose input count already holds the cache
/// counts.
const USAGE_INCLUDED: &str = "path: usage\ninput_tokens_path: prompt_tokens\noutput_tokens_path: completion_tokens\ncache_read_tokens_path: cache_read_input_tokens\ncache_write_tokens_path: cache_creation_input_tokens\ncache_included_in_input: true\naggregation: latest_snapshot\n";

/// The same schema where the input count excludes them.
const USAGE_SEPARATE: &str = "path: usage\ninput_tokens_path: prompt_tokens\noutput_tokens_path: completion_tokens\ncache_read_tokens_path: cache_read_input_tokens\ncache_write_tokens_path: cache_creation_input_tokens\naggregation: latest_snapshot\n";

const FINITE_BUDGET: &str = "limits:\n  spend_usd: \"3\"\n";

fn image_input() -> String {
    "image_input:\n  accepts:\n    media_types: [image/png]\n    max_bytes_per_image: 4096\n    max_images_per_request: 4\n    text_part_template: {type: text, text: \"{text}\"}\n    image_part_template: {type: image_url, image_url: {url: \"{data_url}\"}}\n    tool_result_note: \"[{count} image(s) follow in the next message]\"\n    deferred_label: \"Image {index} from tool result {call_id}:\"\n    omitted_note: \"[earlier image omitted: {media_type} {width}x{height}]\"\n    cost: {pixels_per_token: 750, max_tokens: 1600, unknown_dimensions_tokens: 1600}\n".to_string()
}

/// One tool call per name, then a final answer.
fn calls(names: &[&str]) -> String {
    let mut script = String::from("steps:\n");
    for (index, name) in names.iter().enumerate() {
        script.push_str(&format!(
            "  - tool_calls:\n      - {{ id: call_{index}, name: {name}, arguments: {{}} }}\n"
        ));
    }
    script.push_str("  - text: \"done\"\n");
    script
}

async fn run_to_completion(rig: &Rig) -> String {
    let thread_id = launch_on(rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 800).await
    );
    thread_id
}

async fn assert_no_rejected_request(rig: &Rig) {
    for request in rig.stub.requests().await {
        assert!(
            request.rejected.is_none(),
            "the strict stub rejected request {}: {:?}",
            request.seq,
            request.rejected
        );
    }
}

fn messages(body: &Value) -> &Vec<Value> {
    body["messages"].as_array().expect("messages")
}

/// Indices of the messages holding a mark anywhere in their content.
fn marked(body: &Value) -> Vec<usize> {
    messages(body)
        .iter()
        .enumerate()
        .filter(|(_, message)| message.to_string().contains(MARK))
        .map(|(index, _)| index)
        .collect()
}

fn without_marks(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != MARK)
                .map(|(key, value)| (key.clone(), without_marks(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_marks).collect()),
        other => other.clone(),
    }
}

/// The wire indices the entry's positions select: the system message, the
/// newest message and the message before the last assistant message.
fn expected_marks(body: &Value) -> Vec<usize> {
    let history = messages(body);
    let mut expected = Vec::new();
    if let Some(system) = history.iter().position(|m| m["role"] == "system") {
        expected.push(system);
    }
    expected.push(history.len() - 1);
    if let Some(assistant) = history.iter().rposition(|m| m["role"] == "assistant")
        && assistant > 0
    {
        expected.push(assistant - 1);
    }
    expected.sort_unstable();
    expected.dedup();
    expected
}

/// A mark sits on the last part of the message's content array.
fn assert_marked_on_last_part(message: &Value) {
    let parts = message["content"].as_array().expect("part array content");
    let last = parts.last().expect("a part");
    assert_eq!(last[MARK], json!({"type": "ephemeral"}), "{message}");
    for part in &parts[..parts.len() - 1] {
        assert!(part.get(MARK).is_none(), "{message}");
    }
}

// 1 ── marks across four turns, prefix stability, assistant untouched ────

#[tokio::test(flavor = "multi_thread")]
async fn marks_sit_at_the_declared_positions_and_the_prefix_stays_byte_equal() {
    let rig = start_rig(
        &calls(&["noop_noop", "noop_noop", "noop_noop"]),
        RigOptions {
            provider_top_extra: CACHE_MARKS.to_string(),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    run_to_completion(&rig).await;
    assert_no_rejected_request(&rig).await;

    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 4);
    for (turn, body) in bodies.iter().enumerate() {
        let history = messages(body);
        // The probe directive has no system prompt, so the `system` position
        // selects nothing here; the unit tests cover it.
        let system = history.iter().filter(|m| m["role"] == "system").count();
        assert_eq!(system, 0, "turn {turn}");
        let positions = marked(body);
        assert_eq!(positions, expected_marks(body), "turn {turn}: {history:#?}");
        assert!(positions.len() <= 4);
        // The first request has no earlier turn to mark.
        assert_eq!(
            positions.len(),
            if turn == 0 { 1 } else { 2 },
            "turn {turn}"
        );
        for index in &positions {
            assert_ne!(history[*index]["role"], "assistant", "turn {turn}");
            assert_marked_on_last_part(&history[*index]);
        }
        // Assistant messages keep the plain form they have without marks.
        for message in history.iter().filter(|m| m["role"] == "assistant") {
            assert!(!message.to_string().contains(MARK), "turn {turn}");
            assert!(
                message["content"].is_string() || message["content"].is_null(),
                "turn {turn}: {message}"
            );
        }
        // Every other system, user and tool message renders as a part array
        // too, so a mark moving never changes a message's shape.
        for message in history.iter().filter(|m| m["role"] != "assistant") {
            assert!(message["content"].is_array(), "turn {turn}: {message}");
        }
    }

    // Each request's messages are the previous request's messages, plus new
    // ones, byte for byte, once the mark key is set aside.
    for pair in bodies.windows(2) {
        let (earlier, later) = (messages(&pair[0]), messages(&pair[1]));
        assert!(later.len() > earlier.len());
        for (index, message) in earlier.iter().enumerate() {
            assert_eq!(
                without_marks(message).to_string(),
                without_marks(&later[index]).to_string(),
                "message {index} changed between turns"
            );
        }
        // The earlier newest message is read back marked as the previous
        // tail with identical bytes.
        let previous_newest = earlier.len() - 1;
        assert_eq!(earlier[previous_newest], later[previous_newest]);
        assert!(!pair[0]["tools"].as_array().expect("tools").is_empty());
        assert_eq!(pair[0]["tools"].to_string(), pair[1]["tools"].to_string());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_entry_without_cache_marks_sends_plain_string_content() {
    let rig = start_rig(&calls(&["noop_noop"]), RigOptions::default())
        .await
        .expect("start rig");
    run_to_completion(&rig).await;
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    for body in &bodies {
        assert!(!body.to_string().contains(MARK));
        for message in messages(body) {
            assert!(
                message["content"].is_string() || message["content"].is_null(),
                "{message}"
            );
        }
    }
}

// 2 ── a lowered image message carries the newest mark ───────────────────

#[tokio::test(flavor = "multi_thread")]
async fn marks_coexist_with_a_tool_image_and_its_message_is_byte_equal_next_turn() {
    let rig = start_rig(
        &calls(&["shot_shot", "noop_noop"]),
        RigOptions {
            image_tools: true,
            provider_top_extra: format!("{}{CACHE_MARKS}", image_input()),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    run_to_completion(&rig).await;
    assert_no_rejected_request(&rig).await;

    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 3);
    let after_image = messages(&bodies[1]);
    let synthetic = after_image.last().expect("newest message");
    assert_eq!(synthetic["role"], "user");
    let parts = synthetic["content"].as_array().expect("parts");
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[1]["type"], "image_url");
    assert!(parts[0].get(MARK).is_none());
    assert_eq!(parts[1][MARK], json!({"type": "ephemeral"}));
    assert_eq!(marked(&bodies[1]), expected_marks(&bodies[1]));

    // The next request reads that message back, unchanged, as the previous
    // tail.
    let next = messages(&bodies[2]);
    let index = after_image.len() - 1;
    assert_eq!(&next[index], synthetic);
    assert_eq!(marked(&bodies[2]), expected_marks(&bodies[2]));
    assert!(marked(&bodies[2]).contains(&index));
}

// 3 ── a replayed thinking block rides an unmarked assistant message ─────

#[tokio::test(flavor = "multi_thread")]
async fn marks_coexist_with_a_replayed_thinking_block() {
    let script = "steps:\n  - reasoning: \"think about it\"\n    thinking_blocks:\n      - { thinking: \"step one\", signature: \"SIG-A\" }\n    tool_calls:\n      - { id: call_a, name: noop_noop, arguments: {} }\n    stream: { thinking_chunk_bytes: 4, signature_last: true }\n  - text: \"all done\"\n    require: { assistant_tool_turn_has: [thinking_blocks], assistant_lacks: [reasoning_content] }\n";
    let schemas = "reasoning:\n  capture:\n    - {id: text, kind: text_delta, fields: [reasoning_content]}\n    - {id: blocks, kind: blocks, field: thinking_blocks, merge_keys: [thinking], close_key: signature, whole_types: [redacted_thinking]}\n  replay:\n    compat_group: marks-thinking-1\n    placements:\n      - {from: blocks, into: {message_field: thinking_blocks}, scope: open_loop, on_missing: refuse}\n";
    let rig = start_rig(
        script,
        RigOptions {
            provider_schemas_extra: schemas.to_string(),
            provider_top_extra: CACHE_MARKS.to_string(),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    run_to_completion(&rig).await;
    assert_no_rejected_request(&rig).await;

    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    let history = messages(&bodies[1]);
    let assistant = history
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant turn");
    assert_eq!(
        assistant["thinking_blocks"],
        json!([{"type": "thinking", "thinking": "step one", "signature": "SIG-A"}])
    );
    assert!(!assistant.to_string().contains(MARK));
    assert_eq!(marked(&bodies[1]), expected_marks(&bodies[1]));
}

// 4 ── cache usage on the turn record ────────────────────────────────────

/// The settled assistant turns of the chain, in order.
async fn settled_turns(rig: &Rig, thread_id: &str) -> Vec<Value> {
    chain_event_list(&rig.h, thread_id)
        .await
        .into_iter()
        .filter(|(_, kind, payload)| kind == "cognition_out" && payload.get("parts").is_some())
        .map(|(_, _, payload)| payload)
        .collect()
}

const WRITE_THEN_READ: &str = "steps:\n  - tool_calls:\n      - { id: call_0, name: noop_noop, arguments: {} }\n    usage: { prompt_tokens: 2051, completion_tokens: 20, cache_read_input_tokens: 0, cache_creation_input_tokens: 2013 }\n  - text: \"done\"\n    usage: { prompt_tokens: 2100, completion_tokens: 20, cache_read_input_tokens: 2013, cache_creation_input_tokens: 0, prompt_tokens_details: { cached_tokens: 2013 } }\n";

#[tokio::test(flavor = "multi_thread")]
async fn cached_read_and_write_counts_are_recorded_beside_the_unchanged_input_count() {
    let rig = start_rig(
        WRITE_THEN_READ,
        RigOptions {
            provider_top_extra: CACHE_MARKS.to_string(),
            provider_usage_extra: USAGE_INCLUDED.to_string(),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let thread_id = run_to_completion(&rig).await;
    let turns = settled_turns(&rig, &thread_id).await;
    assert_eq!(turns.len(), 2);

    let write = &turns[0]["provider_accounting"];
    assert_eq!(write["cache_write_tokens"], 2013, "{write}");
    assert_eq!(write["cache_read_tokens"], 0, "{write}");
    assert!(write["cache_miss_tokens"].is_null(), "{write}");
    let read = &turns[1]["provider_accounting"];
    assert_eq!(read["cache_read_tokens"], 2013, "{read}");
    assert_eq!(read["cache_write_tokens"], 0, "{read}");

    // The input count is the provider's figure, cached tokens included.
    assert_eq!(turns[0]["usage"]["input_tokens"], 2051, "{}", turns[0]);
    assert_eq!(turns[1]["usage"]["input_tokens"], 2100, "{}", turns[1]);
    assert_eq!(read["input_tokens"], 2100, "{read}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_that_reports_no_cache_fields_records_nulls() {
    let rig = start_rig(WRITE_THEN_READ, RigOptions::default())
        .await
        .expect("start rig");
    let thread_id = run_to_completion(&rig).await;
    for turn in settled_turns(&rig, &thread_id).await {
        let accounting = &turn["provider_accounting"];
        for key in [
            "cache_read_tokens",
            "cache_write_tokens",
            "cache_miss_tokens",
        ] {
            assert!(accounting[key].is_null(), "{key}: {accounting}");
        }
    }
}

// 5 ── the double-count guard ────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn cache_counts_above_the_input_that_holds_them_invalidate_the_turn_usage() {
    let script = "steps:\n  - text: \"done\"\n    usage: { prompt_tokens: 2000, completion_tokens: 20, cache_read_input_tokens: 1500, cache_creation_input_tokens: 600 }\n";
    let rig = start_rig(
        script,
        RigOptions {
            provider_usage_extra: USAGE_INCLUDED.to_string(),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let thread_id = run_to_completion(&rig).await;
    let turns = settled_turns(&rig, &thread_id).await;
    let errors = turns[0]["provider_accounting"]["schema_errors"].to_string();
    assert!(errors.contains("exceed input tokens 2000"), "{errors}");
}

/// `(state, budget_charge_nanos)` of every provider attempt in the ledger.
fn ledger_charges(h: &ryeosd::test_support::DaemonHarness) -> Vec<(String, Option<i64>)> {
    let db = h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(ryeos_app::accounting_db::ACCOUNTING_DB_FILENAME);
    ryeos_app::test_support::read_existing_accounting(&db, |connection| {
        let mut statement = connection.prepare(
            "SELECT state, budget_charge_usd_nanos FROM provider_attempt_reservation ORDER BY turn, attempt_number",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

const ONE_WRITE_TURN: &str = "steps:\n  - text: \"done\"\n    usage: { prompt_tokens: 2051, completion_tokens: 20, cache_read_input_tokens: 0, cache_creation_input_tokens: 2013 }\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_cache_dimension_is_billed_only_where_the_tariff_covers_it() {
    // Input and output only: the cached write is recorded, never priced.
    let mut uncovered = start_rig(
        ONE_WRITE_TURN,
        RigOptions {
            header_extra: FINITE_BUDGET,
            paid_provider: true,
            provider_usage_extra: USAGE_SEPARATE.to_string(),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    run_to_completion(&uncovered).await;
    uncovered.h.stop_gracefully().await.expect("stop node");
    let rows = ledger_charges(&uncovered.h);
    assert_eq!(rows.len(), 1, "{rows:?}");
    // 2051 input at 2 per million plus 20 output at 10 per million.
    assert_eq!(
        rows[0],
        ("reconciled".to_string(), Some(4_302_000)),
        "{rows:?}"
    );

    // A tariff that also covers cache write: 2013 more units at 2.5.
    let mut covered = start_rig(
        ONE_WRITE_TURN,
        RigOptions {
            header_extra: FINITE_BUDGET,
            paid_provider: true,
            provider_cache_tariff: true,
            provider_usage_extra: USAGE_SEPARATE.to_string(),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    run_to_completion(&covered).await;
    covered.h.stop_gracefully().await.expect("stop node");
    let rows = ledger_charges(&covered.h);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0],
        ("reconciled".to_string(), Some(9_334_500)),
        "{rows:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tariff_billing_input_and_cache_is_refused_when_input_already_holds_the_cache() {
    let rig = start_rig(
        ONE_WRITE_TURN,
        RigOptions {
            header_extra: FINITE_BUDGET,
            paid_provider: true,
            provider_cache_tariff: true,
            provider_usage_extra: USAGE_INCLUDED.to_string(),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    match launch_on(&rig).await {
        Err(error) => {
            let message = format!("{error:#}");
            assert!(message.contains("cache_included_in_input"), "{message}");
        }
        Ok(thread_id) => {
            let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
            assert_eq!(status, "failed");
            let detail = thread_detail(&rig.h, &thread_id, 1500).await;
            assert!(detail.contains("cache_included_in_input"), "{detail}");
        }
    }
    assert!(rig.stub.requests().await.is_empty());
}
