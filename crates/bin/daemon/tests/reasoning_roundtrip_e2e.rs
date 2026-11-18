//! Reasoning that must travel back to the provider, end to end against the
//! scripted chat stub: signed thinking blocks, a signature riding a tool call,
//! a block that never closes, a crash mid-loop, a follow-up continuation and
//! reasoning-token accounting. A continuation reuses the provider entry and
//! directive its launch pinned, so a switch of compat group and a refused
//! frame change are covered by unit tests instead.
//!
//! The stub plays a strict provider: a step's `require` makes it answer 400
//! when the request it receives lacks what a real provider would insist on, so
//! "no rejected request" is an assertion about the wire, not about the runtime.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

#[path = "common/stub_rig.rs"]
mod stub_rig;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use stub_rig::{
    Rig, RigOptions, chain_event_list, launch_on, start_rig, thread_detail, wait_terminal,
};

const MAX_WAIT: Duration = Duration::from_secs(150);

fn execution(extra: &str) -> String {
    format!(
        "category: \"ryeos-runtime\"\nretries: 3\nbackoff_base_ms: 100\nbackoff_max_ms: 1000\n{extra}"
    )
}

/// The provider entry of a Claude-shaped gateway: visible reasoning text, and
/// signed thinking blocks that the open tool loop needs back.
fn blocks_schemas(group: &str, scope: &str, on_missing: &str) -> String {
    format!(
        "reasoning:\n  capture:\n    - {{id: text, kind: text_delta, fields: [reasoning_content]}}\n    - {{id: blocks, kind: blocks, field: thinking_blocks, merge_keys: [thinking], close_key: signature, whole_types: [redacted_thinking]}}\n  replay:\n    compat_group: {group}\n    placements:\n      - {{from: blocks, into: {{message_field: thinking_blocks}}, scope: {scope}, on_missing: {on_missing}}}\n"
    )
}

/// The provider entry of a Gemini-shaped gateway: a signature on the first
/// call of a parallel batch.
fn signature_schemas(on_missing: &str) -> String {
    format!(
        "reasoning:\n  capture:\n    - {{id: sig, kind: tool_call_field, field: extra_content, merge: object_merge}}\n  replay:\n    compat_group: gemini-sig-1\n    placements:\n      - {{from: sig, into: {{tool_call_field: extra_content}}, scope: open_loop, on_missing: {on_missing}}}\n"
    )
}

const CLAUDE_GROUP: &str = "claude-thinking-1";

const SIGNED_BLOCKS: &str = "    thinking_blocks:\n      - { thinking: \"step one\", signature: \"SIG-A\" }\n      - { redacted: \"cmVkYWN0ZWQ=\" }\n";

fn signed_blocks_value() -> Value {
    json!([
        {"type": "thinking", "thinking": "step one", "signature": "SIG-A"},
        {"type": "redacted_thinking", "data": "cmVkYWN0ZWQ="}
    ])
}

/// A tool turn carrying signed blocks, then a final answer the strict provider
/// accepts only when the blocks came back and the text echo did not.
fn claude_script() -> String {
    format!(
        "steps:\n  - reasoning: \"think about it\"\n{SIGNED_BLOCKS}    tool_calls:\n      - {{ id: call_a, name: noop_noop, arguments: {{}} }}\n    stream: {{ thinking_chunk_bytes: 4, signature_last: true }}\n  - text: \"all done\"\n    require: {{ assistant_tool_turn_has: [thinking_blocks], assistant_lacks: [reasoning_content] }}\n"
    )
}

async fn run_to_terminal(rig: &Rig) -> (String, String) {
    let thread_id = launch_on(rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    (thread_id, status)
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

fn assistant_messages(body: &Value) -> Vec<&Value> {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect()
}

/// The `parts` of every settled assistant turn of the chain, in order.
async fn turn_parts(rig: &Rig, thread_id: &str) -> Vec<Vec<Value>> {
    chain_event_list(&rig.h, thread_id)
        .await
        .into_iter()
        .filter(|(_, kind, payload)| kind == "cognition_out" && payload.get("parts").is_some())
        .map(|(_, _, payload)| payload["parts"].as_array().expect("parts").clone())
        .collect()
}

async fn failure_message(rig: &Rig, thread_id: &str) -> String {
    chain_event_list(&rig.h, thread_id)
        .await
        .into_iter()
        .find(|(_, kind, _)| kind == "thread_failed")
        .map(|(_, _, payload)| payload["message"].as_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

/// Operator follow-up on a settled thread: launches the continuation
/// successor and returns its thread id.
async fn follow_up(rig: &Rig, from: &str) -> String {
    let (status, body) = rig
        .h
        .post_execute(
            "service:threads/input",
            &rig.project_str(),
            json!({
                "input": "continue",
                "target": {"kind": "thread", "thread_id": from},
            }),
        )
        .await
        .expect("post threads.input");
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "follow-up refused: {body:#}"
    );
    body.pointer("/result/thread_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("follow-up carried no thread_id: {body:#}"))
        .to_string()
}

// 1 ── Claude-shaped round trip ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn signed_thinking_blocks_come_back_in_order_and_the_text_echo_does_not() {
    let rig = start_rig(
        &claude_script(),
        RigOptions {
            provider_schemas_extra: blocks_schemas(CLAUDE_GROUP, "open_loop", "refuse"),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 600).await
    );
    assert_no_rejected_request(&rig).await;

    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    let assistants = assistant_messages(&bodies[1]);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0]["thinking_blocks"], signed_blocks_value());
    assert!(assistants[0].get("reasoning_content").is_none());
    assert_eq!(assistants[0]["tool_calls"][0]["id"], "call_a");

    let turns = turn_parts(&rig, &thread_id).await;
    let first = &turns[0];
    let text: Vec<&Value> = first
        .iter()
        .filter(|part| part["type"] == "reasoning" && part.get("text").is_some())
        .collect();
    assert_eq!(text.len(), 1, "{first:?}");
    assert_eq!(text[0]["text"], "think about it");
    let artefacts: Vec<&Value> = first
        .iter()
        .filter(|part| part["type"] == "reasoning" && part.get("artefact").is_some())
        .collect();
    assert_eq!(artefacts.len(), 2, "{first:?}");
    for (artefact, expected) in artefacts
        .iter()
        .zip(signed_blocks_value().as_array().unwrap())
    {
        assert_eq!(artefact["artefact"]["capture"], "blocks");
        assert_eq!(artefact["artefact"]["group"], CLAUDE_GROUP);
        assert_eq!(&artefact["artefact"]["value"], expected);
        assert!(artefact.get("text").is_none());
    }
}

// 2 ── Gemini-shaped signature on the first of two parallel calls ────────

#[tokio::test(flavor = "multi_thread")]
async fn a_signature_goes_back_on_the_first_call_of_the_batch_only() {
    let script = "steps:\n  - tool_calls:\n      - { id: call_a, name: noop_noop, arguments: {}, fields: { extra_content: { google: { thought_signature: \"SIG-G\" } } } }\n      - { id: call_b, name: noop_noop, arguments: {} }\n    stream: { tool_call_index: false }\n  - text: \"done\"\n    require: { tool_call_has: [extra_content] }\n";
    let rig = start_rig(
        script,
        RigOptions {
            provider_schemas_extra: signature_schemas("refuse"),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 600).await
    );
    assert_no_rejected_request(&rig).await;

    let bodies = rig.stub.request_bodies().await;
    let assistants = assistant_messages(&bodies[1]);
    let calls = assistants[0]["tool_calls"].as_array().expect("tool calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["id"], "call_a");
    assert_eq!(
        calls[0]["extra_content"],
        json!({"google": {"thought_signature": "SIG-G"}})
    );
    assert!(calls[1].get("extra_content").is_none(), "{calls:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_owes_a_signature_and_has_none_fails_before_its_tool_runs() {
    let script = "steps:\n  - tool_calls:\n      - { id: call_a, name: marker_marker, arguments: {} }\n  - text: \"never reached\"\n";
    let rig = start_rig(
        script,
        RigOptions {
            provider_schemas_extra: signature_schemas("refuse"),
            marker_sleep_seconds: 1,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "failed");
    assert!(
        failure_message(&rig, &thread_id)
            .await
            .starts_with("reasoning_artefact_missing"),
        "{}",
        thread_detail(&rig.h, &thread_id, 800).await
    );
    assert_eq!(rig.marker_count(), 0, "the proposed tool must not have run");
    // Only live fragments were published for the refused call; the chain holds
    // no settled turn that proposes it.
    assert!(
        turn_parts(&rig, &thread_id).await.is_empty(),
        "a refused turn must leave no settled cognition_out"
    );
    assert_eq!(rig.stub.requests().await.len(), 1);
}

// 3 ── a block that never closes ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_block_cut_before_its_signature_is_dropped_and_the_run_continues() {
    let script = "steps:\n  - reasoning: \"half a thought\"\n    thinking_blocks:\n      - { thinking: \"never closed\" }\n    text: \"first half\"\n    finish_reason: length\n  - text: \"second half\"\n    require: { assistant_lacks: [thinking_blocks, reasoning_content] }\n";
    let rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution(
                "on_output_limit:\n  max_consecutive: 2\n  message: \"Continue from where it stopped.\"\n",
            )),
            provider_schemas_extra: blocks_schemas(CLAUDE_GROUP, "open_loop", "refuse"),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 600).await
    );
    assert_no_rejected_request(&rig).await;
    assert_eq!(rig.stub.requests().await.len(), 2);

    let turns = turn_parts(&rig, &thread_id).await;
    assert!(
        turns[0].iter().all(|part| part.get("artefact").is_none()),
        "the unclosed block must not be recorded: {:?}",
        turns[0]
    );
}

// 4 ── crash mid-loop ────────────────────────────────────────────────────

/// The request bodies of an uninterrupted run of `claude_script`, with the
/// same tool the crashed run uses.
async fn control_bodies() -> Vec<Value> {
    let rig = start_rig(
        &claude_script().replace("noop_noop", "marker_marker"),
        RigOptions {
            marker_sleep_seconds: 1,
            provider_schemas_extra: blocks_schemas(CLAUDE_GROUP, "open_loop", "refuse"),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start control rig");
    let (_, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "completed");
    rig.stub.request_bodies().await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_mid_loop_resumes_with_the_same_request_the_live_run_would_send() {
    let control = control_bodies().await;
    let script = claude_script().replace("noop_noop", "marker_marker");
    let mut rig = start_rig(
        &script,
        RigOptions {
            provider_schemas_extra: blocks_schemas(CLAUDE_GROUP, "open_loop", "refuse"),
            marker_sleep_seconds: 120,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let thread_id = launch_on(&rig).await.expect("launch");
    let deadline = Instant::now() + Duration::from_secs(90);
    while rig.marker_count() == 0 {
        assert!(Instant::now() < deadline, "the tool never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    rig.h.restart().await.expect("kill and restart the node");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 800).await
    );
    assert_no_rejected_request(&rig).await;

    let resumed = rig.stub.request_bodies().await;
    assert_eq!(
        resumed.len(),
        2,
        "the settled first turn must not be asked again"
    );
    let live = assistant_messages(&control[1])[0].clone();
    let after_crash = assistant_messages(&resumed[1])[0].clone();
    // The rebuilt assistant turn is the one the live run sent, byte for byte.
    assert_eq!(
        serde_json::to_vec(&after_crash).unwrap(),
        serde_json::to_vec(&live).unwrap()
    );
    assert_eq!(after_crash["thinking_blocks"], signed_blocks_value());
    // Everything before it is the same history too.
    let position = |body: &Value| {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .position(|message| message["role"] == "assistant")
            .unwrap()
    };
    assert_eq!(position(&control[1]), position(&resumed[1]));
    assert_eq!(
        control[1]["messages"].as_array().unwrap()[..position(&control[1])],
        resumed[1]["messages"].as_array().unwrap()[..position(&resumed[1])]
    );
}

// 5 ── operator follow-up continuation ───────────────────────────────────

/// A completed run whose two turns both carry a signed block, then an operator
/// follow-up; returns the successor's request body.
async fn successor_request(scope: &str) -> Value {
    let script = "steps:\n  - reasoning: \"plan\"\n    thinking_blocks:\n      - { thinking: \"one\", signature: \"SIG-1\" }\n    tool_calls:\n      - { id: call_a, name: noop_noop, arguments: {} }\n  - thinking_blocks:\n      - { thinking: \"two\", signature: \"SIG-2\" }\n    text: \"wrapped up\"\n  - text: \"ok\"\n";
    let rig = start_rig(
        script,
        RigOptions {
            provider_schemas_extra: blocks_schemas(CLAUDE_GROUP, scope, "omit"),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (first, status) = run_to_terminal(&rig).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &first, 600).await
    );
    let successor = follow_up(&rig, &first).await;
    let (status, _) = wait_terminal(&rig.h, &successor, MAX_WAIT).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &successor, 800).await
    );
    assert_no_rejected_request(&rig).await;
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 3);
    bodies[2].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_successor_carries_the_artefacts_its_scope_still_covers() {
    let body = successor_request("all").await;
    let blocks: Vec<&Value> = assistant_messages(&body)
        .into_iter()
        .map(|message| &message["thinking_blocks"])
        .collect();
    assert_eq!(
        blocks,
        [
            &json!([{"type": "thinking", "thinking": "one", "signature": "SIG-1"}]),
            &json!([{"type": "thinking", "thinking": "two", "signature": "SIG-2"}]),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_operator_message_closes_the_open_loop_so_older_artefacts_are_not_sent() {
    let body = successor_request("open_loop").await;
    let assistants = assistant_messages(&body);
    assert_eq!(assistants.len(), 2, "both carried turns are in the history");
    assert!(
        assistants
            .iter()
            .all(|message| message.get("thinking_blocks").is_none()),
        "{assistants:?}"
    );
}

// 8 ── reasoning-token accounting ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn reported_reasoning_tokens_are_recorded_on_the_turn() {
    let script = "steps:\n  - text: \"done\"\n    usage: { prompt_tokens: 20, completion_tokens: 30, completion_tokens_details: { reasoning_tokens: 12 } }\n";
    let rig = start_rig(script, RigOptions::default())
        .await
        .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 600).await
    );
    let settled: Vec<Value> = chain_event_list(&rig.h, &thread_id)
        .await
        .into_iter()
        .filter(|(_, kind, payload)| kind == "cognition_out" && payload.get("parts").is_some())
        .map(|(_, _, payload)| payload)
        .collect();
    let accounting = &settled[0]["provider_accounting"];
    assert_eq!(accounting["reasoning_tokens"], 12, "{accounting}");
    assert_eq!(accounting["output_tokens"], 30, "{accounting}");
}
