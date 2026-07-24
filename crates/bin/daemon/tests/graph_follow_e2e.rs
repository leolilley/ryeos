//! End-to-end **daemon-managed follow** through the whole OS-level loop.
//!
//! A parent graph has a `follow: true` action node whose action launches a
//! CHILD graph. Instead of dispatching inline, the daemon:
//!   1. suspends the parent at the follow node (parent thread → `continued`,
//!      emitting `graph_follow_suspended`) and launches the child DETACHED as a
//!      fresh chain root,
//!   2. when the child's whole continuation chain reaches terminal, seeds the
//!      parent's follow-resume successor (its own checkpoint spliced with the
//!      child's result) and launches it,
//!   3. the resumed successor consumes the child result at the follow node and
//!      runs the rest of the graph to completion.
//!
//! The parent graph is `fetch (follow) → mark (gate) → done (return)`. The `mark`
//! node lives AFTER the follow node, so a thread that runs `mark` PROVES it
//! resumed past the follow suspend — a re-suspend (broken result consumption)
//! would loop at `fetch` and never reach `mark`. The child graph is
//! `work (gate) → fin (return)`; the unique node names (`work`, `mark`, `fetch`)
//! identify each thread without needing chain columns.
//!
//! Asserted invariants:
//!   - Parent `/execute` returns `status: "continued"` (suspended, not completed).
//!   - EXACTLY ONE `graph_follow_suspended`, on the original parent, at node
//!     `fetch`, naming `graph:child` — no resume successor re-suspends.
//!   - The original parent ran `fetch`, never `mark`, never `thread_completed`.
//!   - parent / child / successor are three DISTINCT threads; the child (ran
//!     `work`) and the successor (ran post-follow `mark`) each reach
//!     `thread_completed` exactly once.
//!   - Value flow: the child returns a sentinel that the resumed parent consumes
//!     (`${result.child_ran}`) and re-returns, so the successor's persisted
//!     result carries it — proving the child's result was actually consumed, not
//!     merely stepped past.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::fast_fixture::{register_config_fixture_bundle, register_standard_bundle, FastFixture};
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::SigningKey;
use serde_json::Value;

/// Plant ZEN_API_KEY in the sealed vault so the graph runtime preflight passes
/// (mirrors `graph_spawn_smoke`).
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

/// Distinctive value the child returns and the parent must consume + re-return.
/// Kept in sync with the literal in `plant_child_graph` below.
const CHILD_SENTINEL: &str = "child-sentinel-9f3a2b";

/// The followed CHILD: `work (gate) → fin (return)`. The `work` gate emits a
/// `graph_step_started(node=work)` that uniquely marks the child thread; the
/// return node emits a real value (`CHILD_SENTINEL`) so we can prove the parent
/// actually consumed the child's result on resume, not merely advanced past the
/// follow node.
fn plant_child_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    // `output:` sets GraphResult.result — the child's terminal envelope `result`.
    let body = r#"category: ""
version: "1.0.0"
config:
  start: work
  nodes:
    work:
      node_type: gate
      next:
        type: conditional
        branches:
          - to: fin
    fin:
      node_type: return
      output:
        child_ran: "child-sentinel-9f3a2b"
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("child.yaml"), signed)?;
    Ok(())
}

/// The PARENT: `fetch (follow → graph:child) → mark (gate) → done (return)`.
/// `requires.capabilities.declared` grants execute authority over the child so
/// the follow admission passes. `mark` runs only AFTER the follow node resumes.
fn plant_parent_follow_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    // `fetch` consumes the child result on resume and assigns the child's value
    // from the graph child's authored return (`result.child_ran`). Graph follow
    // uses the graph kind's declared native-result projection, so the parent sees
    // the authored value rather than the graph runtime's internal GraphResult
    // envelope. `done` re-returns it so the successor's persisted result carries
    // the sentinel iff the follow result was consumed correctly.
    let body = r#"category: ""
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.graph.child
config:
  start: fetch
  nodes:
    fetch:
      node_type: action
      follow: true
      action:
        item_id: "graph:child"
        ref_bindings: {}
        params: {}
      assign:
        child_ran: "${result.child_ran}"
      next:
        type: unconditional
        to: mark
    mark:
      node_type: gate
      next:
        type: conditional
        branches:
          - to: done
    done:
      node_type: return
      output:
        child_ran: "${state.child_ran}"
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("parent.yaml"), signed)?;
    Ok(())
}

/// Every persisted event across ALL threads: `(thread_id, event_type, payload)`.
/// The test owns the whole daemon, so an unfiltered read is exactly the parent +
/// child + successor threads.
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
    stmt.query_map([], |row| {
        let thread_id: String = row.get(0)?;
        let event_type: String = row.get(1)?;
        let payload_blob: Vec<u8> = row.get(2)?;
        let payload: Value = serde_json::from_slice(&payload_blob).unwrap_or(Value::Null);
        Ok((thread_id, event_type, payload))
    })
    .and_then(|m| m.collect::<Result<Vec<_>, _>>())
    .unwrap_or_default()
}

fn all_threads(state_path: &Path) -> Vec<ryeos_state::queries::ThreadRow> {
    let projection_path = match common::selected_projection_path(state_path) {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    let projection = match ryeos_state::projection::ProjectionDb::open(&projection_path) {
        Ok(projection) => projection,
        Err(_) => return Vec::new(),
    };
    ryeos_state::queries::list_threads(&projection, 200).unwrap_or_default()
}

/// True iff `thread_id` has an event of `event_type` whose payload `node` == `node`.
fn thread_ran_node(
    events: &[(String, String, Value)],
    thread_id: &str,
    event_type: &str,
    node: &str,
) -> bool {
    events.iter().any(|(tid, ty, payload)| {
        tid == thread_id
            && ty == event_type
            && payload.get("node").and_then(|v| v.as_str()) == Some(node)
    })
}

fn thread_has_event(events: &[(String, String, Value)], thread_id: &str, event_type: &str) -> bool {
    events
        .iter()
        .any(|(tid, ty, _)| tid == thread_id && ty == event_type)
}

/// Thread ids (deduped, order-preserving) that ran `graph_step_started(node)`.
fn threads_that_ran(events: &[(String, String, Value)], node: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (tid, ty, payload) in events {
        if ty == "graph_step_started"
            && payload.get("node").and_then(|v| v.as_str()) == Some(node)
            && !out.contains(tid)
        {
            out.push(tid.clone());
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_follow_suspends_launches_child_and_resumes_parent() {
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
    plant_child_graph(project.path(), &fixture.publisher).expect("plant child graph");
    plant_parent_follow_graph(project.path(), &fixture.publisher).expect("plant parent graph");

    // 1. Execute the parent. It suspends at the follow node and returns
    //    `continued` — NOT `completed` — while the child runs detached.
    let post_fut = h.post_execute(
        "graph:parent",
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

    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!("expected 200 OK; got {status}\nbody={body:#}\n--- daemon stderr ---\n{stderr}");
    }
    let result = body
        .get("result")
        .unwrap_or_else(|| panic!("response missing `result`; body={body:#}"));
    assert_eq!(
        result.get("status").and_then(|v| v.as_str()),
        Some("continued"),
        "parent graph must SUSPEND at the follow node (status=continued), not run inline; body={body:#}"
    );
    let parent_tid = body
        .get("thread")
        .and_then(|t| t.get("thread_id"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("execute envelope missing parent thread_id; body={body:#}"))
        .to_string();

    // 2. Poll the projection until the child chain completed AND a distinct
    //    successor thread ran the post-follow `mark` node to completion.
    let deadline = Instant::now() + Duration::from_secs(60);
    let events = loop {
        let events = all_events(&h.state_path);

        // Child: a thread that ran `work` and reached terminal.
        let child_done = threads_that_ran(&events, "work")
            .into_iter()
            .any(|tid| thread_has_event(&events, &tid, "thread_completed"));

        // Successor: a thread OTHER than the original parent that ran the
        // post-follow `mark` node and reached terminal.
        let successor_done = threads_that_ran(&events, "mark")
            .into_iter()
            .any(|tid| tid != parent_tid && thread_has_event(&events, &tid, "thread_completed"));

        if child_done && successor_done {
            break events;
        }
        if Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "follow round trip did not complete in 60s \
                 (child_done={child_done}, successor_done={successor_done}).\n\
                 parent_tid={parent_tid}\nevents={events:#?}\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // 3. EXACTLY ONE suspend, on the original parent, at `fetch`, for graph:child.
    //    More than one would mean a resume successor re-suspended (broken result
    //    consumption looping at the follow node).
    let suspends: Vec<&(String, String, Value)> = events
        .iter()
        .filter(|(_, ty, _)| ty == "graph_follow_suspended")
        .collect();
    assert_eq!(
        suspends.len(),
        1,
        "follow must suspend EXACTLY once (no re-suspend); events={events:#?}"
    );
    let (suspend_tid, _, suspend_payload) = suspends[0];
    assert_eq!(
        suspend_tid, &parent_tid,
        "the suspend must be on the original parent thread; events={events:#?}"
    );
    assert_eq!(
        suspend_payload.get("node").and_then(|v| v.as_str()),
        Some("fetch"),
        "suspend must be at the follow node `fetch`; payload={suspend_payload:#}"
    );
    assert_eq!(
        suspend_payload.get("item_id").and_then(|v| v.as_str()),
        Some("graph:child"),
        "suspend must name the child item; payload={suspend_payload:#}"
    );

    // 4. The original parent ran `fetch`, never `mark`, never completed.
    assert!(
        thread_ran_node(&events, &parent_tid, "graph_step_started", "fetch"),
        "parent thread must have started the follow node `fetch`; events={events:#?}"
    );
    assert!(
        !thread_ran_node(&events, &parent_tid, "graph_step_started", "mark"),
        "the ORIGINAL parent thread must NOT run the post-follow `mark` node — it \
         suspended; the successor runs `mark`; events={events:#?}"
    );
    assert!(
        !thread_has_event(&events, &parent_tid, "thread_completed"),
        "the original parent thread suspended (continued), it must not be \
         thread_completed; events={events:#?}"
    );

    // 5. Child and successor are exactly one each, and parent / child / successor
    //    are three DISTINCT threads (proves the child ran detached and the parent
    //    resumed in a separate successor — not all on one thread).
    let child_tids: Vec<String> = threads_that_ran(&events, "work")
        .into_iter()
        .filter(|tid| thread_has_event(&events, tid, "thread_completed"))
        .collect();
    assert_eq!(
        child_tids.len(),
        1,
        "exactly one child thread must run `work` to completion; events={events:#?}"
    );
    let mark_tids = threads_that_ran(&events, "mark");
    assert_eq!(
        mark_tids.len(),
        1,
        "exactly one thread must run the post-follow `mark` node; events={events:#?}"
    );
    let child_tid = &child_tids[0];
    let successor_tid = &mark_tids[0];
    assert_ne!(
        child_tid, &parent_tid,
        "the child must run detached, not on the parent thread"
    );
    assert_ne!(
        successor_tid, &parent_tid,
        "the successor must be distinct from the suspended parent"
    );
    assert_ne!(
        child_tid, successor_tid,
        "child and successor must be different threads"
    );
    assert!(
        thread_has_event(&events, successor_tid, "thread_completed"),
        "the successor must reach thread_completed; events={events:#?}"
    );

    // 6. Value flow: the child's returned value reached the resumed parent's
    //    persisted result. A missing / mis-shaped follow result would break the
    //    `${result.child_ran}` assign and the sentinel would be absent.
    let (get_status, get_body) = h
        .post_execute(
            "service:threads/get",
            project.path().to_str().unwrap(),
            serde_json::json!({ "thread_id": successor_tid }),
        )
        .await
        .expect("post service:threads/get for successor");
    assert!(
        get_status.is_success(),
        "threads.get for successor failed: status={get_status}; body={get_body:#}"
    );
    let get_str = serde_json::to_string(&get_body).unwrap_or_default();
    assert!(
        get_str.contains(CHILD_SENTINEL),
        "the resumed parent's persisted result must carry the child's returned value \
         `{CHILD_SENTINEL}` (proves the follow result was consumed); threads/get={get_body:#}"
    );
}

// ══ #25 daemon e2e: child failure routes the parent into on_error ══════════════

/// A CHILD graph that FAILS: its action dispatches an item the child has no cap for
/// (the follow child declares no caps), so the dispatch is denied and the child
/// terminates with a failure envelope.
fn plant_failing_child_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
config:
  start: boom
  nodes:
    boom:
      action:
        item_id: "tool:nonexistent/boom"
        ref_bindings: {}
        params: {}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("child_fail.yaml"), signed)?;
    Ok(())
}

/// A PARENT following the failing child with an `on_error` branch: the child's
/// failure must route the resumed parent to `recover`, NOT the success `unreached`.
fn plant_parent_on_error_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.graph.child_fail
config:
  start: fetch
  nodes:
    fetch:
      node_type: action
      follow: true
      action:
        item_id: "graph:child_fail"
        ref_bindings: {}
        params: {}
      on_error: recover
      next:
        type: unconditional
        to: unreached
    recover:
      node_type: gate
      next:
        type: conditional
        branches:
          - to: done
    unreached:
      node_type: gate
      next:
        type: conditional
        branches:
          - to: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("parent_onerr.yaml"), signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_follow_child_failure_routes_parent_on_error() {
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
    .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_failing_child_graph(project.path(), &fixture.publisher).expect("plant failing child");
    plant_parent_on_error_graph(project.path(), &fixture.publisher).expect("plant parent");

    let post_fut = h.post_execute(
        "graph:parent_onerr",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!("POST /execute timed out.\n--- daemon stderr ---\n{stderr}");
            }
        };
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    assert_eq!(
        body.get("result")
            .and_then(|r| r.get("status"))
            .and_then(|v| v.as_str()),
        Some("continued"),
        "parent must suspend at the follow node; body={body:#}"
    );
    let parent_tid = body
        .get("thread")
        .and_then(|t| t.get("thread_id"))
        .and_then(|v| v.as_str())
        .expect("parent thread id")
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(60);
    let events = loop {
        let events = all_events(&h.state_path);
        let child_terminal = threads_that_ran(&events, "boom").into_iter().any(|tid| {
            thread_has_event(&events, &tid, "thread_failed")
                || thread_has_event(&events, &tid, "thread_completed")
        });
        let recovered = threads_that_ran(&events, "recover")
            .into_iter()
            .any(|tid| tid != parent_tid && thread_has_event(&events, &tid, "thread_completed"));
        if child_terminal && recovered {
            break events;
        }
        if Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "failure round trip did not complete (child_terminal={child_terminal}, recovered={recovered}).\nevents={events:#?}\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // The resumed successor took the on_error branch (`recover`), NOT the success
    // branch (`unreached`) — the child's FAILURE routed the parent into on_error.
    let recover_tids = threads_that_ran(&events, "recover");
    assert_eq!(
        recover_tids.len(),
        1,
        "exactly one successor runs the on_error `recover` node; events={events:#?}"
    );
    assert_ne!(
        &recover_tids[0], &parent_tid,
        "recover runs in the resumed successor, not the suspended parent"
    );
    assert!(
        threads_that_ran(&events, "unreached").is_empty(),
        "the success `unreached` branch must NOT run on child failure; events={events:#?}"
    );
}

// ══ #25 daemon e2e: two sequential follow nodes ════════════════════════════════

fn plant_seq_child_a(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
config:
  start: worka
  nodes:
    worka:
      node_type: gate
      next:
        type: conditional
        branches:
          - to: fin
    fin:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("child_a.yaml"), signed)?;
    Ok(())
}

fn plant_seq_directive_child_b(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let directives_dir = project_dir.join(".ai/directives");
    std::fs::create_dir_all(&directives_dir)?;
    let body = r#"---
name: child_b
category: ""
description: "resumed-successor dotenv overlay e2e child"
required_secrets:
  - OVERLAY_CHAIN_SECRET
model:
  tier: general
---
Reply with a short acknowledgement.
"#;
    let signed = lillux::signature::sign_content(body, signer, "<!--", Some("-->"));
    std::fs::write(directives_dir.join("child_b.md"), signed)?;
    Ok(())
}

/// fetch1 (follow child_a) → fetch2 (follow child_b) → done.
fn plant_parent_sequential_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.graph.child_a
      - ryeos.execute.directive.child_b
config:
  start: fetch1
  nodes:
    fetch1:
      node_type: action
      follow: true
      action:
        item_id: "graph:child_a"
        ref_bindings: {}
        params: {}
      next:
        type: unconditional
        to: fetch2
    fetch2:
      node_type: action
      follow: true
      action:
        item_id: "directive:child_b"
        ref_bindings:
          model: "directive:child_b"
        params: {}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("parent_seq.yaml"), signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_follow_two_sequential_nodes_suspend_and_resume_in_order() {
    let mock = MockProvider::start(vec![MockResponse::Text("child b completed".into())]).await;
    let mock_url = mock.base_url.clone();
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-follow-overlay-model-config",
            fixture,
            |bundle_root| plant_mock_provider(bundle_root, &mock_url, &fixture.publisher),
        )?;
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
    .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    // Only the second child needs this project-local secret. It is spawned by
    // the continuation successor after the first follow suspend/resume, so the
    // launch proves that materialized live-FS provenance retained the original
    // project as its `.env` overlay source. The fixture vault deliberately does
    // not contain this name.
    std::fs::write(
        project.path().join(".env"),
        "OVERLAY_CHAIN_SECRET=project-overlay-value\n",
    )
    .expect("plant project dotenv overlay");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant model routing");
    plant_seq_child_a(project.path(), &fixture.publisher).expect("plant child_a");
    plant_seq_directive_child_b(project.path(), &fixture.publisher)
        .expect("plant directive child_b");
    plant_parent_sequential_graph(project.path(), &fixture.publisher).expect("plant parent");

    let post_fut = h.post_execute(
        "graph:parent_seq",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!("POST /execute timed out.\n--- daemon stderr ---\n{stderr}");
            }
        };
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    assert_eq!(
        body.get("result")
            .and_then(|r| r.get("status"))
            .and_then(|v| v.as_str()),
        Some("continued"),
        "parent must suspend at the first follow node; body={body:#}"
    );
    let parent_tid = body
        .get("thread")
        .and_then(|t| t.get("thread_id"))
        .and_then(|v| v.as_str())
        .expect("parent thread id")
        .to_string();

    // Full sequence resolved when TWO graphs have completed (child_a and the final
    // parent successor) and the directive child_b has completed. The suspended
    // parent + first successor are `continued`, never `graph_completed`.
    let deadline = Instant::now() + Duration::from_secs(90);
    let events = loop {
        let events = all_events(&h.state_path);
        let completed = events
            .iter()
            .filter(|(_, et, _)| et == "graph_completed")
            .count();
        let child_b_thread = events.iter().find_map(|(thread_id, event_type, payload)| {
            (event_type == "thread_created"
                && payload.get("item_ref").and_then(Value::as_str) == Some("directive:child_b"))
            .then_some(thread_id)
        });
        let child_b_completed = child_b_thread
            .is_some_and(|thread_id| thread_has_event(&events, thread_id, "thread_completed"));
        if completed >= 2 && child_b_completed {
            break events;
        }
        if Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "sequential follow did not complete (graph_completed={completed}/2, directive_child_completed={child_b_completed}).\nevents={events:#?}\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // Both children ran their work nodes to completion.
    assert!(
        threads_that_ran(&events, "worka")
            .into_iter()
            .any(|t| thread_has_event(&events, &t, "thread_completed")),
        "child_a must run `worka` to completion; events={events:#?}"
    );
    let child_b_thread = events
        .iter()
        .find_map(|(thread_id, event_type, payload)| {
            (event_type == "thread_created"
                && payload.get("item_ref").and_then(Value::as_str) == Some("directive:child_b"))
            .then_some(thread_id)
        })
        .expect("directive child_b thread");
    assert!(
        thread_has_event(&events, child_b_thread, "thread_completed"),
        "directive child_b must complete with its project dotenv secret; events={events:#?}"
    );

    // EXACTLY two follow suspends, at fetch1 then fetch2, on DISTINCT threads: the
    // original parent suspends at fetch1; its resumed successor suspends at fetch2.
    let suspends: Vec<(String, String)> = events
        .iter()
        .filter(|(_, et, _)| et == "graph_follow_suspended")
        .filter_map(|(tid, _, p)| {
            p.get("node")
                .and_then(|v| v.as_str())
                .map(|n| (tid.clone(), n.to_string()))
        })
        .collect();
    assert_eq!(
        suspends.len(),
        2,
        "exactly two follow suspends; got {suspends:?}"
    );
    let fetch1 = suspends
        .iter()
        .find(|(_, n)| n == "fetch1")
        .expect("a fetch1 suspend");
    let fetch2 = suspends
        .iter()
        .find(|(_, n)| n == "fetch2")
        .expect("a fetch2 suspend");
    assert_eq!(
        fetch1.0, parent_tid,
        "fetch1 suspends the original parent thread"
    );
    assert_ne!(
        fetch1.0, fetch2.0,
        "fetch2 suspends a DISTINCT resumed successor thread"
    );
}

// ══ #25 daemon e2e: followed child cost appears in the resumed parent ══════════

/// Plant the mock `chat_completions` provider (the mock returns a fixed
/// `usage: {prompt_tokens: 10, completion_tokens: 5}` on every call).
fn plant_mock_provider(
    root: &Path,
    mock_base_url: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"base_url: "{mock_base_url}"
family: chat_completions
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
auth: {{}}
headers: {{}}
schemas:
  accounting:
    require_usage: true
  streaming:
    mode: delta_merge
  output_limit:
    path: max_tokens
    semantics: provider_native_output_tokens
pricing:
  input_per_million: 0.0
  output_per_million: 0.0
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

/// Map the `general` tier to the mock provider.
fn plant_model_routing(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = r#"tiers:
  general:
    provider: mock
    model: mock-model
    context_window: 200000
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

/// A cost-bearing follow CHILD: a directive that calls the (mock) LLM, incurring
/// 10 input / 5 output tokens.
fn plant_cost_directive_child(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = project_dir.join(".ai/directives");
    std::fs::create_dir_all(&dir)?;
    let body = r#"---
name: costchild
category: ""
description: "follow cost e2e child"
model:
  tier: general
---
Say hello.
"#;
    let signed = lillux::signature::sign_content(body, signer, "<!--", Some("-->"));
    std::fs::write(dir.join("costchild.md"), signed)?;
    Ok(())
}

/// A PARENT graph that follows the cost-bearing directive child.
fn plant_parent_cost_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.costchild
config:
  start: fetch
  nodes:
    fetch:
      node_type: action
      follow: true
      action:
        item_id: "directive:costchild"
        ref_bindings:
          model: "directive:costchild"
        params: {}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("parent_cost.yaml"), signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_follow_child_cost_flows_into_resumed_parent() {
    let mock = MockProvider::start(vec![MockResponse::Text("hi from child".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-follow-model-config",
            fixture,
            |bundle_root| plant_mock_provider(bundle_root, &mock_url, &fixture.publisher),
        )?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeosd=debug,ryeos_graph_runtime=debug,ryeos_directive_runtime=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with mock provider + standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_cost_directive_child(project.path(), &fixture.publisher).expect("plant child directive");
    plant_parent_cost_graph(project.path(), &fixture.publisher).expect("plant parent");

    let post_fut = h.post_execute(
        "graph:parent_cost",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(45), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!("POST /execute timed out.\n--- daemon stderr ---\n{stderr}");
            }
        };
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    assert_eq!(
        body.get("result")
            .and_then(|r| r.get("status"))
            .and_then(|v| v.as_str()),
        Some("continued"),
        "parent must suspend at the follow node; body={body:#}"
    );
    let parent_tid = body
        .get("thread")
        .and_then(|t| t.get("thread_id"))
        .and_then(|v| v.as_str())
        .expect("parent thread id")
        .to_string();

    // The suspended parent never completes; only the resumed successor emits a
    // graph_completed. Wait for it, then read its persisted result.
    let deadline = Instant::now() + Duration::from_secs(90);
    let successor_tid = loop {
        let events = all_events(&h.state_path);
        let successor = events
            .iter()
            .find(|(tid, et, _)| et == "graph_completed" && *tid != parent_tid)
            .map(|(tid, _, _)| tid.clone());
        if let Some(tid) = successor {
            break tid;
        }
        if Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "resumed parent never completed.\nevents={:#?}\n--- daemon stderr ---\n{stderr}",
                all_events(&h.state_path)
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // The child's 10 input tokens must appear in the resumed parent successor's
    // persisted result (the follow node accounts the child's cost on resume).
    let (get_status, get_body) = h
        .post_execute(
            "service:threads/get",
            project.path().to_str().unwrap(),
            serde_json::json!({ "thread_id": successor_tid }),
        )
        .await
        .expect("post service:threads/get for successor");
    assert!(
        get_status.is_success(),
        "threads.get failed: status={get_status}; body={get_body:#}"
    );
    drop(mock);

    // Prove the resumed parent recorded the child's cost in its FOLLOW-NODE RECEIPT
    // (not merely somewhere in the response string): the `fetch` graph_node_receipt
    // must carry the child's 10/5 token cost and no error.
    let result = get_body.get("result").expect("threads/get result");
    let artifacts = result
        .get("artifacts")
        .and_then(|v| v.as_array())
        .expect("threads/get artifacts array");
    let fetch_receipt = artifacts
        .iter()
        .find(|a| {
            a.get("artifact_type").and_then(|v| v.as_str()) == Some("graph_node_receipt")
                && a.get("metadata")
                    .and_then(|m| m.get("node"))
                    .and_then(|v| v.as_str())
                    == Some("fetch")
        })
        .unwrap_or_else(|| {
            panic!(
                "resumed parent must persist a graph_node_receipt for the follow node `fetch`; \
                 threads/get={get_body:#}"
            )
        });
    assert_eq!(
        fetch_receipt["metadata"]["cost"]["input_tokens"],
        serde_json::json!(10),
        "the fetch receipt must record the followed child's 10 input tokens; receipt={fetch_receipt:#}"
    );
    assert_eq!(
        fetch_receipt["metadata"]["cost"]["output_tokens"],
        serde_json::json!(5),
        "the fetch receipt must record the followed child's 5 output tokens; receipt={fetch_receipt:#}"
    );
    assert_eq!(
        fetch_receipt["metadata"]["error"],
        serde_json::Value::Null,
        "the followed child succeeded, so the fetch receipt must carry no error; receipt={fetch_receipt:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_follow_live_input_records_service_under_the_callers_exact_project_authority() {
    // Hold the followed directive inside its provider call long enough to
    // exercise the running-thread input path. The second response covers the
    // additional turn if the queued interrupt is folded before the child settles.
    let mock = MockProvider::start_with_response_delay(
        vec![
            MockResponse::Text("first child turn".into()),
            MockResponse::Text("interrupted child turn".into()),
        ],
        Duration::from_secs(3),
    )
    .await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-follow-live-input-model-config",
            fixture,
            |bundle_root| plant_mock_provider(bundle_root, &mock_url, &fixture.publisher),
        )?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeosd=debug,ryeos_graph_runtime=debug,ryeos_directive_runtime=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with delayed mock provider + standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_cost_directive_child(project.path(), &fixture.publisher).expect("plant child directive");
    plant_parent_cost_graph(project.path(), &fixture.publisher).expect("plant parent graph");
    let project_path = project.path().to_str().expect("utf-8 project path");
    let input_project = tempfile::tempdir().expect("operator input project tempdir");
    std::fs::create_dir(input_project.path().join(".ai"))
        .expect("create operator input project descriptor root");
    let input_project_root = input_project
        .path()
        .canonicalize()
        .expect("canonical operator input project");
    let input_project_path = input_project_root
        .to_str()
        .expect("utf-8 operator input project path");

    let (status, body) = h
        .post_execute("graph:parent_cost", project_path, serde_json::json!({}))
        .await
        .expect("launch graph-follow parent");
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    assert_eq!(
        body.pointer("/result/status").and_then(Value::as_str),
        Some("continued"),
        "parent must suspend while the directive child runs; body={body:#}"
    );
    let parent_id = body
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .expect("parent thread id")
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(10);
    let (parent, child) = loop {
        let threads = all_threads(&h.state_path);
        let parent = threads.iter().find(|thread| thread.thread_id == parent_id);
        let child = threads
            .iter()
            .find(|thread| thread.item_ref == "directive:costchild" && thread.status == "running");
        if let (Some(parent), Some(child)) = (parent, child) {
            break (parent.clone(), child.clone());
        }
        if Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "followed directive child never became observably running.\n\
                 threads={threads:#?}\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        child.project_authority, parent.project_authority,
        "the default inherited follow path must retain the sealed parent authority; callback effective caps must not rewrite its ceiling"
    );

    let (input_status, input_body) = h
        .post_execute(
            "service:threads/input",
            input_project_path,
            serde_json::json!({
                "input": "incorporate this public ARC observation",
                "target": {
                    "kind": "thread",
                    "thread_id": child.thread_id.clone(),
                },
                "intent": "interrupt",
            }),
        )
        .await
        .expect("submit live input to followed directive child");
    assert_eq!(
        input_status,
        reqwest::StatusCode::OK,
        "recorded threads.input invocation must reach its handler; body={input_body:#}"
    );
    assert_eq!(
        input_body
            .pointer("/result/delivery")
            .and_then(Value::as_str),
        Some("submitted"),
        "running child input must use the live delivery path; body={input_body:#}"
    );
    assert_eq!(
        input_body
            .pointer("/result/thread_id")
            .and_then(Value::as_str),
        Some(child.thread_id.as_str()),
        "live input must target the existing followed child without minting a successor"
    );
    assert!(
        input_body
            .pointer("/result/notice")
            .and_then(Value::as_str)
            .is_some_and(|notice| notice.starts_with("Input queued")),
        "the valid persisted child process identity must receive the interrupt without steer degradation; body={input_body:#}"
    );
    let service_id = input_body
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .expect("recorded service invocation thread id")
        .to_string();
    assert_ne!(
        service_id, child.thread_id,
        "the service audit root is distinct from the unchanged target child"
    );

    let input_text = "incorporate this public ARC observation";
    let completion_deadline = Instant::now() + Duration::from_secs(90);
    let (successor_id, terminal_events) = loop {
        let events = all_events(&h.state_path);
        let projected_threads = all_threads(&h.state_path);
        let child_completed = thread_has_event(&events, &child.thread_id, "thread_completed");
        let successor = events
            .iter()
            .find(|(thread_id, event_type, _)| {
                event_type == "graph_completed" && thread_id != &parent_id
            })
            .map(|(thread_id, _, _)| thread_id.clone());
        let child_projection_completed = projected_threads
            .iter()
            .any(|thread| thread.thread_id == child.thread_id && thread.status == "completed");
        let successor_projection_completed = successor.as_ref().is_some_and(|successor_id| {
            projected_threads
                .iter()
                .any(|thread| thread.thread_id == *successor_id && thread.status == "completed")
        });
        let folded_inputs = events
            .iter()
            .filter(|(thread_id, event_type, payload)| {
                thread_id == &child.thread_id
                    && event_type == "cognition_in"
                    && payload.get("content").and_then(Value::as_str) == Some(input_text)
            })
            .count();
        if child_completed
            && child_projection_completed
            && successor_projection_completed
            && folded_inputs == 1
        {
            break (successor.expect("checked successor"), events);
        }
        if Instant::now() >= completion_deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "live input did not fold exactly once through child completion and graph resume.\n\
                 folded_inputs={folded_inputs}\n\
                 child_completed={child_completed}\n\
                 child_projection_completed={child_projection_completed}\n\
                 successor_projection_completed={successor_projection_completed}\n\
                 projected_threads={projected_threads:#?}\n\
                 events={events:#?}\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    assert_ne!(successor_id, parent_id);
    assert_ne!(successor_id, child.thread_id);
    assert!(
        thread_has_event(&terminal_events, &child.thread_id, "thread_completed"),
        "the authentic followed child must complete before graph resumption"
    );
    assert_eq!(
        terminal_events
            .iter()
            .filter(|(thread_id, event_type, payload)| {
                thread_id == &child.thread_id
                    && event_type == "cognition_in"
                    && payload.get("content").and_then(Value::as_str) == Some(input_text)
            })
            .count(),
        1,
        "the submitted operator input must be folded exactly once"
    );

    let threads = all_threads(&h.state_path);
    let service = threads
        .iter()
        .find(|thread| thread.thread_id == service_id)
        .unwrap_or_else(|| panic!("recorded service root {service_id} missing: {threads:#?}"));
    assert_eq!(service.item_ref, "service:threads/input");
    assert_eq!(service.status, "completed");
    assert_ne!(
        service.project_authority, parent.project_authority,
        "the service audit root must use the input caller's project rather than copying target authority"
    );
    assert_eq!(
        service.project_root.as_deref(),
        Some(
            input_project_root
                .to_str()
                .expect("utf-8 canonical input root")
        ),
        "recorded service root must project the input caller's canonical project root"
    );
    assert_ne!(
        service.project_root, parent.project_root,
        "service audit authority and target execution authority must remain distinct"
    );
    assert!(service.base_project_snapshot_hash.is_none());

    let matching_children = threads
        .iter()
        .filter(|thread| thread.item_ref == "directive:costchild")
        .collect::<Vec<_>>();
    assert_eq!(
        matching_children.len(),
        1,
        "live input must not create a second directive child: {matching_children:#?}"
    );
    assert_eq!(matching_children[0].thread_id, child.thread_id);
    assert_eq!(
        matching_children[0].chain_root_id, child.chain_root_id,
        "live input must not change the followed child's chain root"
    );
    assert_eq!(
        matching_children[0].upstream_thread_id, child.upstream_thread_id,
        "live input must not change the followed child's upstream lineage"
    );
    assert_eq!(
        matching_children[0].project_authority, child.project_authority,
        "live input must not change the followed child's execution authority"
    );
    assert_eq!(
        matching_children[0].status, "completed",
        "the interrupted child must resume cognition and settle normally"
    );
    let original_parent = threads
        .iter()
        .find(|thread| thread.thread_id == parent_id)
        .expect("original suspended parent row");
    assert_eq!(
        original_parent.status, "continued",
        "the original graph parent remains the suspended predecessor"
    );
    let successor = threads
        .iter()
        .find(|thread| thread.thread_id == successor_id)
        .expect("resumed graph successor row");
    assert_eq!(successor.status, "completed");
    assert_eq!(
        successor.project_authority, parent.project_authority,
        "graph resumption must preserve the parent execution authority"
    );
}
