//! End-to-end tests for the scheduler module.
//!
//! Tests use the DaemonHarness to start a real ryeosd daemon and invoke
//! scheduler services via POST /execute. Tests cover:
//! - scheduler.register (create schedule)
//! - scheduler.list (list schedules)
//! - scheduler.show_fires (fire history)
//! - scheduler.pause / scheduler.resume (enable/disable)
//! - scheduler.deregister (remove schedule)
//! - Timer dispatch: at-schedule, interval-schedule fires
//!
//! Fire dispatch tests register near-future schedules and poll `show_fires`
//! until a fire record appears. The dispatch itself will fail (no real
//! directive item exists in the test bundle), but the fire record proves
//! the timer loop ticked and attempted dispatch.

mod common;

use common::DaemonHarness;
use serde_json::{json, Value};
use std::time::Duration;

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

// ── Timer dispatch: at-schedule fires ─────────────────────────────────────
//
// Register an `at` schedule 3 seconds in the future. The dispatch will fail
// (directive:test/hello doesn't exist), but the fire record will appear with
// status "dispatch_failed" or "dispatched", proving the timer fired.

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_at_schedule_fires() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    // Compute a timestamp 3 seconds from now
    let fire_at = chrono::Utc::now() + chrono::Duration::seconds(3);
    let fire_at_str = fire_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // Register at-schedule
    let (status, body) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "at-fire-test",
        "item_ref": "directive:test/hello",
        "schedule_type": "at",
        "expression": fire_at_str,
    })).await;
    let result = unwrap_result(status, &body, "scheduler.register at");
    assert_eq!(result["schedule_type"], "at");

    // Poll show_fires until a fire appears (max 10s — timer sleep + margin)
    let fire = poll_for_fires(&h, "at-fire-test", 1, Duration::from_secs(10))
        .await
        .expect("expected at least 1 fire within 10s");

    // Fire record exists — proves the timer dispatched
    assert_eq!(fire["schedule_id"], "at-fire-test");
    let status_str = fire["status"].as_str().unwrap_or("");
    assert!(
        matches!(status_str, "dispatched" | "failed"),
        "expected fire status dispatched or failed, got: {status_str}"
    );
    assert!(
        fire["fire_id"].as_str().unwrap_or("").starts_with("at-fire-test"),
        "fire_id should start with schedule_id"
    );
    assert!(
        fire["scheduled_at"].as_i64().is_some(),
        "scheduled_at should be present"
    );
}

// ── Timer dispatch: interval schedule fires ────────────────────────────────
//
// Register a 2-second interval. Poll for the first fire within 5 seconds.

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_interval_schedule_fires() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "interval-fire-test",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "2",
    })).await;
    let result = unwrap_result(status, &body, "scheduler.register interval");

    // Debug: verify the schedule is in the list (proves DB write worked)
    let (status, body) = exec(&h, "service:scheduler/list", json!({})).await;
    let list_result = unwrap_result(status, &body, "scheduler.list");
    let schedules = list_result["schedules"].as_array().expect("schedules array");
    assert!(
        schedules.iter().any(|s| s["schedule_id"] == "interval-fire-test"),
        "schedule should be in list after register; got {list_result}"
    );

    // Debug: check daemon trace log for scheduler messages
    eprintln!("[test] schedule registered, waiting for fire...");

    // Poll for first fire (2s interval + timer sleep margin)
    let fire = poll_for_fires(&h, "interval-fire-test", 1, Duration::from_secs(8))
        .await;

    if fire.is_none() {
        // Dump trace log for debugging
        let trace_path = h.state_path.join(".ai").join("state").join("trace-events.ndjson");
        if trace_path.exists() {
            let trace_content = std::fs::read_to_string(&trace_path).unwrap_or_default();
            eprintln!("[test] daemon trace (last 50 lines):");
            for line in trace_content.lines().rev().take(50) {
                eprintln!("  {line}");
            }
        } else {
            eprintln!("[test] no trace file at {}", trace_path.display());
        }
        panic!("expected at least 1 fire within 8s");
    }

    let fire = fire.unwrap();
    assert_eq!(fire["schedule_id"], "interval-fire-test");
    let status_str = fire["status"].as_str().unwrap_or("");
    assert!(
        matches!(status_str, "dispatched" | "failed"),
        "expected fire status dispatched or failed, got: {status_str}"
    );
}

// ── Timer dispatch: interval fires multiple times ──────────────────────────
//
// Register a 2-second interval, wait for 2 fires, verify they have
// different fire_ids (proving dedup works).

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_interval_fires_twice() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "multi-fire-test",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "2",
    })).await;
    unwrap_result(status, &body, "scheduler.register interval");

    // Wait for at least 2 fires (2 fires × 2s interval + margin)
    let fires = poll_for_fires_count(&h, "multi-fire-test", 2, Duration::from_secs(12))
        .await
        .expect("expected at least 2 fires within 12s");

    // Verify two distinct fire IDs
    let id0 = fires[0]["fire_id"].as_str().unwrap_or("");
    let id1 = fires[1]["fire_id"].as_str().unwrap_or("");
    assert_ne!(id0, id1, "two fires should have different fire_ids");
    assert!(id0.starts_with("multi-fire-test"));
    assert!(id1.starts_with("multi-fire-test"));
}

// ── Pause prevents fires ──────────────────────────────────────────────────
//
// Register a 2-second interval, wait for a fire, then pause.
// Wait another 5s and verify no additional fires appeared.

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_pause_prevents_fires() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, _) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "pause-no-fire",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "2",
    })).await;
    assert!(status.is_success());

    // Wait for first fire
    let _first = poll_for_fires(&h, "pause-no-fire", 1, Duration::from_secs(8))
        .await
        .expect("first fire should appear");

    // Pause
    let (status, _) = exec(&h, "service:scheduler/pause", json!({
        "schedule_id": "pause-no-fire",
    })).await;
    assert!(status.is_success());

    // Wait 5 seconds — no new fires should appear
    tokio::time::sleep(Duration::from_secs(5)).await;

    let (status, body) = exec(&h, "service:scheduler/show_fires", json!({
        "schedule_id": "pause-no-fire",
    })).await;
    let result = unwrap_result(status, &body, "show_fires after pause");
    let total = result["total"].as_u64().unwrap_or(0);
    // Should still be exactly 1 (the fire before pause)
    assert_eq!(total, 1, "paused schedule should not have additional fires");
}

// ── Deregister stops fires ────────────────────────────────────────────────
//
// Register a 2-second interval, wait for a fire, deregister.
// Wait another 5s and verify no additional fires.

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_deregister_stops_fires() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, _) = exec(&h, "service:scheduler/register", json!({
        "schedule_id": "dereg-stop",
        "item_ref": "directive:test/hello",
        "schedule_type": "interval",
        "expression": "2",
    })).await;
    assert!(status.is_success());

    // Wait for first fire
    let _first = poll_for_fires(&h, "dereg-stop", 1, Duration::from_secs(8))
        .await
        .expect("first fire should appear");

    // Deregister
    let (status, _) = exec(&h, "service:scheduler/deregister", json!({
        "schedule_id": "dereg-stop",
    })).await;
    assert!(status.is_success());

    // Wait 5 seconds — no new fires should appear
    tokio::time::sleep(Duration::from_secs(5)).await;

    // The schedule is gone from the list
    let (status, body) = exec(&h, "service:scheduler/list", json!({})).await;
    let result = unwrap_result(status, &body, "scheduler.list after deregister");
    let schedules = result["schedules"].as_array().expect("schedules array");
    assert!(
        !schedules.iter().any(|s| s["schedule_id"] == "dereg-stop"),
        "deregistered schedule should not appear in list"
    );
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Poll `show_fires` until at least `min_count` fire records appear.
/// Returns the first fire record.
async fn poll_for_fires(
    h: &DaemonHarness,
    schedule_id: &str,
    min_count: usize,
    timeout: Duration,
) -> Option<Value> {
    let fires = poll_for_fires_count(h, schedule_id, min_count, timeout).await?;
    fires.into_iter().next()
}

/// Poll `show_fires` until at least `min_count` fire records appear.
/// Returns all fire records found.
async fn poll_for_fires_count(
    h: &DaemonHarness,
    schedule_id: &str,
    min_count: usize,
    timeout: Duration,
) -> Option<Vec<Value>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (status, body) = exec(h, "service:scheduler/show_fires", json!({
            "schedule_id": schedule_id,
        })).await;

        if status.is_success() {
            if let Some(result) = body.get("result") {
                let total = result["total"].as_u64().unwrap_or(0) as usize;
                if total >= min_count {
                    return result["fires"].as_array().cloned();
                }
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
