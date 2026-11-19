//! Compaction and fork-and-retry on context overflow, end to end against the
//! scripted chat stub.
//!
//! A compaction replaces the older part of a conversation by a summary and
//! keeps the newest turns whole. These tests prove what the model is sent
//! after one, what the record holds, and how a refused-as-too-long request is
//! answered. They need populated bundles (`-f bundles=branch`).

#[path = "common/stub_rig.rs"]
mod stub_rig;
use stub_rig::*;

use std::time::Duration;

use serde_json::Value;

/// A step that is both a normal turn (it calls the bulk tool) and a usable
/// summary (it carries text), so the same script answers either request.
const TURN: &str = "text: \"SUMMARY-TEXT\"\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n";

/// The provider entry's rejection rows for the stub: a context-length refusal.
const REJECTION_ROWS: &str = "request_rejections:\n  - code: provider_context_length_exceeded\n    statuses: [400, 413]\n    body_contains_any: [\"context length\", \"too long\"]\n";

/// A directive that may continue, so an overflow recovery can fork.
const CONTINUING: &str = "continuation:\n  carry_turns: 2\n";

fn continuation_yaml(compaction: &str) -> String {
    format!("category: \"ryeos-runtime\"\ncompaction:\n  enabled: true\n{compaction}")
}

/// The threads of one chain with their statuses.
async fn chain_threads(rig: &Rig, root: &str) -> Vec<(String, String)> {
    rig.h
        .list_threads(2000)
        .await
        .map(|listed| {
            listed
                .into_iter()
                .filter(|t| t.chain_root_id == root || t.thread_id == root)
                .map(|t| (t.thread_id, t.status))
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until every thread of the chain is terminal and the thread count has
/// stayed the same across several polls.
async fn wait_chain_settled(rig: &Rig, root: &str) -> Vec<(String, String)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    let mut stable = 0u32;
    let mut last_count = 0usize;
    loop {
        let chain = chain_threads(rig, root).await;
        let terminal =
            !chain.is_empty() && chain.iter().all(|(_, s)| TERMINAL.contains(&s.as_str()));
        if terminal && chain.len() == last_count {
            stable += 1;
        } else {
            stable = 0;
        }
        last_count = chain.len();
        if stable >= 6 || std::time::Instant::now() >= deadline {
            return chain;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn messages_of(body: &Value) -> Vec<Value> {
    body.get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn role_of(message: &Value) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or("?")
}

/// Whether a request offers no tools: a compaction's summary call.
fn offers_no_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

async fn request_bodies(rig: &Rig) -> Vec<Value> {
    rig.stub
        .requests()
        .await
        .into_iter()
        .map(|request| request.body)
        .collect()
}

async fn thread_text(rig: &Rig, thread_id: &str) -> String {
    rig.h
        .get_thread(thread_id)
        .await
        .ok()
        .flatten()
        .map(|record| record.to_string())
        .unwrap_or_default()
}

/// E4 without a model: live context reaching the trigger compacts in place. The
/// record names the replaced and kept turns, the next request is the pinned
/// task, the fixed note and the kept turns, and the run completes.
#[tokio::test(flavor = "multi_thread")]
async fn live_context_at_the_trigger_compacts_in_place() {
    let script = format!("steps:\n  - repeat: 14\n    {TURN}  - text: done\n");
    let rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.4\n  keep_turns: 2\n  min_turns_between: 2\n  summary:\n    mode: none\n",
            )),
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let chain = wait_chain_settled(&rig, &id).await;
    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert!(
        !records.is_empty(),
        "a compaction was recorded; chain={chain:?} requests={}",
        rig.stub.requests().await.len()
    );
    let first = &records[0];
    assert_eq!(first["trigger"], "pressure", "{first}");
    assert_eq!(first["summary_model_call"], false, "{first}");
    assert!(
        first["kept_from_turn"].as_u64() > first["replaces_to_turn"].as_u64(),
        "{first}"
    );
    assert_eq!(first["pinned"].as_array().map(Vec::len), Some(1), "{first}");

    let bodies = request_bodies(&rig).await;
    // No summary request: mode none makes no model call.
    assert!(bodies.iter().all(|body| !offers_no_tools(body)));
    // The request after the compaction is shorter than the one before it, and
    // carries the fixed note.
    let counts: Vec<usize> = bodies.iter().map(|body| messages_of(body).len()).collect();
    let drop_at = counts
        .windows(2)
        .position(|pair| pair[1] < pair[0])
        .map(|index| index + 1)
        .unwrap_or_else(|| panic!("a request is shorter than its predecessor: {counts:?}"));
    let after = messages_of(&bodies[drop_at]);
    assert!(
        after.iter().any(|m| m
            .to_string()
            .contains("were removed to keep it within the context window")),
        "{after:?}"
    );
    assert_eq!(
        role_of(
            after
                .iter()
                .find(|m| role_of(m) != "system")
                .expect("a conversation")
        ),
        "user"
    );
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(60)).await;
    assert_eq!(status, "completed", "{}", thread_text(&rig, &id).await);
}

/// E4 with a model: the summary call offers no tools, carries the policy's
/// instruction as system text, and its answer is the summary the next request
/// holds.
#[tokio::test(flavor = "multi_thread")]
async fn the_summary_call_has_no_tools_and_its_text_is_carried() {
    let script = format!("steps:\n  - repeat: 14\n    {TURN}  - text: done\n");
    let rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.4\n  keep_turns: 2\n  min_turns_between: 2\n  summary:\n    instruction: \"SUMMARIZE-THE-RANGE\"\n",
            )),
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let chain = wait_chain_settled(&rig, &id).await;
    let bodies = request_bodies(&rig).await;
    let summary_requests: Vec<&Value> =
        bodies.iter().filter(|body| offers_no_tools(body)).collect();
    assert!(
        !summary_requests.is_empty(),
        "a summary request was made; chain={chain:?}"
    );
    let system = messages_of(summary_requests[0])
        .into_iter()
        .find(|m| role_of(m) == "system")
        .map(|m| m.to_string())
        .unwrap_or_default();
    assert!(system.contains("SUMMARIZE-THE-RANGE"), "{system}");

    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert!(!records.is_empty());
    assert_eq!(records[0]["summary_model_call"], true, "{}", records[0]);
    assert!(records[0]["turn"].as_u64().unwrap_or(0) > 0);
    assert!(records[0].to_string().contains("SUMMARY-TEXT"));

    // The summary turn is recorded as a turn of the run with its own finish,
    // and the request after the compaction holds the summary text once as a
    // user message.
    let outs = chain_payloads(&rig.h, &id, "cognition_out").await;
    assert!(
        outs.iter()
            .any(|out| out["finish"] == "compaction_summary_pressure"),
        "the summary turn is in the record"
    );
    let compacted_request = bodies
        .iter()
        .find(|body| {
            messages_of(body)
                .iter()
                .any(|m| role_of(m) == "user" && m.to_string().contains("SUMMARY-TEXT"))
        })
        .expect("a request carries the summary");
    assert!(!offers_no_tools(compacted_request));
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(60)).await;
    assert_eq!(status, "completed", "{}", thread_text(&rig, &id).await);
}

/// E11. A request the provider refuses as too long: the failing thread settles
/// `continued`, its successor's first act is the compaction (one summary
/// request), and its first request is the retried one.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_request_forks_into_a_successor_that_compacts_and_retries() {
    let script = format!(
        "steps:\n  - repeat: 6\n    {TURN}  - fail: {{ status: 400, body: \"context length exceeded: prompt is too long\" }}\n  - text: \"RECOVERY-SUMMARY\"\n  - text: done\n"
    );
    let rig = start_rig(
        &script,
        RigOptions {
            // The trigger never fires: only the refusal compacts.
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.99\n  keep_turns: 2\n  summary:\n    mode: model\n",
            )),
            header_extra: CONTINUING,
            provider_top_extra: REJECTION_ROWS.to_string(),
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let chain = wait_chain_settled(&rig, &id).await;
    assert!(chain.len() >= 2, "the refusal forked: {chain:?}");
    assert!(chain.iter().any(|(_, s)| s == "continued"), "{chain:?}");
    assert!(chain.iter().any(|(_, s)| s == "completed"), "{chain:?}");
    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["trigger"], "overflow");
    assert_eq!(records[0]["overflow_recoveries"], 1);

    let bodies = request_bodies(&rig).await;
    // Six turns, the refused request, the summary request, the retried request.
    assert_eq!(bodies.len(), 9, "{}", bodies.len());
    assert!(offers_no_tools(&bodies[7]), "request 7 is the summary call");
    let retried = &bodies[8];
    assert!(!offers_no_tools(retried));
    assert!(retried.to_string().contains("RECOVERY-SUMMARY"));
    // The retried request ends where the refused one ended: on a tool result.
    let refused_tail = messages_of(&bodies[6]);
    let retried_tail = messages_of(retried);
    assert_eq!(
        role_of(refused_tail.last().expect("a message")),
        role_of(retried_tail.last().expect("a message"))
    );
    assert!(retried_tail.len() < refused_tail.len());
}

/// E12. A retried request refused again is the typed failure, with no third
/// request and no second recovery.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_refused_again_after_the_recovery_is_the_typed_failure() {
    let script = format!(
        "steps:\n  - repeat: 6\n    {TURN}  - fail: {{ status: 400, body: \"context length exceeded: prompt is too long\" }}\n  - text: \"RECOVERY-SUMMARY\"\n  - fail: {{ status: 400, body: \"context length exceeded: prompt is too long\" }}\n  - text: unreachable\n"
    );
    let rig = start_rig(
        &script,
        RigOptions {
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.99\n  keep_turns: 2\n  summary:\n    mode: model\n",
            )),
            header_extra: CONTINUING,
            provider_top_extra: REJECTION_ROWS.to_string(),
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let chain = wait_chain_settled(&rig, &id).await;
    assert_eq!(chain.len(), 2, "{chain:?}");
    let failed = chain
        .iter()
        .find(|(_, status)| status == "failed")
        .unwrap_or_else(|| panic!("a thread failed: {chain:?}"));
    let text = thread_text(&rig, &failed.0).await;
    assert!(text.contains("provider_context_length_exceeded"), "{text}");
    assert_eq!(rig.stub.requests().await.len(), 9);
}

/// E15. With continuation off and the conversation over the window threshold,
/// the run ends with the typed failure instead of finalising with its last
/// message as the answer.
#[tokio::test(flavor = "multi_thread")]
async fn over_the_window_without_continuation_is_a_typed_failure() {
    let script = format!("steps:\n  - repeat: 40\n    {TURN}  - text: done\n");
    let rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    assert_eq!(status, "failed", "{}", thread_text(&rig, &id).await);
    let text = thread_text(&rig, &id).await;
    assert!(text.contains("context_window_exhausted"), "{text}");
}
