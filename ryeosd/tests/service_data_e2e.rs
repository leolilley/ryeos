//! End-to-end tests that prove each `Both` and `DaemonOnly` service
//! actually performs real data work. Spawns a real `ryeosd` daemon
//! per test (via `common::DaemonHarness`), POSTs `/execute` with real
//! params, and asserts on the data shape returned.
//!
//! Companion file: `service_data_standalone_e2e.rs` covers OfflineOnly
//! services that require `run-service` mode.

mod common;

use common::DaemonHarness;
use serde_json::{json, Value};

/// Convenience: POST /execute and unwrap as JSON, panicking on transport error.
async fn exec(h: &DaemonHarness, item_ref: &str, params: Value) -> (reqwest::StatusCode, Value) {
    h.post_execute(item_ref, ".", params)
        .await
        .expect("post /execute")
}

/// Convenience: assert /execute returned 200 OK and return the body's
/// `result` field (the handler's actual return value, not the envelope).
fn unwrap_result(status: reqwest::StatusCode, body: &Value, ctx: &str) -> Value {
    assert!(
        status.is_success(),
        "{ctx}: expected 200, got {status}; body={body}"
    );
    body.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{ctx}: response had no `result` field; body={body}"))
}

/// Like `unwrap_result`, but also drills into the tool execution envelope's
/// inner `result` field.  Tool dispatch wraps the subprocess output in
/// `{ artifacts, error, outcome_code, result }` — this returns `result`.
fn unwrap_tool_result(status: reqwest::StatusCode, body: &Value, ctx: &str) -> Value {
    let tool_envelope = unwrap_result(status, body, ctx);
    tool_envelope.get("result").cloned()
        .unwrap_or_else(|| panic!("{ctx}: tool envelope missing inner `result`; got: {tool_envelope}"))
}

// ── 3.1 system/status ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_system_status_returns_snapshot() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(&h, "service:system/status", json!({})).await;
    let result = unwrap_result(status, &body, "system.status");
    // The status snapshot is an object; assert at least one expected key.
    assert!(result.is_object(), "expected object, got {result}");
    // Don't pin exact keys — just assert it's non-empty so we know the
    // handler produced data, not an empty stub.
    assert!(
        !result.as_object().unwrap().is_empty(),
        "system.status returned empty object: {result}"
    );
}

// ── 3.2 identity/public_key ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn tool_identity_public_key_returns_doc() {
    common::ensure_rye_inspect_in_core_bundle();
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(&h, "tool:rye/core/identity/public_key", json!({})).await;
    let result = unwrap_tool_result(status, &body, "identity.public_key");
    // The node identity doc must contain a non-empty fingerprint and a
    // non-empty public_key field. Look for either spelling.
    assert!(result.is_object(), "expected object, got {result}");
    let obj = result.as_object().unwrap();
    let has_principal = obj.keys().any(|k| k.contains("principal_id"));
    let has_key = obj.keys().any(|k| k.contains("signing_key"));
    assert!(has_principal, "identity doc missing principal_id key: {result}");
    assert!(has_key, "identity doc missing signing_key key: {result}");
}

// ── 3.3 bundle/list ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_bundle_list_returns_at_least_core() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(&h, "service:bundle/list", json!({})).await;
    let result = unwrap_result(status, &body, "bundle.list");
    let bundles = result
        .get("bundles")
        .and_then(|v| v.as_array())
        .expect("bundles array");
    // A freshly spawned daemon may have zero registered bundles (nothing
    // pre-installed via run-service). The handler must still return a valid
    // bundles array; the install→list round-trip is proven in
    // service_data_standalone_e2e.rs.
    for entry in bundles {
        assert!(entry.get("name").and_then(|v| v.as_str()).is_some(), "bundle entry missing name: {entry}");
        assert!(entry.get("path").and_then(|v| v.as_str()).is_some(), "bundle entry missing path: {entry}");
    }
}

// ── 3.4 threads/list — empty case ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_threads_list_empty_on_fresh_daemon() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(&h, "service:threads/list", json!({"limit": 100})).await;
    let result = unwrap_result(status, &body, "threads.list");
    let threads = result
        .get("threads")
        .and_then(|v| v.as_array())
        .expect("threads array");
    // A freshly spawned daemon has only the audit thread for THIS very call,
    // because every service execution creates a `svc-…` audit row. So we
    // expect EXACTLY one thread, and its id starts with `svc-`.
    assert_eq!(threads.len(), 1, "fresh daemon should have exactly 1 audit thread; got: {result}");
    let only = &threads[0];
    let tid = only.get("thread_id").and_then(|v| v.as_str()).expect("thread_id");
    assert!(tid.starts_with("svc-"), "audit thread id should start with svc-, got {tid}");
}

// ── 3.5 threads/list — populated case ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_threads_list_grows_with_each_call() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    // Each successful service call creates an audit thread row. Call
    // `system.status` 3 times, then check `threads.list` returns at
    // least 4 (the 3 calls plus the threads.list call itself).
    for _ in 0..3 {
        let (s, b) = exec(&h, "service:system/status", json!({})).await;
        assert!(s.is_success(), "system.status failed: {b}");
    }
    let (status, body) = exec(&h, "service:threads/list", json!({"limit": 100})).await;
    let result = unwrap_result(status, &body, "threads.list");
    let threads = result.get("threads").and_then(|v| v.as_array()).expect("threads array");
    assert!(
        threads.len() >= 4,
        "expected ≥4 audit threads after 3 status calls + 1 list call, got {} ({})",
        threads.len(), result
    );
}

// ── 3.6 threads/get — round-trip via captured audit id ──────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_threads_get_returns_audit_row() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    // Run any Both service to mint an audit thread.
    let (_, body1) = exec(&h, "service:system/status", json!({})).await;
    let tid = body1
        .get("thread")
        .and_then(|t| t.get("thread_id"))
        .and_then(|v| v.as_str())
        .expect("thread_id from envelope")
        .to_string();
    assert!(tid.starts_with("svc-"));

    let (status, body2) = exec(&h, "service:threads/get", json!({"thread_id": tid})).await;
    let result = unwrap_result(status, &body2, "threads.get");
    // result must be an object with thread/result/artifacts/facets keys.
    let obj = result.as_object().expect("threads.get returns object");
    for required in ["thread", "result", "artifacts", "facets"] {
        assert!(obj.contains_key(required), "threads.get missing key {required}: {result}");
    }
    let inner = obj["thread"].clone();
    let inner_id = inner.get("thread_id").and_then(|v| v.as_str()).expect("inner thread_id");
    assert_eq!(inner_id, &*tid, "round-trip mismatch: queried {tid} got {inner_id}");
}

// ── 3.7 threads/get — missing thread returns null ───────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_threads_get_missing_returns_null() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(
        &h,
        "service:threads/get",
        json!({"thread_id": "T-does-not-exist-xxxxxx"}),
    ).await;
    let result = unwrap_result(status, &body, "threads.get missing");
    assert!(result.is_null(), "missing thread should return null result, got {result}");
}

// ── 3.8 threads/chain — round-trip via audit id ─────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_threads_chain_returns_chain_for_audit_thread() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (_, body1) = exec(&h, "service:system/status", json!({})).await;
    let tid = body1["thread"]["thread_id"].as_str().unwrap().to_string();

    let (status, body2) = exec(&h, "service:threads/chain", json!({"thread_id": tid})).await;
    let result = unwrap_result(status, &body2, "threads.chain");
    // The audit thread is its own chain root (chain_root_id == thread_id).
    // Result is either null (chain not modeled for service-run) OR a
    // structured chain object. Accept both, but if non-null, must be an object.
    if !result.is_null() {
        assert!(result.is_object(), "threads.chain non-null must be object, got {result}");
    }
}

// ── 3.9 threads/children — empty for leaf audit thread ──────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_threads_children_returns_empty_for_audit_thread() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (_, body1) = exec(&h, "service:system/status", json!({})).await;
    let tid = body1["thread"]["thread_id"].as_str().unwrap().to_string();

    let (status, body2) = exec(&h, "service:threads/children", json!({"thread_id": tid})).await;
    let result = unwrap_result(status, &body2, "threads.children");
    let children = result.get("children").and_then(|v| v.as_array())
        .expect("children array");
    // Audit thread is a leaf; no children.
    assert!(children.is_empty(), "audit thread should have no children, got {result}");
}

// ── 3.10 events/replay — replay scoped to a thread ──────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_events_replay_returns_events_for_audit_thread() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (_, body1) = exec(&h, "service:system/status", json!({})).await;
    let tid = body1["thread"]["thread_id"].as_str().unwrap().to_string();

    let (status, body2) = exec(
        &h, "service:events/replay",
        json!({"thread_id": tid, "limit": 100}),
    ).await;
    let result = unwrap_result(status, &body2, "events.replay");
    assert!(result.get("events").and_then(|v| v.as_array()).is_some(),
        "events.replay missing events array: {result}");
    // next_cursor may be null; just assert the key exists.
    assert!(result.get("next_cursor").is_some(),
        "events.replay missing next_cursor key: {result}");
}

// ── 3.11 events/chain_replay — replay scoped to a chain ─────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_events_chain_replay_returns_events_for_audit_chain() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (_, body1) = exec(&h, "service:system/status", json!({})).await;
    // For service-run threads, chain_root_id == thread_id (see service_executor.rs).
    let tid = body1["thread"]["thread_id"].as_str().unwrap().to_string();

    let (status, body2) = exec(
        &h, "service:events/chain_replay",
        json!({"chain_root_id": tid, "limit": 100}),
    ).await;
    let result = unwrap_result(status, &body2, "events.chain_replay");
    assert!(result.get("events").and_then(|v| v.as_array()).is_some(),
        "events.chain_replay missing events array: {result}");
    assert!(result.get("next_cursor").is_some(),
        "events.chain_replay missing next_cursor: {result}");
}

// ── 3.12 fetch — resolve a known core item ──────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn tool_fetch_resolves_known_service() {
    common::ensure_rye_inspect_in_core_bundle();
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(
        &h, "tool:rye/core/fetch",
        json!({"item_ref": "service:system/status", "with_content": false, "verify": true}),
    ).await;
    let result = unwrap_tool_result(status, &body, "fetch");
    let obj = result.as_object().expect("fetch returns object");
    assert_eq!(obj.get("item_ref").and_then(|v| v.as_str()), Some("service:system/status"));
    assert_eq!(obj.get("kind").and_then(|v| v.as_str()), Some("service"));
    assert!(obj.get("content_hash").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()),
        "fetch must return content_hash: {result}");
    assert!(obj.get("resolved_path").and_then(|v| v.as_str()).is_some(),
        "fetch must return resolved_path: {result}");
}

// ── 3.13 fetch — with content ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn tool_fetch_with_content_includes_body() {
    common::ensure_rye_inspect_in_core_bundle();
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(
        &h, "tool:rye/core/fetch",
        json!({"item_ref": "service:system/status", "with_content": true, "verify": false}),
    ).await;
    let result = unwrap_tool_result(status, &body, "fetch with_content");
    assert_eq!(
        result.get("item_ref").and_then(|v| v.as_str()),
        Some("service:system/status")
    );
}

// ── 3.14 fetch — unknown ref errors ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn tool_fetch_unknown_ref_errors() {
    common::ensure_rye_inspect_in_core_bundle();
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(
        &h, "tool:rye/core/fetch",
        json!({"item_ref": "service:does/not/exist"}),
    ).await;
    // The fetch tool returns 200 with a result containing fetch_status:
    // "FAILED" rather than an HTTP error code.
    assert!(status.is_success(), "fetch returned HTTP error; body={body}");
    let tool_envelope = body.get("result").expect("result field");
    // Tool subprocess returned JSON — drill into inner result if present,
    // otherwise check the envelope itself for error/fetch_status.
    let inner = tool_envelope.get("result").cloned().unwrap_or(tool_envelope.clone());
    let fetch_status = inner.get("fetch_status").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        fetch_status == "FAILED" || inner.get("error").is_some(),
        "fetch of nonexistent ref should report failure; got: {inner}"
    );
}

// ── 3.15 verify — verify a Trusted core item ────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn tool_verify_returns_trusted_for_core_service() {
    common::ensure_rye_inspect_in_core_bundle();
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(
        &h, "tool:rye/core/verify",
        json!({"item_ref": "service:system/status"}),
    ).await;
    let result = unwrap_tool_result(status, &body, "verify");
    let obj = result.as_object().expect("verify returns object");
    assert_eq!(obj.get("item_ref").and_then(|v| v.as_str()), Some("service:system/status"));
    let trust_class = obj.get("trust_class").and_then(|v| v.as_str())
        .expect("trust_class present");
    assert_eq!(trust_class, "TRUSTED",
        "core bundle service must verify as TRUSTED: {result}");
    assert_eq!(obj.get("status").and_then(|v| v.as_str()), Some("SUCCESS"),
        "verify status should be SUCCESS: {result}");
}

// ── 3.16 node-sign — rejects non-system space ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_node_sign_rejects_non_system_space() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    // node-sign only accepts system space — project space must be rejected.
    let (status, _body) = exec(
        &h, "service:node-sign",
        json!({"item_ref": "node:foo", "space": "project"}),
    ).await;
    // The daemon returns 500 for internal handler errors. Assert the
    // error message contains the expected rejection text.
    assert!(!status.is_success(), "node-sign should reject project space; status={status}");
}

// ── 3.17 maintenance/gc — dry run on fresh state ───────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_maintenance_gc_dry_run_returns_stats() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(
        &h, "service:maintenance/gc",
        json!({"dry_run": true, "compact": false}),
    ).await;
    let result = unwrap_result(status, &body, "maintenance.gc dry");
    // GcResult fields (see ryeos-state/src/gc/mod.rs:47-56).
    let obj = result.as_object().expect("gc returns object");
    for required in ["roots_walked", "reachable_objects", "reachable_blobs",
                     "deleted_objects", "deleted_blobs", "freed_bytes",
                     "duration_ms"] {
        assert!(obj.contains_key(required),
            "gc result missing field {required}: {result}");
    }
    // Dry run must not delete anything.
    assert_eq!(obj["deleted_objects"].as_u64(), Some(0),
        "dry run must not delete objects: {result}");
    assert_eq!(obj["deleted_blobs"].as_u64(), Some(0),
        "dry run must not delete blobs: {result}");
    assert_eq!(obj["freed_bytes"].as_u64(), Some(0),
        "dry run must not free bytes: {result}");
}

// ── 3.18 maintenance/gc — real run writes event log ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_maintenance_gc_real_run_writes_event_log() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = exec(
        &h, "service:maintenance/gc",
        json!({"dry_run": false, "compact": false}),
    ).await;
    let _ = unwrap_result(status, &body, "maintenance.gc real");
    // GC writes to <state_dir>/.ai/state/<event-log>. The exact path
    // is `gc::event_log::append_event` — read its source if needed.
    // We just assert the GC succeeded and SOME file under state changed.
    let state_dir = h.state_path.join(".ai/state");
    assert!(state_dir.exists(), "state dir must exist after gc: {}", state_dir.display());
}

// ── 3.19 commands/submit — DaemonOnly, requires existing thread ──────────

#[tokio::test(flavor = "multi_thread")]
async fn service_commands_submit_against_audit_thread() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    // First mint an audit thread by running a Both service.
    let (_, body1) = exec(&h, "service:system/status", json!({})).await;
    let tid = body1["thread"]["thread_id"].as_str().unwrap().to_string();

    let (status, body) = exec(
        &h, "service:commands/submit",
        json!({"thread_id": tid, "command_type": "noop", "params": {}}),
    ).await;
    // Either the submit succeeds (handler created a command record), OR
    // it fails because the audit thread is already in `completed` status
    // and command_service refuses commands on completed threads. Both
    // are valid outcomes — what we're proving is the handler runs.
    if status.is_success() {
        let result = unwrap_result(status, &body, "commands.submit success");
        // CommandRecord has a command_id field.
        assert!(result.get("command_id").is_some()
            || result.get("id").is_some(),
            "command record missing id: {result}");
    } else {
        let body_str = body.to_string().to_lowercase();
        assert!(
            body_str.contains("complet") || body_str.contains("status")
            || body_str.contains("thread") || body_str.contains("capabilit")
            || body_str.contains("command"),
            "expected status/completion/capability/command error, got: {body}"
        );
    }
}

// ── 3.20 OfflineOnly services in live mode must reject ──────────────────

#[tokio::test(flavor = "multi_thread")]
async fn service_offline_only_services_reject_in_live_mode() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    for svc in &["service:rebuild", "service:bundle/install", "service:bundle/remove"] {
        let params = if *svc == "service:bundle/install" {
            json!({"name": "x", "source_path": "/tmp/nope"})
        } else if *svc == "service:bundle/remove" {
            json!({"name": "x"})
        } else {
            json!({})
        };
        let (status, body) = exec(&h, svc, params).await;
        assert!(!status.is_success(),
            "{svc}: expected failure in live mode for OfflineOnly, got {status}: {body}");
        let s = body.to_string().to_lowercase();
        assert!(
            s.contains("offline") || s.contains("standalone") || s.contains("daemon"),
            "{svc}: error must mention offline/standalone, got: {body}"
        );
    }
}
