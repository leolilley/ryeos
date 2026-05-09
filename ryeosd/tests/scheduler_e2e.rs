//! End-to-end tests for the scheduler module.
//!
//! Tests use the DaemonHarness to start a real ryeosd daemon and invoke
//! scheduler services via POST /execute. Tests cover:
//! - scheduler.register (create schedule)
//! - scheduler.list (list schedules)
//! - scheduler.show_fires (fire history)
//! - scheduler.pause / scheduler.resume (enable/disable)
//! - scheduler.deregister (remove schedule)
//!
//! Note: These tests do NOT verify that fires actually dispatch — that
//! would require either waiting for a cron/interval to tick or using an
//! `at` schedule with a near-future timestamp, both of which make tests
//! slow and flaky. Instead, these tests validate the CRUD surface and
//! service wiring.

mod common;

use common::DaemonHarness;
use serde_json::{json, Value};

/// Convenience: POST /execute and unwrap.
async fn exec(h: &DaemonHarness, item_ref: &str, params: Value) -> (reqwest::StatusCode, Value) {
    h.post_execute(item_ref, ".", params)
        .await
        .expect("post /execute")
}

/// Convenience: assert success and return `result` field.
fn unwrap_result(status: reqwest::StatusCode, body: &Value, ctx: &str) -> Value {
    assert!(
        status.is_success(),
        "{ctx}: expected 200, got {status}; body={body}"
    );
    body.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{ctx}: response had no `result` field; body={body}"))
}

// ── Register + List ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_register_and_list() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    // Register an interval schedule
    let (status, body) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "test-interval",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "3600",
        "timezone": "UTC",
    })).await;
    let result = unwrap_result(status, &body, "scheduler.register");
    assert_eq!(result["schedule_id"], "test-interval");
    assert_eq!(result["schedule_type"], "interval");
    assert_eq!(result["created"], true);

    // List schedules — should contain our new one
    let (status, body) = exec(&h, "service:scheduler/list", json!({})).await;
    let result = unwrap_result(status, &body, "scheduler.list");
    let schedules = result["schedules"].as_array().expect("schedules array");
    assert!(
        schedules.iter().any(|s| s["schedule_id"] == "test-interval"),
        "list should contain test-interval; got {result}"
    );
}

// ── Register duplicate updates ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_register_update_existing() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    // Create
    let (status, _) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "update-test",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "60",
    })).await;
    assert!(status.is_success());

    // Update with different expression
    let (status, body) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "update-test",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "120",
    })).await;
    let result = unwrap_result(status, &body, "scheduler.register update");
    assert_eq!(result["created"], false, "should be update, not create");
    assert_eq!(result["expression"], "120");
}

// ── Pause + Resume ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_pause_and_resume() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    // Create
    exec(&h, "service:scheduler/register", json!({
        "schedule_id": "pause-test",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "60",
    })).await;

    // Pause
    let (status, body) = exec(&h, "service:scheduler/pause", json!({
        "schedule_id": "pause-test",
    })).await;
    let result = unwrap_result(status, &body, "scheduler.pause");
    assert_eq!(result["schedule_id"], "pause-test");
    assert_eq!(result["enabled"], false);

    // Resume
    let (status, body) = exec(&h, "service:scheduler/resume", json!({
        "schedule_id": "pause-test",
    })).await;
    let result = unwrap_result(status, &body, "scheduler.resume");
    assert_eq!(result["schedule_id"], "pause-test");
    assert_eq!(result["enabled"], true);
}

// ── Show Fires (empty for new schedule) ──────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_show_fires_empty() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    exec(&h, "service:scheduler/register", json!({
        "schedule_id": "fires-test",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "86400",
    })).await;

    let (status, body) = exec(&h, "service:scheduler/show_fires", json!({
        "schedule_id": "fires-test",
    })).await;
    let result = unwrap_result(status, &body, "scheduler.show_fires");
    assert_eq!(result["total"], 0);
    assert!(result["fires"].as_array().unwrap().is_empty());
}

// ── Deregister ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_deregister_removes_schedule() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    // Create
    exec(&h, "service:scheduler/register", json!({
        "schedule_id": "deleteme",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "60",
    })).await;

    // Deregister
    let (status, body) = exec(&h, "service:scheduler/deregister", json!({
        "schedule_id": "deleteme",
    })).await;
    let result = unwrap_result(status, &body, "scheduler.deregister");
    assert_eq!(result["schedule_id"], "deleteme");

    // List should not contain it
    let (_, body) = exec(&h, "service:scheduler/list", json!({})).await;
    let result = body.get("result").unwrap();
    let schedules = result["schedules"].as_array().expect("schedules array");
    assert!(
        !schedules.iter().any(|s| s["schedule_id"] == "deleteme"),
        "list should not contain deleteme after deregister"
    );
}

// ── Register with cron expression ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_register_cron_schedule() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "cron-test",
        "item_ref": "directive:test/hello",
        "schedule_type": "cron",
        "expression": "0 0 * * * *",
        "timezone": "America/New_York",
        "overlap_policy": "cancel_previous",
        "misfire_policy": "fire_once_now",
    })).await;
    let result = unwrap_result(status, &body, "scheduler.register cron");
    assert_eq!(result["schedule_type"], "cron");
    assert_eq!(result["timezone"], "America/New_York");
    assert_eq!(result["overlap_policy"], "cancel_previous");
    assert_eq!(result["misfire_policy"], "fire_once_now");
}

// ── Register validation errors ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_register_rejects_bad_expression() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "bad-expr",
        "item_ref": "directive:test/hello",
        "schedule_type": "cron",
        "expression": "not a cron expression",
    })).await;
    assert!(
        !status.is_success(),
        "expected error for bad expression, got {status}; body={body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_register_rejects_past_at() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "past-at",
        "item_ref": "directive:test/hello",
        "schedule_type": "at",
        "expression": "2020-01-01T00:00:00Z",
    })).await;
    assert!(
        !status.is_success(),
        "expected error for past at timestamp, got {status}; body={body}"
    );
}

// ── Deregister nonexistent ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_deregister_nonexistent_fails() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, _) = exec(&h, "service:scheduler/deregister", json!({
        "schedule_id": "no-exist",
    })).await;
    assert!(
        !status.is_success(),
        "expected error for deregistering nonexistent schedule"
    );
}

// ── Pause nonexistent ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_pause_nonexistent_fails() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, _) = exec(&h, "service:scheduler/pause", json!({
        "schedule_id": "no-exist",
    })).await;
    assert!(
        !status.is_success(),
        "expected error for pausing nonexistent schedule"
    );
}
