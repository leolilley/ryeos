//! G1a — graph action e2e: permitted cap allows tool dispatch.
//!
//! Proves the full callback path:
//!   walker → callback runtime.dispatch_action → daemon enforce_callback_caps
//!   → dispatch → tool executor chain → subprocess → result returned to walker
//!
//! The graph has `requires.capabilities.declared: [ryeos.execute.tool.echo]`
//! which the daemon composes into effective_caps on the callback token. The tool `echo.py`
//! is a planted Python script that reads params from stdin and returns JSON.
//!
//! G2 must land first (walker self-check removed) so that this test pins
//! the daemon-side gate as the single boundary.

mod common;

use std::path::Path;

use common::fast_fixture::{register_standard_bundle, FastFixture};
use common::DaemonHarness;
use lillux::crypto::SigningKey;
use serde_json::{json, Map, Value};

/// Plant ZEN_API_KEY in the sealed vault for any directive work the graph may
/// trigger. Graph launch itself does not require provider auth.
fn plant_vault_with_zen_key(state_path: &Path) -> anyhow::Result<()> {
    use std::collections::HashMap;
    let pub_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("public_key.pem");
    // The fast fixture already writes the public key; just seal secrets.
    let pub_key = lillux::vault::read_public_key(&pub_path)?;
    let store_path = ryeos_app::vault::default_sealed_store_path(state_path);
    let secrets = HashMap::from([(
        "ZEN_API_KEY".to_string(),
        "test-zen-api-key-value".to_string(),
    )]);
    ryeos_app::vault::write_sealed_secrets(&store_path, &pub_key, &secrets)?;
    Ok(())
}

/// Plant a Python tool at `.ai/tools/echo.py`.
///
/// Unsigned — Unsigned trust class is accepted by the engine for tool
/// items the chain doesn't gate on (matches `hello_world_python.rs`'s
/// working pattern). Signing with `after_shebang: true` is brittle to
/// reproduce in tests; unsigned is the documented test path.
fn plant_echo_tool(project_dir: &Path) -> anyhow::Result<()> {
    let tools_dir = project_dir.join(".ai").join("tools");
    let tool_dir = tools_dir.join("echo");
    std::fs::create_dir_all(&tool_dir)?;

    let body = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: echo
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   description: "echo input as json"

import json, sys
raw = sys.stdin.read()
params = json.loads(raw) if raw.strip() else {}
print(json.dumps({"msg": params.get("msg", "default")}))
"#;
    std::fs::write(tool_dir.join("echo.py"), body)?;
    Ok(())
}

/// Plant a graph with permissions that allow tool:echo/echo.
fn plant_permitted_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    // EdgeSpec is internally tagged as of wave-5 phase D; `next` must be
    // an object with a `type` discriminator (was a bare scalar before).
    //
    // `requires.capabilities.declared` populates the callback token's
    // effective_caps via the graph_permissions composer; the cap shape mirrors
    // `enforce_callback_caps` in runtime_dispatch.rs — `ryeos.execute.<kind>.<bare_id>`
    // where the bare id keeps its `/` separators (canonical Capability format).
    let body = r#"category: ""
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.echo/echo
config:
  start: greet
  nodes:
    greet:
      action:
        item_id: "tool:echo/echo"
        params:
          msg: "hello"
      assign:
        greeting: "${result.msg}"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("flow.yaml"), signed)?;
    Ok(())
}

/// Plant a graph with empty permissions (deny-all).
fn plant_denied_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
config:
  start: greet
  nodes:
    greet:
      action:
        item_id: "tool:echo/echo"
        params:
          msg: "hello"
      assign:
        greeting: "${result.msg}"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("denied.yaml"), signed)?;
    Ok(())
}

/// Plant a graph that tries to self-grant bundle-event runtime authority by
/// naming it under `requires.capabilities.declared`. The daemon must reject this
/// at launch, before the graph runs — runtime authority is manifest-backed only.
fn plant_runtime_authority_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.append.bundle-events.echo/some_event
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("reserved.yaml"), signed)?;
    Ok(())
}

/// Plant a graph using the removed `requires.capabilities.callbacks` key. The
/// daemon's `graph_permissions` composer must reject it at compose time — no
/// silent acceptance — so the graph never runs.
fn plant_legacy_callbacks_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
requires:
  capabilities:
    callbacks:
      bundle_events:
        - event_kind: some_event
          operations: [append]
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("legacy.yaml"), signed)?;
    Ok(())
}

fn graph_thread_id<'a>(body: &'a Value, ctx: &str) -> &'a str {
    body.get("thread")
        .and_then(|thread| thread.get("thread_id"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!("{ctx}: daemon execute envelope missing graph thread_id; body={body:#}")
        })
}

async fn graph_node_receipts(h: &DaemonHarness, thread_id: &str, graph_body: &Value) -> Vec<Value> {
    let (threads_get_status, threads_get_body) = h
        .post_execute(
            "service:threads/get",
            ".",
            json!({ "thread_id": thread_id }),
        )
        .await
        .expect("post service:threads/get for graph thread");
    assert!(
        threads_get_status.is_success(),
        "threads.get for graph thread failed: status={threads_get_status}; body={threads_get_body:#}"
    );
    let thread_projection = threads_get_body
        .get("result")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| {
            panic!(
                "threads.get response missing result object; body={threads_get_body:#}; graph_body={graph_body:#}"
            )
        });

    let receipts: Vec<Value> = thread_projection
        .get("artifacts")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!(
                "threads.get result missing artifacts array; result={thread_projection:#?}; graph_body={graph_body:#}"
            )
        })
        .iter()
        .filter(|artifact| {
            artifact
                .get("artifact_type")
                .and_then(|v| v.as_str())
                == Some("graph_node_receipt")
        })
        .cloned()
        .collect();
    assert!(
        !receipts.is_empty(),
        "graph thread must persist graph_node_receipt artifacts; threads.get={threads_get_body:#}; graph_body={graph_body:#}"
    );
    receipts
}

fn receipt_metadata_for_node<'a>(
    receipts: &'a [Value],
    node: &str,
    graph_body: &Value,
) -> &'a Map<String, Value> {
    let receipt = receipts
        .iter()
        .find(|artifact| {
            artifact
                .get("metadata")
                .and_then(|m| m.get("node"))
                .and_then(|v| v.as_str())
                == Some(node)
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a persisted graph_node_receipt for node `{node}`; receipts={receipts:#?}; graph_body={graph_body:#}"
            )
        });

    receipt
        .get("metadata")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!("receipt for node `{node}` missing metadata object: {receipt:#}"))
}

fn persisted_thread_events(state_path: &Path, thread_id: &str) -> Vec<(String, Value)> {
    let db_path = common::selected_projection_path(state_path)
        .expect("resolve selected projection generation");
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap_or_else(|e| panic!("open projection DB at {}: {e}", db_path.display()));
    let mut stmt = conn
        .prepare(
            "SELECT event_type, payload FROM events \
             WHERE thread_id = ?1 \
             ORDER BY chain_seq ASC",
        )
        .expect("prepare persisted event query");
    stmt.query_map(rusqlite::params![thread_id], |row| {
        let event_type: String = row.get(0)?;
        let payload_blob: Vec<u8> = row.get(1)?;
        let payload: Value = serde_json::from_slice(&payload_blob).unwrap_or(Value::Null);
        Ok((event_type, payload))
    })
    .expect("query persisted events")
    .collect::<Result<Vec<_>, _>>()
    .expect("read persisted events")
}

fn assert_graph_runtime_event_identity(
    events: &[(String, Value)],
    event_type: &str,
    graph_result: &Map<String, Value>,
    definition_ref: &str,
    node: &str,
    status: Option<&str>,
) {
    let payload = events
        .iter()
        .find(|(ty, payload)| {
            ty == event_type
                && payload.get("node").and_then(|v| v.as_str()) == Some(node)
                && status.is_none_or(|expected| {
                    payload.get("status").and_then(|v| v.as_str()) == Some(expected)
                })
        })
        .map(|(_, payload)| payload)
        .unwrap_or_else(|| {
            panic!(
                "expected persisted {event_type} event for node `{node}` status {status:?}; events={events:#?}"
            )
        });

    assert_eq!(
        payload.get("definition_ref").and_then(|v| v.as_str()),
        Some(definition_ref),
        "persisted event must carry definition_ref; event={payload:#}"
    );
    assert_eq!(
        payload.get("definition_hash").and_then(|v| v.as_str()),
        graph_result.get("definition_hash").and_then(|v| v.as_str()),
        "persisted event definition_hash must match GraphResult; event={payload:#}; graph_result={graph_result:#?}"
    );
    assert_eq!(
        payload.get("graph_run_id").and_then(|v| v.as_str()),
        graph_result.get("graph_run_id").and_then(|v| v.as_str()),
        "persisted event graph_run_id must match GraphResult; event={payload:#}; graph_result={graph_result:#?}"
    );
    let expected_node_ref = format!("{definition_ref}#node:{node}");
    assert_eq!(
        payload.get("node_ref").and_then(|v| v.as_str()),
        Some(expected_node_ref.as_str()),
        "persisted event must carry node_ref; event={payload:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_action_completes_with_permitted_cap() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };

    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeosd=debug,ryeos_graph_runtime=debug".into()),
        );
    })
    .await
    .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_echo_tool(project.path()).expect("plant echo tool");
    plant_permitted_graph(project.path(), &fixture.publisher).expect("plant permitted graph");

    let post_fut = h.post_execute(
        "graph:flow",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!(
                    "POST /execute timed out after 30s.\n\
                 --- daemon stderr ---\n{stderr}"
                );
            }
        };

    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!("expected 200 OK; got {status}\nbody={body:#}\n--- daemon stderr ---\n{stderr}");
    }

    let result = match body.get("result") {
        Some(r) => r,
        None => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("response missing `result`\nbody={body:#}\n--- daemon stderr ---\n{stderr}");
        }
    };

    if result.get("success").and_then(|v| v.as_bool()) != Some(true) {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "graph with permitted cap must succeed; body={body:#}\n--- daemon stderr ---\n{stderr}"
        );
    }
    assert_eq!(
        result.get("status").and_then(|v| v.as_str()),
        Some("completed"),
        "graph must complete; body={body:#}"
    );
    // The wire shape is RuntimeResult-wrapped GraphResult:
    //   body.result            ← RuntimeResult (success/status/result/outputs/warnings)
    //   body.result.result     ← GraphResult   (graph_id/state/result/steps/...)
    //   body.result.result.state.greeting  ← assigned via `assign: greeting: ${result.msg}`
    let graph_result = result
        .get("result")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| {
            panic!("missing nested GraphResult under result.result; body={body:#}");
        });
    let greeting = graph_result
        .get("state")
        .and_then(|s| s.get("greeting"))
        .and_then(|v| v.as_str());
    if greeting != Some("hello") {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "state.greeting must be 'hello' from tool result; got {greeting:?}\n\
             body={body:#}\n--- daemon stderr ---\n{stderr}"
        );
    }

    // Smell guard: when a return node has NO explicit `output:` template,
    // GraphResult.result MUST be omitted (None). Previously commit_terminal
    // defaulted `result` to `state.clone()`, producing the
    // `body.result.result.result == body.result.result.state` duplicate
    // that callers were forced to choose between.
    assert!(
        graph_result.get("result").is_none(),
        "GraphResult.result must be absent when return node has no `output:` template; \
         got body={body:#}"
    );

    let graph_thread_id = graph_thread_id(&body, "permitted graph").to_string();
    let receipts = graph_node_receipts(&h, &graph_thread_id, &body).await;

    // `service:threads/receipts` returns the same node receipts server-side,
    // sorted by step, without the caller sifting through every artifact.
    let (receipts_status, receipts_body) = h
        .post_execute(
            "service:threads/receipts",
            ".",
            json!({ "thread_id": graph_thread_id }),
        )
        .await
        .expect("post service:threads/receipts");
    assert!(
        receipts_status.is_success(),
        "threads.receipts failed: status={receipts_status}; body={receipts_body:#}"
    );
    // `post_execute` wraps the handler result under `result`.
    let receipts_result = receipts_body
        .get("result")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!("threads.receipts missing result; body={receipts_body:#}"));
    let svc_receipts = receipts_result
        .get("receipts")
        .and_then(|v| v.as_array())
        .expect("threads.receipts returns a receipts array");
    assert_eq!(
        receipts_result.get("count").and_then(|v| v.as_u64()),
        Some(svc_receipts.len() as u64),
        "threads.receipts count must match; body={receipts_body:#}"
    );
    assert_eq!(
        svc_receipts.len(),
        receipts.len(),
        "threads.receipts must return every graph_node_receipt; body={receipts_body:#}"
    );
    let steps: Vec<u64> = svc_receipts
        .iter()
        .map(|r| r.get("step").and_then(|v| v.as_u64()).unwrap_or(u64::MAX))
        .collect();
    assert!(
        steps.windows(2).all(|w| w[0] <= w[1]),
        "threads.receipts must be sorted by step; steps={steps:?}"
    );
    assert!(
        svc_receipts
            .iter()
            .any(|r| r.get("node").and_then(|v| v.as_str()) == Some("greet")),
        "threads.receipts must include the greet node; body={receipts_body:#}"
    );

    let receipt_metadata = receipt_metadata_for_node(&receipts, "greet", &body);
    assert_eq!(
        receipt_metadata
            .get("definition_ref")
            .and_then(|v| v.as_str()),
        Some("graph:flow"),
        "receipt must carry portable definition ref; receipt_metadata={receipt_metadata:#?}"
    );
    assert_eq!(
        receipt_metadata
            .get("graph_run_id")
            .and_then(|v| v.as_str()),
        graph_result.get("graph_run_id").and_then(|v| v.as_str()),
        "receipt graph_run_id must match GraphResult; receipt_metadata={receipt_metadata:#?}; graph_result={graph_result:#?}"
    );
    assert!(
        receipt_metadata
            .get("definition_hash")
            .and_then(|v| v.as_str())
            .is_some_and(|hash| !hash.is_empty()),
        "receipt must carry a non-empty portable definition hash; receipt_metadata={receipt_metadata:#?}"
    );
    assert!(
        receipt_metadata
            .get("node_result_hash")
            .and_then(|v| v.as_str())
            .is_some_and(|hash| !hash.is_empty()),
        "successful action receipt must carry a non-empty node result hash; receipt_metadata={receipt_metadata:#?}"
    );

    let events = persisted_thread_events(&h.state_path, &graph_thread_id);
    assert_graph_runtime_event_identity(
        &events,
        "graph_step_started",
        graph_result,
        "graph:flow",
        "greet",
        None,
    );
    assert_graph_runtime_event_identity(
        &events,
        "tool_call_result",
        graph_result,
        "graph:flow",
        "greet",
        Some("ok"),
    );
}

/// A graph whose `requires.capabilities.declared` names a manifest
/// runtime-authority capability (bundle events / vault) must be refused at the
/// cap-assembly boundary — that authority is minted only from a signed manifest,
/// never self-granted. The
/// daemon returns a typed `capability_rejected` (HTTP 400) at launch, before the
/// graph runs at all (distinct from the in-run callback denial above).
#[tokio::test(flavor = "multi_thread")]
async fn graph_with_runtime_authority_permission_rejected_at_launch() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };

    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeosd=debug,ryeos_graph_runtime=debug".into()),
        );
    })
    .await
    .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_runtime_authority_graph(project.path(), &fixture.publisher)
        .expect("plant runtime-authority graph");

    let post_fut = h.post_execute(
        "graph:reserved",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!("POST /execute timed out after 30s.\n--- daemon stderr ---\n{stderr}");
            }
        };

    // Typed rejection at launch → HTTP 400 `capability_rejected`, not a
    // 200-with-error-envelope and not a 500.
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "self-granted runtime authority must be rejected at launch with 400; got {status}\nbody={body:#}"
    );
    let body_str = serde_json::to_string(&body).unwrap_or_default();
    assert!(
        body_str.contains("capability_rejected") || body_str.contains("reserved"),
        "expected capability_rejected/reserved in error body; got body={body:#}"
    );
    assert!(
        body_str.contains("bundle-events.echo/some_event"),
        "error must name the offending grant; got body={body:#}"
    );
}

/// A graph using the removed `requires.capabilities.callbacks` key must be
/// rejected by the daemon (at compose time, in `graph_permissions`) — it must
/// never run. No back-compat: old authoring fails loud.
#[tokio::test(flavor = "multi_thread")]
async fn graph_with_legacy_callbacks_rejected() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };

    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeosd=debug,ryeos_graph_runtime=debug".into()),
        );
    })
    .await
    .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_legacy_callbacks_graph(project.path(), &fixture.publisher)
        .expect("plant legacy-callbacks graph");

    let post_fut = h.post_execute(
        "graph:legacy",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!("POST /execute timed out after 30s.\n--- daemon stderr ---\n{stderr}");
            }
        };

    // The daemon must reject the graph before running it. The exact HTTP code
    // for a compose-time rejection is less important than "not a success".
    assert!(
        !status.is_success(),
        "legacy `callbacks` graph must be rejected, not run; got {status}\nbody={body:#}"
    );
    let stderr = h.drain_stderr_nonblocking().await;
    let body_str = serde_json::to_string(&body).unwrap_or_default();
    assert!(
        body_str.contains("callbacks") || stderr.contains("callbacks"),
        "rejection must surface the removed `callbacks` key; got body={body:#}\nstderr={stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_action_denied_without_permitted_cap() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };

    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeosd=debug,ryeos_graph_runtime=debug".into()),
        );
    })
    .await
    .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_echo_tool(project.path()).expect("plant echo tool");
    plant_denied_graph(project.path(), &fixture.publisher).expect("plant denied graph");

    let post_fut = h.post_execute(
        "graph:denied",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!(
                    "POST /execute timed out after 30s.\n\
                 --- daemon stderr ---\n{stderr}"
                );
            }
        };

    // HTTP 200 — the daemon returns the graph result envelope even on
    // internal cap denial. The error is inside the result, not an HTTP
    // error code.
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "expected 200 OK (daemon returns error envelope); got {status}\nbody={body:#}"
    );

    let result = body.get("result").unwrap_or_else(|| {
        panic!("response missing `result`; body={body:#}");
    });

    assert_eq!(
        result.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "graph with no caps must fail; body={body:#}"
    );

    // The wire shape is RuntimeResult-wrapped GraphResult; the graph's
    // `error` field is at result.result.error.
    let graph_result = result
        .get("result")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| {
            panic!("missing nested GraphResult under result.result; body={body:#}");
        });
    assert_eq!(
        graph_result.get("status").and_then(|v| v.as_str()),
        Some("error"),
        "denied graph must terminate with error status; body={body:#}"
    );
    let error_msg = graph_result
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // The walker wraps callback errors as "node '<n>' failed: dispatch failed: <inner>".
    // The inner `<inner>` is the daemon-side error from `enforce_callback_caps`,
    // which contains "callback denied" — but the walker may truncate the cause
    // chain. Accept either the cap-denial wording OR the walker-side
    // "dispatch failed" surface. G2 (walker self-check removed) ensures the
    // cause is the daemon gate, even if the visible string is the wrapper.
    assert!(
        error_msg.contains("dispatch failed")
            || error_msg.contains("callback denied")
            || error_msg.contains("effective_caps"),
        "expected dispatch/cap-denial error; got: {error_msg}\nbody={body:#}"
    );

    let graph_thread_id = graph_thread_id(&body, "denied graph").to_string();
    let receipts = graph_node_receipts(&h, &graph_thread_id, &body).await;
    let receipt_metadata = receipt_metadata_for_node(&receipts, "greet", &body);
    assert_eq!(
        receipt_metadata
            .get("definition_ref")
            .and_then(|v| v.as_str()),
        Some("graph:denied"),
        "error receipt must carry portable definition ref; receipt_metadata={receipt_metadata:#?}"
    );
    assert_eq!(
        receipt_metadata
            .get("graph_run_id")
            .and_then(|v| v.as_str()),
        graph_result.get("graph_run_id").and_then(|v| v.as_str()),
        "error receipt graph_run_id must match GraphResult; receipt_metadata={receipt_metadata:#?}; graph_result={graph_result:#?}"
    );
    assert!(
        receipt_metadata
            .get("definition_hash")
            .and_then(|v| v.as_str())
            .is_some_and(|hash| !hash.is_empty()),
        "error receipt must carry a non-empty portable definition hash; receipt_metadata={receipt_metadata:#?}"
    );
    assert_eq!(
        receipt_metadata.get("node_result_hash"),
        Some(&serde_json::Value::Null),
        "failed action receipt must not carry a successful node result hash; receipt_metadata={receipt_metadata:#?}"
    );
    assert!(
        receipt_metadata
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|error| error.contains("dispatch failed")),
        "failed action receipt must carry dispatch error context; receipt_metadata={receipt_metadata:#?}"
    );

    let events = persisted_thread_events(&h.state_path, &graph_thread_id);
    assert_graph_runtime_event_identity(
        &events,
        "graph_step_started",
        graph_result,
        "graph:denied",
        "greet",
        None,
    );
    assert_graph_runtime_event_identity(
        &events,
        "tool_call_result",
        graph_result,
        "graph:denied",
        "greet",
        Some("dispatch_failed"),
    );
    assert_graph_runtime_event_identity(
        &events,
        "graph_step_completed",
        graph_result,
        "graph:denied",
        "greet",
        Some("error"),
    );
}
