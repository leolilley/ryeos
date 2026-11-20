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
        .list_threads(200)
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
    wait_chain_settled_for(rig, root, 300).await
}

async fn wait_chain_settled_for(rig: &Rig, root: &str, seconds: u64) -> Vec<(String, String)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
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

/// A request is a valid start: its first non-system message is a user message,
/// every assistant tool call has its result, and no result is orphaned.
fn assert_valid_start(body: &Value, label: &str) {
    let messages = messages_of(body);
    let first = messages
        .iter()
        .find(|m| role_of(m) != "system")
        .unwrap_or_else(|| panic!("{label}: no conversation"));
    assert_eq!(role_of(first), "user", "{label}");
    let mut calls: Vec<String> = Vec::new();
    let mut answered: Vec<String> = Vec::new();
    for message in &messages {
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            calls.extend(
                tool_calls
                    .iter()
                    .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned)),
            );
        }
        if role_of(message) == "tool" {
            let id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            assert!(calls.contains(&id), "{label}: orphan tool result {id}");
            answered.push(id);
        }
    }
    for call in &calls {
        assert!(answered.contains(call), "{label}: unanswered call {call}");
    }
}

/// The indices of the requests that open a successor: a request with no more
/// messages than the one before it.
fn successor_starts(bodies: &[Value]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut previous = 0usize;
    for (index, body) in bodies.iter().enumerate() {
        let count = messages_of(body).len();
        if index > 0 && count <= previous {
            starts.push(index);
        }
        previous = count;
    }
    starts
}

/// A small window forces a successor about every ten bulk calls.
const LONG_CHAIN: &str = "steps:\n  - repeat: 130\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n  - repeat: 5\n    text: done\n";

/// About ten successors at the 2000-token window. The test daemon's native
/// service window rolls over after about thirteen thread terminals, so a chain
/// of fifteen continuations cannot finish in one test daemon incarnation.
const MEDIUM_CHAIN: &str = "steps:\n  - repeat: 105\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n  - repeat: 5\n    text: done\n";

/// E7 (as far as one test daemon incarnation allows). With the chain limit
/// set in the signed config above the default of 12, a chain of about ten
/// continuations completes, every successor starts well formed, and the
/// limit is not reached.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_chain_runs_under_a_raised_limit() {
    let rig = start_rig(
        MEDIUM_CHAIN,
        RigOptions {
            context_window: 2000,
            header_extra: CONTINUING,
            continuation_yaml: Some(
                "category: \"ryeos-runtime\"\nchain_depth_limit: 40\n".to_string(),
            ),
            limits: Some((2000, 2000)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let chain = wait_chain_settled_for(&rig, &id, 900).await;
    assert!(chain.len() >= 8, "a long chain: {chain:?}");
    assert!(chain.iter().all(|(_, s)| s != "failed"), "{chain:?}");
    assert!(chain.iter().any(|(_, s)| s == "completed"), "{chain:?}");
    let bodies = request_bodies(&rig).await;
    let starts = successor_starts(&bodies);
    assert!(starts.len() >= 7, "{starts:?}");
    for start in starts {
        assert_valid_start(&bodies[start], &format!("successor request {start}"));
    }
}

/// E15 (chain limit). At the signed limit the chain ends with the typed failure.
#[tokio::test(flavor = "multi_thread")]
async fn reaching_the_chain_limit_is_a_typed_failure() {
    let rig = start_rig(
        LONG_CHAIN,
        RigOptions {
            context_window: 2000,
            header_extra: CONTINUING,
            continuation_yaml: Some(
                "category: \"ryeos-runtime\"\nchain_depth_limit: 3\n".to_string(),
            ),
            limits: Some((2000, 2000)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let chain = wait_chain_settled(&rig, &id).await;
    assert_eq!(chain.len(), 4, "{chain:?}");
    let failed = chain
        .iter()
        .find(|(_, status)| status == "failed")
        .unwrap_or_else(|| panic!("a thread failed: {chain:?}"));
    let text = thread_text(&rig, &failed.0).await;
    assert!(text.contains("continuation_chain_limit"), "{text}");
}

/// Clipping. The older large tool results of the kept turns are read clipped
/// after a compaction, the newest whole; the record carries the rule.
#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_clips_the_older_tool_results_of_the_kept_turns() {
    let script = format!("steps:\n  - repeat: 14\n    {TURN}  - text: done\n");
    let rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.4\n  keep_turns: 3\n  min_turns_between: 2\n  summary:\n    mode: none\n  clip:\n    over_bytes: 100\n    head_bytes: 20\n    tail_bytes: 20\n    keep_recent: 1\n",
            )),
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    wait_chain_settled(&rig, &id).await;
    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert!(!records.is_empty());
    assert_eq!(records[0]["clip"]["keep_recent"], 1, "{}", records[0]);
    let bodies = request_bodies(&rig).await;
    let clipped = bodies.iter().find(|body| {
        body.to_string()
            .contains("bytes of this result were clipped when the conversation was compacted")
    });
    let clipped = clipped.expect("a request after the compaction reads a clipped result");
    // The newest tool result is whole.
    let last_tool = messages_of(clipped)
        .into_iter()
        .rev()
        .find(|m| role_of(m) == "tool")
        .expect("a tool result");
    assert!(
        !last_tool.to_string().contains("were clipped"),
        "{last_tool}"
    );
}

/// E3. A successor started right after a failed `directive_return` whose
/// sibling call was answered with the refusal starts well formed.
#[tokio::test(flavor = "multi_thread")]
async fn a_successor_can_start_from_a_failed_return_tail() {
    let script = "steps:\n  - tool_calls:\n      - { id: bad_return, name: directive_return, arguments: {} }\n      - { id: sibling, name: bulk_bulk, arguments: {} }\n  - repeat: 40\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n  - repeat: 5\n    text: done\n";
    let rig = start_rig(
        script,
        RigOptions {
            context_window: 2000,
            header_extra: "continuation:\n  carry_turns: 2\noutputs:\n  - name: answer\n    type: string\n",
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let chain = wait_chain_settled(&rig, &id).await;
    assert!(chain.len() >= 2, "{chain:?}");
    let bodies = request_bodies(&rig).await;
    assert!(
        bodies[1]
            .to_string()
            .contains("tool_not_run_after_failed_return"),
        "the sibling is answered in the next request"
    );
    for start in successor_starts(&bodies) {
        assert_valid_start(&bodies[start], &format!("successor request {start}"));
    }
}

/// E9. An image in a replaced turn is not in the request after the compaction.
#[tokio::test(flavor = "multi_thread")]
async fn an_image_in_a_replaced_turn_leaves_the_conversation() {
    let mut script = String::from(
        "steps:\n  - tool_calls:\n      - { id: shot_call, name: shot_shot, arguments: {} }\n",
    );
    script.push_str(&format!("  - repeat: 12\n    {TURN}  - text: done\n"));
    let rig = start_rig(
        &script,
        RigOptions {
            context_window: 3000,
            image_tools: true,
            provider_top_extra: "image_input:\n  accepts:\n    media_types: [image/png]\n    max_bytes_per_image: 4096\n    max_images_per_request: 4\n    text_part_template: {type: text, text: \"{text}\"}\n    image_part_template: {type: image_url, image_url: {url: \"{data_url}\"}}\n    tool_result_note: \"[{count} image(s) follow in the next message]\"\n    deferred_label: \"Image {index} from tool result {call_id}:\"\n    omitted_note: \"[earlier image omitted: {media_type} {width}x{height}]\"\n    cost: {pixels_per_token: 750, max_tokens: 1600, unknown_dimensions_tokens: 1600}\n".to_string(),
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.6\n  keep_turns: 2\n  min_turns_between: 2\n  summary:\n    mode: none\n",
            )),
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    wait_chain_settled(&rig, &id).await;
    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert!(!records.is_empty());
    let bodies = request_bodies(&rig).await;
    assert!(
        bodies
            .iter()
            .any(|b| b.to_string().contains("data:image/png"))
    );
    let compacted = bodies
        .iter()
        .position(|b| {
            b.to_string()
                .contains("were removed to keep it within the context window")
        })
        .expect("a request after the compaction");
    assert!(
        !bodies[compacted].to_string().contains("data:image/png"),
        "the replaced turn's image is not sent again"
    );
}

/// A hook tool that answers with a control action.
fn plant_control_tool(rig: &Rig, name: &str, action: &str) {
    plant_python_tool(
        rig.project.path(),
        name,
        &format!("print(json.dumps({{'action': '{action}'}}))"),
        &rig.publisher,
    )
    .expect("plant control tool");
}

/// A `context_pressure` hook that runs the named control tool.
const VETO_HOOK: &str = "hooks:\n  - id: pressure-choice\n    event: context_pressure\n    result: control\n    action:\n      primary: execute\n      item_id: \"tool:veto/veto\"\n      ref_bindings: {}\n      params: {}\n";
const PRESSING_HOOK: &str = "hooks:\n  - id: pressure-choice\n    event: context_pressure\n    result: control\n    action:\n      primary: execute\n      item_id: \"tool:pressing/pressing\"\n      ref_bindings: {}\n      params: {}\n";

/// E13. A `context_pressure` hook that answers `skip_compaction` prevents the
/// compaction: no record and no summary request.
#[tokio::test(flavor = "multi_thread")]
async fn a_hook_can_veto_a_compaction() {
    let script = format!("steps:\n  - repeat: 8\n    {TURN}  - text: done\n");
    let rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            header_extra: VETO_HOOK,
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.4\n  keep_turns: 2\n  min_turns_between: 2\n",
            )),
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    plant_control_tool(&rig, "veto", "skip_compaction");
    let id = launch_on(&rig).await.expect("launch");
    wait_chain_settled(&rig, &id).await;
    assert!(
        chain_payloads(&rig.h, &id, "cognition_compaction")
            .await
            .is_empty()
    );
    assert!(
        request_bodies(&rig)
            .await
            .iter()
            .all(|b| !offers_no_tools(b))
    );
}

/// E14. With the policy set to observe, a hook answering `compact` is the
/// only trigger.
#[tokio::test(flavor = "multi_thread")]
async fn a_hook_can_trigger_a_compaction_when_the_policy_only_observes() {
    let script = format!("steps:\n  - repeat: 14\n    {TURN}  - text: done\n");
    let rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            header_extra: PRESSING_HOOK,
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.4\n  keep_turns: 2\n  min_turns_between: 100\n  on_pressure: observe\n  summary:\n    mode: none\n",
            )),
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    plant_control_tool(&rig, "pressing", "compact");
    let id = launch_on(&rig).await.expect("launch");
    wait_chain_settled(&rig, &id).await;
    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert_eq!(records.len(), 1, "{records:?}");
}

/// A refusal after a compaction is already on the path: the successor's start
/// is built from that compaction, and its first act is the overflow
/// compaction, not a refusal of its start.
#[tokio::test(flavor = "multi_thread")]
async fn an_overflow_after_an_earlier_compaction_still_recovers() {
    let script = format!(
        "steps:\n  - repeat: 14\n    {TURN}  - fail: {{ status: 400, body: \"context length exceeded: prompt is too long\" }}\n  - text: done\n"
    );
    let rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.4\n  keep_turns: 2\n  min_turns_between: 2\n  summary:\n    mode: none\n",
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
    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert!(
        records.iter().any(|r| r["trigger"] == "pressure"),
        "a compaction was already on the path: {records:?}"
    );
    assert!(
        records.iter().any(|r| r["trigger"] == "overflow"),
        "the refusal was answered by a compaction: {records:?} chain={chain:?}"
    );
    assert!(chain.iter().any(|(_, s)| s == "continued"), "{chain:?}");
    assert!(chain.iter().any(|(_, s)| s == "completed"), "{chain:?}");
}
