//! Schedule authority and bounds, end to end through the daemon.
//!
//! A schedule's signed lifetime bounds pause it by itself once used up, with
//! the typed reason recorded beside `enabled: false`.

#![cfg(all(unix, feature = "scheduling"))]

use ryeosd::test_support as common;

use std::time::Duration;

use common::DaemonHarness;
use serde_json::{Value, json};

/// POST a service through `/execute` as the harness operator; return
/// `(status, body)`.
async fn exec(h: &DaemonHarness, item_ref: &str, params: Value) -> (reqwest::StatusCode, Value) {
    h.post_execute(item_ref, ".", params)
        .await
        .unwrap_or_else(|error| panic!("post {item_ref}: {error:#}"))
}

fn result(status: reqwest::StatusCode, body: &Value, what: &str) -> Value {
    assert!(
        status.is_success(),
        "{what}: expected success, got {status}; body={body:#}"
    );
    body.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{what}: response has no result; body={body:#}"))
}

/// A projectless interval registration. Fires need not succeed: the lifetime
/// counts boundaries, not successful runs.
fn interval_registration(schedule_id: &str, interval_secs: u64) -> Value {
    json!({
        "schedule_id": schedule_id,
        "item_ref": "directive:test/hello",
        "ref_bindings": {"model": "directive:test/hello"},
        "schedule_type": "interval",
        "expression": interval_secs.to_string(),
        "params": {},
        "timezone": "UTC",
        "misfire_policy": "skip",
        "overlap_policy": "allow",
        "lateness_grace_secs": 60,
        "enabled": true,
        "capabilities": ["ryeos.execute.directive.test/*"],
        "execution_policy": {
            "schema_version": 2,
            "ownership": "daemon_owned",
            "recovery": "restart_recoverable",
            "response": "accepted",
            "target": {"kind": "here"},
            "environment": {"kind": "none"},
            "project": {"kind": "projectless"}
        },
    })
}

/// Poll `scheduler.explain` until `done` accepts it.
async fn explain_until(
    h: &DaemonHarness,
    schedule_id: &str,
    timeout: Duration,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (status, body) = exec(
            h,
            "service:scheduler/explain",
            json!({"schedule_id": schedule_id}),
        )
        .await;
        let explained = result(status, &body, "scheduler.explain");
        if done(&explained) {
            return explained;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "schedule {schedule_id} did not reach the expected state in time: {explained:#}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn max_fires_pauses_the_schedule_with_a_typed_reason() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");

    let mut registration = interval_registration("bounded-by-count", 1);
    registration["max_fires"] = json!(2);
    let (status, body) = exec(&h, "service:scheduler/register", registration).await;
    let registered = result(status, &body, "register bounded schedule");
    assert_eq!(registered["max_fires"], 2, "{registered:#}");

    let explained = explain_until(&h, "bounded-by-count", Duration::from_secs(30), |e| {
        e["enabled"] == false
    })
    .await;
    assert_eq!(
        explained["paused_reason"], "max_fires_reached",
        "{explained:#}"
    );
    assert_eq!(explained["lifetime"]["max_fires"], 2, "{explained:#}");
    assert!(explained["next_fire_at"].is_null(), "{explained:#}");

    let (status, body) = exec(
        &h,
        "service:scheduler/show_fires",
        json!({"schedule_id": "bounded-by-count"}),
    )
    .await;
    let fires = result(status, &body, "show_fires");
    let fires = fires["fires"].as_array().expect("fire list");
    assert_eq!(
        fires.len(),
        2,
        "exactly the two admitted boundaries are recorded: {fires:#?}"
    );

    // Resuming a used-up lifetime is refused with the typed reason.
    let (status, body) = exec(
        &h,
        "service:scheduler/resume",
        json!({"schedule_id": "bounded-by-count"}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{body:#}");
    assert!(
        body.to_string().contains("schedule_lifetime_exhausted"),
        "resume must name the typed refusal: {body:#}"
    );
}
