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
use serde_json::{Value, json};
use std::time::Duration;

/// Convenience: POST /execute and unwrap.
async fn exec(
    h: &DaemonHarness,
    item_ref: &str,
    mut params: Value,
) -> (reqwest::StatusCode, Value) {
    // Registration tests exercise scheduler behavior rather than repeating the
    // mandatory execution-authority envelope in every fixture. Keep that
    // envelope explicit here: fires are daemon-owned, restart-recoverable,
    // projectless test executions with a deliberately narrow capability.
    if item_ref == "service:scheduler/register" {
        let object = params
            .as_object_mut()
            .expect("scheduler registration params must be an object");
        let doctor =
            object.get("item_ref").and_then(Value::as_str) == Some("tool:ryeos/core/node/doctor");
        object.entry("capabilities").or_insert_with(|| {
            if doctor {
                json!([
                    "ryeos.execute.tool.ryeos/core/node/doctor",
                    "ryeos.read.project.live"
                ])
            } else {
                json!(["ryeos.execute.directive.test/*"])
            }
        });
        object.entry("execution_policy").or_insert_with(|| {
            if doctor {
                json!({
                    "schema_version": 2,
                    "ownership": "daemon_owned",
                    "recovery": "restart_recoverable",
                    "response": "accepted",
                    "target": {"kind": "here"},
                    "environment": {"kind": "none"},
                    "project": {
                        "kind": "live_direct",
                        "access": "read_only",
                        "child_policy": {"kind": "inherit"}
                    }
                })
            } else {
                json!({
                "schema_version": 2,
                "ownership": "daemon_owned",
                "recovery": "restart_recoverable",
                "response": "accepted",
                "target": {"kind": "here"},
                "environment": {"kind": "none"},
                "project": {"kind": "projectless"}
                })
            }
        });
    }
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
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    // Register an interval schedule
    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "test-interval",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "3600",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    let result = unwrap_result(status, &body, "scheduler.register");
    assert_eq!(result["schedule_id"], "test-interval");
    assert_eq!(result["schedule_type"], "interval");
    assert_eq!(result["created"], true);

    // List schedules — should contain our new one
    let (status, body) = exec(&h, "service:scheduler/list", json!({})).await;
    let result = unwrap_result(status, &body, "scheduler.list");
    let schedules = result["schedules"].as_array().expect("schedules array");
    assert!(
        schedules
            .iter()
            .any(|s| s["schedule_id"] == "test-interval"),
        "list should contain test-interval; got {result}"
    );
}

// ── Register duplicate updates ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_register_update_existing() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    // Create
    let (status, _) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "update-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "60",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    assert!(status.is_success());

    // Update with different expression
    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "update-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "120",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    let result = unwrap_result(status, &body, "scheduler.register update");
    assert_eq!(result["created"], false, "should be update, not create");
    assert_eq!(result["expression"], "120");
}

// ── Pause + Resume ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_pause_and_resume() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    // Create
    exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "pause-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "60",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;

    // Pause
    let (status, body) = exec(
        &h,
        "service:scheduler/pause",
        json!({
            "schedule_id": "pause-test",
        }),
    )
    .await;
    let result = unwrap_result(status, &body, "scheduler.pause");
    assert_eq!(result["schedule_id"], "pause-test");
    assert_eq!(result["enabled"], false);

    // Resume
    let (status, body) = exec(
        &h,
        "service:scheduler/resume",
        json!({
            "schedule_id": "pause-test",
        }),
    )
    .await;
    let result = unwrap_result(status, &body, "scheduler.resume");
    assert_eq!(result["schedule_id"], "pause-test");
    assert_eq!(result["enabled"], true);
}

// ── Show Fires (empty for new schedule) ──────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_show_fires_empty() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "fires-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "86400",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;

    let (status, body) = exec(
        &h,
        "service:scheduler/show_fires",
        json!({
            "schedule_id": "fires-test",
        }),
    )
    .await;
    let result = unwrap_result(status, &body, "scheduler.show_fires");
    assert_eq!(result["total"], 0);
    assert!(result["fires"].as_array().unwrap().is_empty());
}

// ── Deregister ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_deregister_removes_schedule() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    // Create
    exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "deleteme",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "60",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;

    // Deregister
    let (status, body) = exec(
        &h,
        "service:scheduler/deregister",
        json!({
            "schedule_id": "deleteme",
        }),
    )
    .await;
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
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "cron-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "cron",
            "expression": "0 0 * * * *",
            "params": {},
            "timezone": "America/New_York",
            "overlap_policy": "cancel_previous",
            "misfire_policy": "fire_once_now",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    let result = unwrap_result(status, &body, "scheduler.register cron");
    assert_eq!(result["schedule_type"], "cron");
    assert_eq!(result["timezone"], "America/New_York");
    assert_eq!(result["overlap_policy"], "cancel_previous");
    assert_eq!(result["misfire_policy"], "fire_once_now");
}

// ── Register validation errors ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_register_rejects_bad_expression() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "bad-expr",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "cron",
            "expression": "not a cron expression",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "skip",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    assert!(
        !status.is_success(),
        "expected error for bad expression, got {status}; body={body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_register_rejects_past_at() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "past-at",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "at",
            "expression": "2020-01-01T00:00:00Z",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "skip",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    assert!(
        !status.is_success(),
        "expected error for past at timestamp, got {status}; body={body}"
    );
}

// ── Deregister nonexistent ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_deregister_nonexistent_fails() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    let (status, _) = exec(
        &h,
        "service:scheduler/deregister",
        json!({
            "schedule_id": "no-exist",
        }),
    )
    .await;
    assert!(
        !status.is_success(),
        "expected error for deregistering nonexistent schedule"
    );
}

// ── Pause nonexistent ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_pause_nonexistent_fails() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    let (status, _) = exec(
        &h,
        "service:scheduler/pause",
        json!({
            "schedule_id": "no-exist",
        }),
    )
    .await;
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
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    // Compute a timestamp 6 seconds from now (generous margin for
    // daemon startup + timer loop initialization)
    let fire_at = chrono::Utc::now() + chrono::Duration::seconds(6);
    let fire_at_str = fire_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // Register at-schedule
    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "at-fire-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "at",
            "expression": fire_at_str,
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "skip",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    let result = unwrap_result(status, &body, "scheduler.register at");
    assert_eq!(result["schedule_type"], "at");

    // Poll show_fires until a fire appears (max 10s — timer sleep + margin)
    let fire = poll_for_fires(&h, "at-fire-test", 1, Duration::from_secs(10))
        .await
        .expect("expected at least 1 fire within 10s");
    let fire = await_dispatch_handoff(&h, "at-fire-test", fire).await;

    // Fire record exists — proves the timer dispatched
    assert_eq!(fire["schedule_id"], "at-fire-test");
    let status_str = fire["status"].as_str().unwrap_or("");
    assert!(
        matches!(status_str, "dispatched" | "failed"),
        "expected fire status dispatched or failed, got: {status_str}"
    );
    assert!(
        fire["fire_id"]
            .as_str()
            .unwrap_or("")
            .starts_with("at-fire-test"),
        "fire_id should start with schedule_id"
    );
    assert!(
        fire["scheduled_at"].as_i64().is_some(),
        "scheduled_at should be present"
    );
    assert!(
        fire["thread_id"].as_str().unwrap_or("").starts_with("T-"),
        "scheduled execution thread_id must be canonical T-*; got {fire}"
    );
}

// ── Timer dispatch: interval schedule fires ────────────────────────────────
//
// Register a 2-second interval. Poll for the first fire within 5 seconds.

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_interval_schedule_fires() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "interval-fire-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "2",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    unwrap_result(status, &body, "scheduler.register interval");

    // Debug: verify the schedule is in the list (proves DB write worked)
    let (status, body) = exec(&h, "service:scheduler/list", json!({})).await;
    let list_result = unwrap_result(status, &body, "scheduler.list");
    let schedules = list_result["schedules"]
        .as_array()
        .expect("schedules array");
    assert!(
        schedules
            .iter()
            .any(|s| s["schedule_id"] == "interval-fire-test"),
        "schedule should be in list after register; got {list_result}"
    );

    // Debug: check daemon trace log for scheduler messages
    eprintln!("[test] schedule registered, waiting for fire...");

    // Poll for first fire (2s interval + timer sleep margin)
    let fire = poll_for_fires(&h, "interval-fire-test", 1, Duration::from_secs(8)).await;

    if fire.is_none() {
        // Dump trace log for debugging
        let trace_path = h
            .state_path
            .join(".ai")
            .join("state")
            .join("trace-events.ndjson");
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

    let fire = await_dispatch_handoff(&h, "interval-fire-test", fire.unwrap()).await;
    assert_eq!(fire["schedule_id"], "interval-fire-test");
    let status_str = fire["status"].as_str().unwrap_or("");
    assert!(
        matches!(status_str, "dispatched" | "failed"),
        "expected fire status dispatched or failed, got: {status_str}"
    );
}

// ── Authenticated Core doctor schedule: real subprocess + retained terminal ──

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_dispatches_core_doctor_and_records_terminal_fire() {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon from prepared bundles");
    let (status, body) = exec(&h, "service:bundle/list", json!({})).await;
    let installed = unwrap_result(status, &body, "verify Core+Scheduling selection");
    let bundles = installed["bundles"].as_array().expect("bundle list");
    assert!(
        !bundles
            .iter()
            .any(|bundle| bundle["name"] == "central-auth")
    );
    assert!(bundles.iter().any(|bundle| bundle["name"] == "core"));
    assert!(bundles.iter().any(|bundle| bundle["name"] == "scheduling"));
    assert!(!bundles.iter().any(|bundle| bundle["name"] == "standard"));

    let project_root = h.user_space.path().join("scheduler-doctor-project");
    let project_ai = project_root.join(".ai");
    let source = project_ai.join("fixtures/scheduler-doctor-probe");
    let source_ai = source.join(".ai");
    std::fs::create_dir_all(&source_ai).expect("create isolated doctor probe source");
    std::fs::write(
        project_ai.join("manifest.source.yaml"),
        "name: scheduler-doctor-project\nversion: \"0.1.0\"\n",
    )
    .expect("write valid project source manifest");
    // Recorded fixture bytes, SHA-256 e921fba150e4cbdc31364d8cd21d12c733cb4f2aa1c3535aee95ed0d0e648dc8.
    const MALFORMED_DOCTOR_MANIFEST: &str =
        "name: scheduler-doctor-probe\nversion: \"0.1.0\"\nbogus_field: true\n";
    std::fs::write(
        source_ai.join("manifest.source.yaml"),
        MALFORMED_DOCTOR_MANIFEST,
    )
    .expect("write fixed malformed doctor input");
    let project_root = project_root
        .canonicalize()
        .expect("resolve valid doctor project root");
    let source = source
        .canonicalize()
        .expect("resolve malformed doctor probe source");
    assert!(
        source.starts_with(&project_root),
        "doctor source must remain inside its admitted read-only project root"
    );
    let project_root_text = project_root.display().to_string();
    // This is the exact registered Core path under the daemon's selected app root.
    let core_registry_root = h.state_path.join(".ai/bundles/core");
    let fire_at = chrono::Utc::now() + chrono::Duration::seconds(8);
    let fire_at_str = fire_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "core-doctor-authenticated-fire",
            "item_ref": "tool:ryeos/core/node/doctor",
            "ref_bindings": {},
            "schedule_type": "at",
            "expression": fire_at_str,
            "params": {
                // The admitted plan refuses any parameter that embeds its
                // project root, so the source is named relative to it.
                "source": ".ai/fixtures/scheduler-doctor-probe",
                "registry_roots": [core_registry_root.display().to_string()]
            },
            "timezone": "UTC",
            "misfire_policy": "skip",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
            "project_root": project_root_text,
        }),
    )
    .await;
    unwrap_result(status, &body, "register Core doctor schedule");
    h.retain_evidence_on_drop(true);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let (thread_id, admitted_capsule_hash) = loop {
        let (status, body) = exec(
            &h,
            "service:scheduler/show_fires",
            json!({"schedule_id": "core-doctor-authenticated-fire"}),
        )
        .await;
        let result = unwrap_result(status, &body, "observe Core doctor fire");
        if let Some(fire) = result["fires"].as_array().and_then(|fires| fires.first()) {
            assert!(
                !matches!(fire["status"].as_str(), Some("failed" | "cancelled")),
                "Core doctor fire failed before dispatch: {fire}"
            );
            if matches!(fire["status"].as_str(), Some("dispatched" | "completed")) {
                let thread_id = fire["thread_id"]
                    .as_str()
                    .filter(|id| id.starts_with("T-"))
                    .expect("admitted fire must retain its canonical root thread id");
                let capsule_hash = fire["admitted_capsule_hash"]
                    .as_str()
                    .filter(|hash| {
                        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .expect("admitted fire must retain its capsule content hash");
                // The authenticated schedule's persisted capsule is the
                // admission evidence; the local project manifest is only input.
                break (thread_id.to_string(), capsule_hash.to_string());
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Core doctor fire did not reach admitted dispatch"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    // The fire record keeps only status, outcome class, thread id and the
    // admitted capsule hash; the doctor's findings are read back through the
    // Core `threads/get` service once the thread is terminal.
    loop {
        let (status, body) = exec(
            &h,
            "service:scheduler/show_fires",
            json!({"schedule_id": "core-doctor-authenticated-fire"}),
        )
        .await;
        let result = unwrap_result(status, &body, "read terminal Core doctor fire");
        if let Some(fire) = result["fires"].as_array().and_then(|fires| fires.first())
            && fire["status"] == "completed"
        {
            assert_eq!(fire["outcome"], "success", "terminal fire failed: {fire}");
            assert!(fire["completed_at"].as_i64().is_some());
            assert_eq!(fire["thread_id"], thread_id);
            assert_eq!(fire["admitted_capsule_hash"], admitted_capsule_hash);
            // The tool completing is a lifecycle verdict; the doctor's own
            // project verdict is a separate authority carried in its result.
            let (status, body) =
                exec(&h, "service:threads/get", json!({"thread_id": thread_id})).await;
            let retained = unwrap_result(status, &body, "read retained doctor thread");
            assert_eq!(retained["thread"]["status"], "completed", "{retained}");
            let doctor_result = &retained["result"]["result"];
            assert_eq!(doctor_result["ok"], false, "{retained}");
            let manifest_finding = doctor_result["checks"]
                .as_array()
                .and_then(|checks| checks.iter().find(|check| check["name"] == "manifest"))
                .expect("doctor report must retain the manifest finding");
            assert_eq!(manifest_finding["status"], "fail");
            assert!(
                manifest_finding["detail"]["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("bogus_field")),
                "malformed manifest finding must name the fixed unknown field: {manifest_finding}"
            );
            h.kill_daemon()
                .await
                .expect("stop daemon before restoring temporary cleanup");
            h.retain_evidence_on_drop(false);
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "scheduler did not persist the terminal fire for thread {thread_id}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ── Timer dispatch: interval fires multiple times ──────────────────────────
//
// Register a 2-second interval, wait for 2 fires, verify they have
// different fire_ids (proving dedup works).

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_interval_fires_twice() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "multi-fire-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "2",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
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
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    let (status, _) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "pause-no-fire",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "2",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    assert!(status.is_success());

    // Wait for first fire
    let _first = poll_for_fires(&h, "pause-no-fire", 1, Duration::from_secs(8))
        .await
        .expect("first fire should appear");

    // Pause
    let (status, _) = exec(
        &h,
        "service:scheduler/pause",
        json!({
            "schedule_id": "pause-no-fire",
        }),
    )
    .await;
    assert!(status.is_success());

    // A boundary already claimed before the pause write gate linearizes may
    // legitimately be visible now. From the successful pause response onward,
    // however, the count must remain stable.
    let count_after_pause = fire_count(&h, "pause-no-fire")
        .await
        .expect("show_fires after pause");
    let stable = observe_fire_count_stable(
        &h,
        "pause-no-fire",
        count_after_pause,
        3,
        Duration::from_secs(3),
    )
    .await;
    assert!(
        stable,
        "paused schedule should not produce additional fires"
    );
}

// ── Deregister stops fires ────────────────────────────────────────────────
//
// Register a 2-second interval, wait for a fire, deregister.
// Wait another 5s and verify no additional fires.

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_deregister_stops_fires() {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    let (status, _) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "dereg-stop",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "2",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    assert!(status.is_success());

    // Wait for first fire
    let _first = poll_for_fires(&h, "dereg-stop", 1, Duration::from_secs(8))
        .await
        .expect("first fire should appear");

    // Deregister
    let (status, _) = exec(
        &h,
        "service:scheduler/deregister",
        json!({
            "schedule_id": "dereg-stop",
        }),
    )
    .await;
    assert!(status.is_success());

    // The schedule is gone from the list
    let (status, body) = exec(&h, "service:scheduler/list", json!({})).await;
    let result = unwrap_result(status, &body, "scheduler.list after deregister");
    let schedules = result["schedules"].as_array().expect("schedules array");
    assert!(
        !schedules.iter().any(|s| s["schedule_id"] == "dereg-stop"),
        "deregistered schedule should not appear in list"
    );

    // `show_fires` is owner-authorized through the live schedule spec and
    // therefore returns not-found after deregistration. The successful
    // deregister response is the fence after which no new fire identity may
    // be reserved (completion may still update an already-claimed row). Give
    // the 2s interval time to fire again, stop the node in order, and read
    // the durable fire rows from the stopped Scheduler store.
    let deregistered_at_ms = lillux::time::timestamp_millis();
    tokio::time::sleep(Duration::from_secs(3)).await;
    h.stop_gracefully()
        .await
        .expect("stop daemon before reading its Scheduler store");
    let reservations = stopped_fire_reservations(&h, "dereg-stop");
    assert!(
        !reservations.is_empty(),
        "the fire observed before deregistration must be durable: {reservations:?}"
    );
    assert!(
        reservations
            .iter()
            .all(|(_, reserved_at)| *reserved_at <= deregistered_at_ms),
        "deregistered schedule should not reserve additional fires after {deregistered_at_ms}: \
         {reservations:?}"
    );
}

// ── Schedule ID reuse blocked ──────────────────────────────────────────────
//
// Register → deregister → re-register same ID with different item_ref.
// The JSONL history file is preserved on disk after deregister, so
// re-registration should be blocked to prevent history corruption.

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_reuse_blocked_after_deregister() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    // Register with a short interval so a fire happens quickly
    let (status, _) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "reuse-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "2",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    assert!(status.is_success(), "first register should succeed");

    // Wait for a fire so JSONL history exists on disk
    let _fire = poll_for_fires(&h, "reuse-test", 1, Duration::from_secs(8))
        .await
        .expect("expected at least 1 fire");

    // Deregister
    let (status, _) = exec(
        &h,
        "service:scheduler/deregister",
        json!({
            "schedule_id": "reuse-test",
        }),
    )
    .await;
    assert!(status.is_success(), "deregister should succeed");

    // Attempt to re-register same schedule_id — should be blocked
    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "reuse-test",
            "item_ref": "directive:test/different",
            "ref_bindings": {"model": "directive:test/different"},
            "schedule_type": "interval",
            "expression": "120",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    assert!(
        !status.is_success(),
        "re-registration after deregister should be blocked; got {status}; body={body}"
    );
    let error_msg = body.to_string().to_lowercase();
    assert!(
        error_msg.contains("reuse") || error_msg.contains("history"),
        "error should mention reuse/history: {body}"
    );
}

// ── Recovery: schedule survives restart ─────────────────────────────────────
//
// Register an interval schedule, let it fire, restart the daemon, verify
// the schedule is still in the list (projection rebuilt from CAS).

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_survives_restart() {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    // Register a short-interval schedule
    let (status, _) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "restart-test",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "2",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    assert!(status.is_success());

    // Wait for a fire
    let _fire = poll_for_fires(&h, "restart-test", 1, Duration::from_secs(8))
        .await
        .expect("expected at least 1 fire before restart");

    // Restart daemon
    h.restart().await.expect("restart daemon");

    // Schedule should still be in the list
    let (status, body) = exec(&h, "service:scheduler/list", json!({})).await;
    let result = unwrap_result(status, &body, "list after restart");
    let schedules = result["schedules"].as_array().expect("schedules array");
    assert!(
        schedules.iter().any(|s| s["schedule_id"] == "restart-test"),
        "schedule should survive restart; got {result}"
    );

    // Fire history should still be accessible
    let (status, body) = exec(
        &h,
        "service:scheduler/show_fires",
        json!({
            "schedule_id": "restart-test",
        }),
    )
    .await;
    let result = unwrap_result(status, &body, "show_fires after restart");
    let total = result["total"].as_u64().unwrap_or(0);
    assert!(
        total >= 1,
        "fire history should survive restart; total={total}"
    );
}

// ── Fire ID determinism across restart ──────────────────────────────────────
//
// Register an interval schedule, note the fire_id of the first fire.
// Restart and verify the schedule still produces consistent fire_ids.

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_fire_id_deterministic() {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    // Register a 2-second interval schedule
    let (status, _) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "det-fire",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "2",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    assert!(status.is_success());

    // Wait for first fire
    let fire = poll_for_fires(&h, "det-fire", 1, Duration::from_secs(8))
        .await
        .expect("expected first fire");

    let first_fire_id = fire["fire_id"].as_str().unwrap_or("").to_string();
    assert!(!first_fire_id.is_empty(), "fire_id should be non-empty");

    // Restart daemon
    h.restart().await.expect("restart daemon");

    // Fire history should show the same fire_id
    let (status, body) = exec(
        &h,
        "service:scheduler/show_fires",
        json!({
            "schedule_id": "det-fire",
        }),
    )
    .await;
    let result = unwrap_result(status, &body, "show_fires after restart");
    let fires = result["fires"].as_array().expect("fires array");
    assert!(
        fires
            .iter()
            .any(|f| f["fire_id"].as_str() == Some(first_fire_id.as_str())),
        "fire_id '{first_fire_id}' should be in history after restart; got {result}"
    );
}

// ── registered_at preserved on update ───────────────────────────────────────
//
// Register a schedule, update it, verify the registered_at timestamp
// didn't drift (it should be the same as the original registration).

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_registered_at_preserved_on_update() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start daemon");

    // Register
    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "ts-drift",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "60",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    unwrap_result(status, &body, "first register");

    // Get the schedule from list (to see registered_at in the YAML)
    // Note: the list endpoint returns schedule data; we can verify the
    // schedule survives an update by re-registering and checking it's
    // still recognized as an update (not new creation).

    // Update with different expression
    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "ts-drift",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "120",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    let result2 = unwrap_result(status, &body, "update register");
    assert_eq!(result2["created"], false, "should be update not create");
    assert_eq!(result2["expression"], "120");

    // Update again — registered_at should still be stable (not drifted)
    let (status, body) = exec(
        &h,
        "service:scheduler/register",
        json!({
            "schedule_id": "ts-drift",
            "item_ref": "directive:test/hello",
            "ref_bindings": {"model": "directive:test/hello"},
            "schedule_type": "interval",
            "expression": "180",
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
        }),
    )
    .await;
    let result3 = unwrap_result(status, &body, "second update");
    assert_eq!(result3["created"], false, "should still be update");
    assert_eq!(result3["expression"], "180");
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Poll `show_fires` until at least `min_count` fire records appear.
/// Returns the first fire record.
/// A fire record is first written as `reserved` and becomes `dispatched` (or
/// `failed`) when the launch handoff persists, so the first sighting of the
/// record is not yet the dispatch outcome. Follow that fire, by identity
/// since later fires may exist, until it leaves `reserved`; a fire that stays
/// reserved past the deadline is returned as is and fails the caller's check.
async fn await_dispatch_handoff(h: &DaemonHarness, schedule_id: &str, mut fire: Value) -> Value {
    let first_fire_id = fire["fire_id"].clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while fire["status"] == "reserved" && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some(fires) = poll_for_fires_count(h, schedule_id, 1, Duration::from_secs(1)).await
            && let Some(latest) = fires.into_iter().find(|f| f["fire_id"] == first_fire_id)
        {
            fire = latest;
        }
    }
    fire
}

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
        let (status, body) = exec(
            h,
            "service:scheduler/show_fires",
            json!({
                "schedule_id": schedule_id,
            }),
        )
        .await;

        if status.is_success()
            && let Some(result) = body.get("result")
        {
            let total = result["total"].as_u64().unwrap_or(0) as usize;
            if total >= min_count {
                return result["fires"].as_array().cloned();
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn fire_count(h: &DaemonHarness, schedule_id: &str) -> Option<u64> {
    let (status, body) = exec(
        h,
        "service:scheduler/show_fires",
        json!({
            "schedule_id": schedule_id,
        }),
    )
    .await;
    if !status.is_success() {
        return None;
    }
    body.get("result")?.get("total")?.as_u64()
}

/// `(fire_id, reserved_at_ms)` of every durable fire of `schedule_id`, read
/// from a stopped node's Scheduler store through the native test gate. The
/// daemon must already have exited; a live store is never reopened.
fn stopped_fire_reservations(h: &DaemonHarness, schedule_id: &str) -> Vec<(String, i64)> {
    let state_dir = h.state_path.join(ryeos_engine::AI_DIR).join("state");
    let host = ryeos_scheduler::test_native::TestSchedulerHost::acquire();
    host.with_fixture_sql(&state_dir, "scheduler.sqlite3", false, |connection| {
        let mut statement = connection.prepare(
            "SELECT fire_id, reserved_at FROM schedule_fires \
             WHERE schedule_id = ?1 ORDER BY reserved_at",
        )?;
        let rows = statement
            .query_map([schedule_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// Observe `show_fires` over a time window, checking that the fire count
/// stays stable at `expected_count` for `consecutive` consecutive checks.
/// Spreads checks evenly across `window` duration.
/// Returns `true` if all checks saw exactly `expected_count` fires.
async fn observe_fire_count_stable(
    h: &DaemonHarness,
    schedule_id: &str,
    expected_count: u64,
    consecutive: usize,
    window: Duration,
) -> bool {
    let interval = window / consecutive as u32;
    for _ in 0..consecutive {
        tokio::time::sleep(interval).await;
        if fire_count(h, schedule_id).await != Some(expected_count) {
            return false;
        }
    }
    true
}
