//! `/execute/launch` accepted-mode durability tests.

mod common;

use std::time::{Duration, Instant};

use common::DaemonHarness;
use serde_json::{json, Value};

fn unwrap_result(status: reqwest::StatusCode, body: &Value, ctx: &str) -> Value {
    assert!(
        status.is_success(),
        "{ctx}: expected success, got {status}; body={body}"
    );
    body.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{ctx}: response had no result field; body={body}"))
}

async fn thread_get(h: &DaemonHarness, thread_id: &str) -> Value {
    let (status, body) = h
        .post_execute(
            "service:threads/get",
            ".",
            json!({ "thread_id": thread_id }),
        )
        .await
        .expect("post threads/get");
    unwrap_result(status, &body, "threads.get")
}

async fn wait_for_thread(h: &DaemonHarness, thread_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let result = thread_get(h, thread_id).await;
        if !result.is_null() {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "accepted thread_id {thread_id} never became inspectable"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_returns_inspectable_thread_id() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project_path = h.user_space.path().to_string_lossy().into_owned();

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "tool:ryeos/core/identity/public_key",
                "project_path": project_path,
                "parameters": {},
                "launch_mode": "accepted"
            }),
        )
        .await
        .expect("post /execute/launch");

    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "body={body}");
    assert_eq!(body.get("status").and_then(Value::as_str), Some("accepted"));
    let thread_id = body
        .get("thread_id")
        .and_then(Value::as_str)
        .expect("accepted response thread_id");

    let thread = wait_for_thread(&h, thread_id).await;
    assert_eq!(
        thread
            .get("thread")
            .and_then(|thread| thread.get("thread_id"))
            .and_then(Value::as_str),
        Some(thread_id),
        "threads.get returned unexpected thread: {thread}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_non_tool_ref_does_not_return_phantom_thread_id() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project_path = h.user_space.path().to_string_lossy().into_owned();

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "service:system/status",
                "project_path": project_path,
                "parameters": {},
                "launch_mode": "accepted"
            }),
        )
        .await
        .expect("post /execute/launch non-tool ref");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(
        body.get("error").and_then(Value::as_str),
        Some("launch_mode='accepted' currently supports tool refs only"),
        "unexpected non-tool rejection body: {body}"
    );
    assert!(
        body.get("thread_id").is_none(),
        "non-tool ref response must not include thread_id: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_invalid_item_does_not_return_phantom_thread_id() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project_path = h.user_space.path().to_string_lossy().into_owned();

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "tool:no/such-tool",
                "project_path": project_path,
                "parameters": {},
                "launch_mode": "accepted"
            }),
        )
        .await
        .expect("post /execute/launch invalid item");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body={body}");
    assert!(
        body.get("thread_id").is_none(),
        "invalid item response must not include thread_id: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_subprocess_terminal_execution_is_bad_request() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = h
        .post_execute(
            "tool:ryeos/core/subprocess/execute",
            ".",
            json!({ "command": "/bin/true" }),
        )
        .await
        .expect("post direct subprocess terminal");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some("root_executor_missing"),
        "unexpected direct terminal rejection body: {body}"
    );
    let error = body.get("error").and_then(Value::as_str).unwrap_or("");
    assert!(error.contains("@subprocess"), "missing remediation: {body}");
    assert!(error.contains("config"), "missing config guidance: {body}");
}
