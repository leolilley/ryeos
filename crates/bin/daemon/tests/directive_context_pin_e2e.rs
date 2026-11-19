//! Recorded and pinned context, end to end against the scripted chat stub.
//!
//! A directive with `before` and `after` context positions records them as
//! pinned inputs. These tests prove what the model is actually sent: the
//! context messages are in the record, a crash resume rebuilds the same
//! request, and a successor keeps the pinned prefix whatever the carried tail.
//! They need populated bundles (`-f bundles=branch`).

use ryeosd::test_support as common;

#[path = "common/stub_rig.rs"]
mod stub_rig;
use stub_rig::*;

use std::path::Path;
use std::time::Duration;

use lillux::crypto::SigningKey;
use serde_json::Value;

const BEFORE_TEXT: &str = "PIN-BEFORE: the output contract is a single sentence.";
const AFTER_TEXT: &str = "PIN-AFTER: end every answer with the word finished.";

/// The directive header that declares both context positions.
const CONTEXT_HEADER: &str = "context:\n  before:\n    - \"knowledge:pin/before\"\n  after:\n    - \"knowledge:pin/after\"\n";

/// Plant a knowledge item at `<project>/.ai/knowledge/<rel>.md`.
fn plant_knowledge(
    project: &Path,
    rel: &str,
    body: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = project.join(format!(".ai/knowledge/{rel}.md"));
    std::fs::create_dir_all(path.parent().expect("knowledge parent dir"))?;
    let stem = Path::new(rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel);
    let category = Path::new(rel)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    let content = format!("```yaml\nname: {stem}\ncategory: \"{category}\"\n```\n\n{body}\n");
    let signed = lillux::signature::sign_content(&content, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

fn plant_context(rig: &Rig) -> anyhow::Result<()> {
    plant_knowledge(
        rig.project.path(),
        "pin/before",
        BEFORE_TEXT,
        &rig.publisher,
    )?;
    plant_knowledge(rig.project.path(), "pin/after", AFTER_TEXT, &rig.publisher)
}

/// The roles and texts of a request's non-system messages.
fn conversation(body: &Value) -> Vec<(String, String)> {
    body.get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
                .map(|m| {
                    (
                        m.get("role")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string(),
                        m.get("content")
                            .map(|c| match c {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The launch batch's inputs: `cognition_in` payloads that carry an origin
/// (turn markers carry none).
async fn input_payloads(rig: &Rig, root: &str) -> Vec<Value> {
    chain_payloads(&rig.h, root, "cognition_in")
        .await
        .into_iter()
        .filter(|payload| payload.get("origin").is_some())
        .collect()
}

/// E1. The context positions are recorded as inputs in the order the model is
/// given them, each pinned, and the first request carries them with their
/// acknowledgements.
#[tokio::test(flavor = "multi_thread")]
async fn context_is_recorded_as_pinned_inputs() {
    let rig = start_rig(
        "steps:\n  - text: done\n",
        RigOptions {
            header_extra: CONTEXT_HEADER,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    plant_context(&rig).expect("plant context");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &id, 2000).await
    );

    let inputs = input_payloads(&rig, &id).await;
    let origins: Vec<&str> = inputs
        .iter()
        .map(|p| p["origin"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(origins, ["context", "launch", "context"], "{inputs:?}");
    for input in &inputs {
        assert_eq!(input["pinned"], true, "{input}");
    }
    assert!(inputs[0]["ack"].is_string(), "{}", inputs[0]);

    let requests = rig.stub.requests().await;
    let first = conversation(&requests[0].body);
    let roles: Vec<&str> = first.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", "user", "assistant"],
        "{first:?}"
    );
    assert!(first[0].1.contains(BEFORE_TEXT), "{first:?}");
    assert!(first[3].1.contains(AFTER_TEXT), "{first:?}");
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
/// stayed the same across several polls (a successor appears a moment after
/// its predecessor settles).
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

/// The indices of the requests that open a successor: a request with no more
/// messages than the one before it. A run's own requests only ever grow, and
/// a successor starts from a trimmed conversation.
fn successor_starts(requests: &[Value]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut previous = 0usize;
    for (index, body) in requests.iter().enumerate() {
        let count = messages_of(body).len();
        if index > 0 && count <= previous {
            starts.push(index);
        }
        previous = count;
    }
    starts
}

/// The pinned prefix of a request: everything up to and including the
/// acknowledgement of the `after` position (the system text, if any, then
/// before, its acknowledgement, the task, after, its acknowledgement).
fn pinned_prefix(body: &Value) -> Vec<Value> {
    let messages = messages_of(body);
    let end = messages
        .iter()
        .position(|m| {
            role_of(m) == "assistant"
                && m.get("content")
                    .map(Value::to_string)
                    .unwrap_or_default()
                    .contains("Noted. I will apply this guidance.")
        })
        .expect("the request carries the after acknowledgement");
    messages[..=end].to_vec()
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

/// Assistant messages that are real turns: not the acknowledgements the
/// runtime writes after a context position.
fn turn_count(body: &Value) -> usize {
    messages_of(body)
        .iter()
        .filter(|m| role_of(m) == "assistant")
        .filter(|m| {
            let text = m.get("content").map(Value::to_string).unwrap_or_default();
            !text.contains("Understood. I will use this context.")
                && !text.contains("Noted. I will apply this guidance.")
        })
        .count()
}

const CHAIN_HEADER: &str = "continuation:\n  carry_turns: 2\ncontext:\n  before:\n    - \"knowledge:pin/before\"\n  after:\n    - \"knowledge:pin/after\"\n";

/// A small window forces a successor about every ten bulk calls.
const CHAIN_SCRIPT: &str = "steps:\n  - repeat: 60\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n  - repeat: 5\n    text: done\n";

/// E3 and E4. A run forced through several continuations keeps its pinned
/// prefix byte for byte, carries at most `carry_turns` of the recent turns,
/// and every successor starts with a user message and paired tool calls.
#[tokio::test(flavor = "multi_thread")]
async fn successors_keep_the_pinned_prefix_and_a_well_formed_start() {
    let rig = start_rig(
        CHAIN_SCRIPT,
        RigOptions {
            context_window: 2000,
            header_extra: CHAIN_HEADER,
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    plant_context(&rig).expect("plant context");
    let id = launch_on(&rig).await.expect("launch");
    let chain = wait_chain_settled(&rig, &id).await;
    let bodies: Vec<Value> = rig
        .stub
        .requests()
        .await
        .into_iter()
        .map(|r| r.body)
        .collect();
    let starts = successor_starts(&bodies);
    assert!(
        starts.len() >= 3,
        "the window should force at least three successors; chain={chain:?} starts={starts:?} sizes={:?}",
        bodies
            .iter()
            .map(|b| messages_of(b).len())
            .collect::<Vec<_>>()
    );

    let first_prefix = pinned_prefix(&bodies[0]);
    assert!(first_prefix.len() >= 5, "{first_prefix:?}");
    assert!(
        first_prefix
            .iter()
            .any(|m| m.to_string().contains(BEFORE_TEXT))
    );
    assert!(
        first_prefix
            .iter()
            .any(|m| m.to_string().contains(AFTER_TEXT))
    );
    for &start in &starts {
        let label = format!("successor request {start}");
        assert_eq!(pinned_prefix(&bodies[start]), first_prefix, "{label}");
        assert!(turn_count(&bodies[start]) <= 2, "{label}");
        assert_valid_start(&bodies[start], &label);
    }
}

/// E5. An operator follow-up is pinned: after it, a chain longer than
/// `carry_turns` still carries the follow-up text in every later successor.
#[tokio::test(flavor = "multi_thread")]
async fn an_operator_follow_up_survives_a_long_chain() {
    let script = "steps:\n  - text: first\n  - repeat: 60\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n  - repeat: 5\n    text: done\n";
    let rig = start_rig(
        script,
        RigOptions {
            context_window: 2000,
            header_extra: CHAIN_HEADER,
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    plant_context(&rig).expect("plant context");
    let first = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &first, Duration::from_secs(120)).await;
    assert_eq!(status, "completed");
    let follow_up_text = "FOLLOW-UP-TASK: now summarise the marker results.";
    let (post_status, body) = rig
        .h
        .post_execute(
            "service:threads/input",
            &rig.project_str(),
            serde_json::json!({
                "input": follow_up_text,
                "target": {"kind": "thread", "thread_id": first},
            }),
        )
        .await
        .expect("post threads.input");
    assert_eq!(post_status, reqwest::StatusCode::OK, "{body:#}");
    let chain = wait_chain_settled(&rig, &first).await;
    let bodies: Vec<Value> = rig
        .stub
        .requests()
        .await
        .into_iter()
        .map(|r| r.body)
        .collect();
    // Request 0 is the first thread's, request 1 the follow-up's own start;
    // every later start is a machine successor of the follow-up thread.
    let starts: Vec<usize> = successor_starts(&bodies)
        .into_iter()
        .filter(|start| *start > 1)
        .collect();
    assert!(
        starts.len() >= 2,
        "the window should force at least two successors; chain={chain:?} starts={starts:?}"
    );
    assert!(
        bodies[1].to_string().contains(follow_up_text),
        "the follow-up thread's own start carries the follow-up"
    );
    for &start in &starts {
        let label = format!("successor request {start}");
        assert!(
            bodies[start].to_string().contains(follow_up_text),
            "{label}"
        );
        assert!(bodies[start].to_string().contains(BEFORE_TEXT), "{label}");
        assert_valid_start(&bodies[start], &label);
    }
}

/// The provider entry's rejection rows for the stub: a context-length refusal.
const REJECTION_ROWS: &str = "request_rejections:\n  - code: provider_context_length_exceeded\n    statuses: [400, 413]\n    body_contains_any: [\"context length\", \"too long\"]\n";

/// A `provider_error` hook that runs the marker tool, so the test can see the
/// event fire.
const PROVIDER_ERROR_HOOK: &str = "hooks:\n  - id: on-rejection\n    event: provider_error\n    result: observation\n    action:\n      primary: execute\n      item_id: \"tool:marker/marker\"\n      ref_bindings: {}\n      params: {}\n";

/// The failure code recorded on a settled thread.
async fn failure_code(rig: &Rig, thread_id: &str) -> String {
    let record = rig
        .h
        .get_thread(thread_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    fn find_code(value: &Value) -> Option<String> {
        match value {
            Value::Object(map) => map
                .get("code")
                .and_then(Value::as_str)
                .filter(|code| code.starts_with("provider_") || *code == "directive_failed")
                .map(str::to_owned)
                .or_else(|| map.values().find_map(find_code)),
            Value::Array(items) => items.iter().find_map(find_code),
            _ => None,
        }
    }
    find_code(&record).unwrap_or_else(|| format!("no failure code in {record}"))
}

/// E6. A refusal the provider entry names is a typed failure that is not
/// retried and reaches the directive's `provider_error` hook; a refusal it does
/// not name stays the generic failure.
#[tokio::test(flavor = "multi_thread")]
async fn a_context_length_refusal_is_a_typed_failure() {
    let script = "steps:\n  - fail: { status: 400, body: \"This model's maximum context length is 8192 tokens\" }\n  - text: unreachable\n";
    let rig = start_rig(
        script,
        RigOptions {
            header_extra: PROVIDER_ERROR_HOOK,
            provider_top_extra: REJECTION_ROWS.to_string(),
            // Even a status the node would retry is final when a row names it.
            exec_yaml: Some(
                execution_with_timeout(60)
                    .replace("[429, 500, 502, 503]", "[400, 429, 500, 502, 503]"),
            ),
            marker_sleep_seconds: 0,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    assert_eq!(
        status,
        "failed",
        "{}",
        thread_detail(&rig.h, &id, 2000).await
    );
    assert_eq!(
        failure_code(&rig, &id).await,
        "provider_context_length_exceeded"
    );
    assert_eq!(
        rig.stub.requests().await.len(),
        1,
        "a named refusal is not retried"
    );
    assert_eq!(rig.marker_count(), 1, "the provider_error hook ran once");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_the_entry_does_not_name_stays_generic() {
    let script = "steps:\n  - fail: { status: 400, body: \"malformed request body\" }\n";
    let rig = start_rig(
        script,
        RigOptions {
            header_extra: PROVIDER_ERROR_HOOK,
            provider_top_extra: REJECTION_ROWS.to_string(),
            marker_sleep_seconds: 0,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    assert_eq!(status, "failed");
    assert_eq!(failure_code(&rig, &id).await, "directive_failed");
    assert_eq!(rig.marker_count(), 0, "no hook for an unnamed refusal");
}

/// E6, graph side. A parent graph that follows the refused directive reads the
/// child's failure code as `state._outcome.failure_code` on its `on_error`
/// path.
#[tokio::test(flavor = "multi_thread")]
async fn a_parent_graph_branches_on_the_childs_failure_code() {
    let script = "steps:\n  - fail: { status: 413, body: \"request entity too large: prompt is too long\" }\n";
    let rig = start_rig(
        script,
        RigOptions {
            provider_top_extra: REJECTION_ROWS.to_string(),
            graph_vault: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    plant_graph(
        &rig,
        "recover",
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.probe/run
      - "ryeos.execute.tool.*"
config:
  start: work
  nodes:
    work:
      node_type: action
      follow: true
      action:
        item_id: "directive:probe/run"
        ref_bindings: { model: "directive:probe/run" }
        params: { name: probe }
      on_error: recover
      next: { type: unconditional, to: done }
    recover:
      node_type: return
      output:
        route: "recover"
        failure_code: "${state._outcome.failure_code}"
    done:
      node_type: return
      output:
        route: "done"
"#,
    )
    .expect("plant graph");
    let id = launch_graph_on(&rig, "graph:recover", serde_json::json!({}))
        .await
        .expect("launch graph");
    let give_up = std::time::Instant::now() + Duration::from_secs(180);
    let mut chain;
    loop {
        chain = chain_threads(&rig, &id).await;
        let finished = chain.iter().any(|(_, s)| s == "completed")
            && chain.iter().all(|(_, s)| TERMINAL.contains(&s.as_str()));
        if finished || std::time::Instant::now() > give_up {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut result_text = String::new();
    for (thread, status) in &chain {
        if status == "completed" {
            result_text.push_str(&thread_detail(&rig.h, thread, 6000).await);
        }
    }
    assert!(
        result_text.contains("recover") && result_text.contains("provider_context_length_exceeded"),
        "the recovery node read the failure code: chain={chain:?} {result_text}"
    );
}
