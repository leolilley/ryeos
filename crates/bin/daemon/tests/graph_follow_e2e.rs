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
//!     (`${result.result.child_ran}`) and re-returns, so the successor's persisted
//!     result carries it — proving the child's result was actually consumed, not
//!     merely stepped past.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::fast_fixture::{register_standard_bundle, FastFixture};
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
      assign: {tick: true}
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
    // into state via `${result.result.child_ran}` — for a graph child the follow
    // result binds to the child's bare GraphResult, so its output lives at
    // `result.result`. `done` re-returns it so the successor's persisted result
    // carries the sentinel iff the follow result was consumed correctly.
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
        params: {}
      assign:
        child_ran: "${result.result.child_ran}"
      next:
        type: unconditional
        to: mark
    mark:
      node_type: gate
      assign: {seen: true}
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
    let db_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/projection.sqlite3");
    let conn = match rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT thread_id, event_type, payload FROM events ORDER BY chain_seq ASC",
    ) {
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
    //    `${result.result.child_ran}` assign and the sentinel would be absent.
    let (get_status, get_body) = h
        .post_execute(
            "service:threads/get",
            ".",
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
