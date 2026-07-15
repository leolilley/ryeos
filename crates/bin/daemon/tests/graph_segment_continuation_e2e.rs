//! End-to-end **segment-cut multi-continuation** for the graph runtime.
//!
//! `graph_crash_recovery_e2e` drives a single crash-and-resume through the
//! reconcile path. This file drives the CLEAN cut path across MULTIPLE segment
//! boundaries in one run: a graph with `segment_steps: 1` checkpoints and cuts a
//! machine continuation after every step, and the daemon launches a successor
//! that resumes from the copied-forward checkpoint (`CheckpointResumeMode::
//! CopyPredecessor`). The chain walks node-by-node across a fresh thread per
//! segment until a return node completes.
//!
//! Two invariants:
//!   1. A multi-node chain completes across several distinct successor threads,
//!      each running exactly one node — the resume started from the copied
//!      checkpoint cursor, not a cold restart that re-runs earlier nodes.
//!   2. A deterministic dispatch rejection does not consume an authored retry
//!      budget across a segment cut. Retry-count checkpoint restoration is
//!      covered at the walker boundary where a retryable callback failure can
//!      be injected deterministically.

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use common::fast_fixture::{register_standard_bundle, FastFixture};
use common::{build_signed_headers_for_bytes, DaemonHarness};
use lillux::crypto::SigningKey;
use serde_json::{json, Value};

/// Plant ZEN_API_KEY in the sealed vault so the graph runtime launch preflight
/// passes (mirrors the crash-recovery + spawn-smoke fixtures).
fn plant_vault_with_zen_key(state_path: &Path) -> anyhow::Result<()> {
    use std::collections::HashMap;
    let pub_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("public_key.pem");
    let pub_key = lillux::vault::read_public_key(&pub_path)?;
    let store_path = ryeos_app::vault::default_sealed_store_path(state_path);
    let secrets = HashMap::from([(
        "ZEN_API_KEY".to_string(),
        "test-zen-api-key-value".to_string(),
    )]);
    ryeos_app::vault::write_sealed_secrets(&store_path, &pub_key, &secrets)?;
    Ok(())
}

/// A four-node gate chain `a → b → c → done(return)` with `segment_steps: 1`.
/// Each gate completion writes a checkpoint whose cursor is the NEXT node and
/// cuts a machine continuation, so the run walks across four threads with no
/// tool/LLM dependency — fully deterministic.
fn plant_segment_chain_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
config:
  start: a
  segment_steps: 1
  nodes:
    a:
      node_type: gate
      assign: {at: "a"}
      next:
        type: conditional
        branches:
          - to: b
    b:
      node_type: gate
      assign: {at: "b"}
      next:
        type: conditional
        branches:
          - to: c
    c:
      node_type: gate
      assign: {at: "c"}
      next:
        type: conditional
        branches:
          - to: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("segment_chain.yaml"), signed)?;
    Ok(())
}

/// A retry-decorated node whose dispatch is deterministically rejected because
/// the referenced tool is not authorized. Authored retry budgets apply only to
/// explicitly retryable failures, so this routes directly to `on_error` and the
/// segment successor terminates cleanly without emitting a retry milestone.
fn plant_retry_segment_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
config:
  start: flaky
  segment_steps: 1
  nodes:
    flaky:
      action: {item_id: "tool:segmenttest/never_resolves"}
      retry: {attempts: 2, backoff_ms: 1}
      on_error: recover
    recover:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("retry_segment.yaml"), signed)?;
    Ok(())
}

/// Every persisted event across ALL threads: `(thread_id, event_type, payload)`
/// ordered by chain sequence. Segment successors are distinct threads, so the
/// whole run must be read across threads, not filtered to one.
fn all_events(state_path: &Path) -> Vec<(String, String, Value)> {
    let db_path = match common::selected_projection_path(state_path) {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    let conn = match rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn
        .prepare("SELECT thread_id, event_type, payload FROM events ORDER BY chain_seq ASC")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt
        .query_map([], |row| {
            let thread_id: String = row.get(0)?;
            let event_type: String = row.get(1)?;
            let payload_blob: Vec<u8> = row.get(2)?;
            let payload: Value = serde_json::from_slice(&payload_blob).unwrap_or(Value::Null);
            Ok((thread_id, event_type, payload))
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default();
    rows
}

fn count_event(events: &[(String, String, Value)], event_type: &str) -> usize {
    events.iter().filter(|(_, ty, _)| ty == event_type).count()
}

fn count_step_started_for_node(events: &[(String, String, Value)], node: &str) -> usize {
    events
        .iter()
        .filter(|(_, ty, payload)| {
            ty == "graph_step_started" && payload.get("node").and_then(|v| v.as_str()) == Some(node)
        })
        .count()
}

/// Fire `/execute` for `item_ref` as a detached background request. The first
/// segment settles `continued` and returns quickly; the rest of the chain runs
/// server-side, so the caller polls the projection for the terminal.
fn spawn_execute(
    h: &DaemonHarness,
    project_path: &Path,
    item_ref: &str,
) -> tokio::task::JoinHandle<()> {
    let url = format!("http://{}/execute", h.bind);
    let body = json!({
        "item_ref": item_ref,
        "ref_bindings": {},
        "project_path": project_path.to_str().unwrap(),
        "parameters": {},
    });
    let body_bytes = serde_json::to_vec(&body).expect("serialize body");
    let headers = build_signed_headers_for_bytes(
        h.user_key.as_ref().expect("user key"),
        h.node_key.as_ref().expect("node key"),
        "POST",
        "/execute",
        &body_bytes,
    );
    tokio::spawn(async move {
        let mut req = reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .body(body_bytes);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let _ = req.send().await;
    })
}

async fn poll_until_completed(
    state_path: &Path,
    deadline: Instant,
    h: &mut DaemonHarness,
) -> Vec<(String, String, Value)> {
    loop {
        let events = all_events(state_path);
        if count_event(&events, "thread_completed") >= 1 {
            return events;
        }
        if Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "no thread_completed appeared across the segment chain.\n\
                 events={events:#?}\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_segment_cuts_resume_across_multiple_continuations() {
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
    plant_segment_chain_graph(project.path(), &fixture.publisher).expect("plant segment chain");

    let exec = spawn_execute(&h, project.path(), "graph:segment_chain");

    let deadline = Instant::now() + Duration::from_secs(60);
    let state_path = h.state_path.clone();
    let events = poll_until_completed(&state_path, deadline, &mut h).await;
    exec.abort();

    // The chain reached a clean terminal.
    assert!(
        count_event(&events, "thread_completed") >= 1,
        "the segment chain must reach thread_completed; events={events:#?}"
    );

    // Each gate ran exactly once — resume started from the copied checkpoint
    // cursor, it did NOT cold-restart and re-run earlier nodes.
    for node in ["a", "b", "c"] {
        assert_eq!(
            count_step_started_for_node(&events, node),
            1,
            "node `{node}` must run exactly once across the whole chain; events={events:#?}"
        );
    }

    // Multiple distinct threads carried the walk — proof of multiple segment
    // cuts (each `segment_steps: 1` boundary launched a fresh successor thread).
    let threads_running_gates: HashSet<&str> = events
        .iter()
        .filter(|(_, ty, payload)| {
            ty == "graph_step_started"
                && matches!(
                    payload.get("node").and_then(|v| v.as_str()),
                    Some("a") | Some("b") | Some("c")
                )
        })
        .map(|(tid, _, _)| tid.as_str())
        .collect();
    assert!(
        threads_running_gates.len() >= 3,
        "gates a/b/c must run on distinct successor threads (>=3 segments), got {}: {:#?}",
        threads_running_gates.len(),
        threads_running_gates
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_nonretryable_failure_does_not_consume_retry_budget_across_segment_cut() {
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
    plant_retry_segment_graph(project.path(), &fixture.publisher)
        .expect("plant retry segment graph");

    let exec = spawn_execute(&h, project.path(), "graph:retry_segment");

    let deadline = Instant::now() + Duration::from_secs(60);
    let state_path = h.state_path.clone();
    let events = poll_until_completed(&state_path, deadline, &mut h).await;
    exec.abort();

    // The run terminated (the exhausted retry routed to the recover return node).
    assert!(
        count_event(&events, "thread_completed") >= 1,
        "the retry+segment chain must reach thread_completed; events={events:#?}"
    );

    // Authorization and authoring failures are deterministic. They must route
    // through on_error without consuming the authored retry budget.
    let retries = count_event(&events, "graph_node_retry");
    assert_eq!(
        retries, 0,
        "a non-retryable dispatch rejection must not emit graph_node_retry; events={events:#?}"
    );
}
