//! End-to-end **graph crash recovery** through the daemon's managed
//! native-resume path (V5.5 Blocker-2 closeout).
//!
//! `graph_native_resume_after_restart_e2e.rs` pins the load-bearing
//! checkpoint primitives in isolation. This file drives the WHOLE loop
//! at the OS level, exercising the exact seam the Blocker-2 fix added:
//!
//!   1. Launch a multi-step graph (`a` → `b` → `c`). The graph is a
//!      runtime-registry kind that declares `native_resume` and
//!      `supports_continuation: true`, so the managed launch captures a
//!      `ResumeContext` carrying `runtime_ref: graph` and allocates a
//!      per-thread checkpoint dir.
//!   2. A prod-inert test hook (`RYEOS_GRAPH_TEST_BLOCK_AFTER_CHECKPOINT`)
//!      parks the graph runtime AFTER it durably writes the checkpoint
//!      whose cursor is node `b` — i.e. node `a` is done, `b` is next.
//!   3. SIGKILL the daemon (it never observes the child exit, so the
//!      thread row stays `running`) and leave the exact orphan group alive.
//!   4. Respawn the daemon. Holding the single-daemon state lock, startup
//!      reconcile pins and hard-kills that exact prior group, compare-clears
//!      its attachment, classifies the row `NativeResume`, and routes it through
//!      `launch_existing_native_resume` → the managed envelope path. The
//!      resumed launch injects `RYEOS_RESUME=1`, so the park hook does NOT
//!      fire and the walker resumes from node `b` to completion.
//!
//! Asserted invariants (the heart of Blocker-2 + the Oracle refinements):
//!   - `thread_started` is emitted EXACTLY ONCE across both runs — the
//!     resumed runtime's `runtime.mark_running` callback hits the
//!     idempotent `running → running` no-op (no duplicate lifecycle
//!     event, `started_at` preserved).
//!   - node `a`'s `graph_step_started` appears EXACTLY ONCE — the resume
//!     started from the checkpoint cursor (`b`), it did NOT cold-restart
//!     and re-run `a`.
//!   - node `b` runs (post-resume step) and the thread reaches
//!     `thread_completed` — the resumed graph drove past the checkpoint
//!     to a clean terminal.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::fast_fixture::{register_standard_bundle, FastFixture};
use common::{build_signed_headers_for_bytes, DaemonHarness};
use lillux::crypto::SigningKey;
use ryeos_runtime::CheckpointWriter;
use serde_json::{json, Value};

/// Plant ZEN_API_KEY in the sealed vault so the graph runtime preflight
/// passes (mirrors `graph_spawn_smoke`).
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

/// Plant a three-node gate chain: `a` → `b` → `c(return)`. Each gate
/// completion writes a checkpoint whose cursor is the NEXT node, so the
/// run produces an intermediate, resumable checkpoint at `b`. No action
/// nodes ⇒ no tool/LLM dependency, fully deterministic.
fn plant_chain_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    // A branch with no `when` is the default/fallback (always taken) —
    // the proven shape from the walker's gate fixtures.
    let body = r#"category: ""
version: "1.0.0"
config:
  start: a
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
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("chain.yaml"), signed)?;
    Ok(())
}

/// Block until exactly one thread directory exists under
/// `<state>/threads/` and return its id (the daemon creates
/// `<state>/threads/<thread_id>/checkpoints` at launch, before the
/// runtime subprocess starts).
async fn await_thread_id(state_path: &Path, deadline: Instant) -> String {
    let threads_root = state_path.join("threads");
    loop {
        if let Ok(entries) = std::fs::read_dir(&threads_root) {
            let dirs: Vec<_> = entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            if dirs.len() == 1 {
                return dirs.into_iter().next().unwrap();
            }
            assert!(
                dirs.len() <= 1,
                "expected at most one graph thread dir, found {dirs:?}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "no graph thread dir appeared under {}",
            threads_root.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Block until the graph has durably written a checkpoint whose cursor is
/// `cursor` (proves node before `cursor` completed and the runtime is now
/// parked at the test hook).
async fn await_checkpoint_cursor(checkpoints_dir: &Path, cursor: &str, deadline: Instant) {
    let reader = CheckpointWriter::new(checkpoints_dir.to_path_buf());
    loop {
        if let Ok(Some(payload)) = reader.load_latest() {
            if payload.get("current_node").and_then(|v| v.as_str()) == Some(cursor) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "checkpoint with cursor `{cursor}` never appeared at {}",
            checkpoints_dir.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Read the recorded process identity for `thread_id` from the runtime DB
/// (recorded by the runtime's `runtime.mark_running` callback, which is
/// its first action — so it is present by the time any checkpoint exists).
fn read_process_identity(
    state_path: &Path,
    thread_id: &str,
    deadline: Instant,
) -> ryeos_app::process::ExecutionProcessIdentity {
    let db_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/runtime.sqlite3");
    loop {
        if let Ok(conn) = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            let identity: Option<String> = conn
                .query_row(
                    "SELECT process_identity FROM thread_runtime WHERE thread_id = ?1",
                    rusqlite::params![thread_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            if let Some(value) = identity {
                if let Ok(identity) = serde_json::from_str(&value) {
                    return identity;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "process identity for {thread_id} never recorded in {}",
            db_path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn projection_events(state_path: &Path, thread_id: &str) -> Vec<(String, Value)> {
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
    let mut stmt = match conn.prepare(
        "SELECT event_type, payload FROM events WHERE thread_id = ?1 ORDER BY chain_seq ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt
        .query_map(rusqlite::params![thread_id], |row| {
            let event_type: String = row.get(0)?;
            let payload_blob: Vec<u8> = row.get(1)?;
            let payload: Value = serde_json::from_slice(&payload_blob).unwrap_or(Value::Null);
            Ok((event_type, payload))
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default();
    rows
}

fn count_event(events: &[(String, Value)], event_type: &str) -> usize {
    events.iter().filter(|(ty, _)| ty == event_type).count()
}

fn count_step_started_for_node(events: &[(String, Value)], node: &str) -> usize {
    events
        .iter()
        .filter(|(ty, payload)| {
            ty == "graph_step_started" && payload.get("node").and_then(|v| v.as_str()) == Some(node)
        })
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_resumes_from_checkpoint_after_daemon_crash() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };

    // The block node is the resume cursor: node `a` completes, the
    // checkpoint records `b` as next, then the runtime parks.
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env("RYEOS_GRAPH_TEST_BLOCK_AFTER_CHECKPOINT", "b");
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeosd=debug,ryeos_graph_runtime=debug".into()),
        );
    })
    .await
    .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_chain_graph(project.path(), &fixture.publisher).expect("plant chain graph");

    // Fire `/execute` as a detached background request: it will hang while
    // the graph is parked, and error out when we SIGKILL the daemon. We
    // build the signed request fully up-front (fresh timestamp/nonce) so no
    // signing key needs to cross the spawn boundary.
    let url = format!("http://{}/execute", h.bind);
    let body = json!({
        "item_ref": "graph:chain",
        "project_path": project.path().to_str().unwrap(),
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
    let exec_task = tokio::spawn(async move {
        let mut req = reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .body(body_bytes);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        // Result is intentionally discarded — the daemon is killed mid-flight.
        let _ = req.send().await;
    });

    let setup_deadline = Instant::now() + Duration::from_secs(30);

    // 1. Discover the graph thread + wait for it to checkpoint at cursor `b`.
    let thread_id = await_thread_id(&h.state_path, setup_deadline).await;
    let checkpoints_dir = h
        .state_path
        .join("threads")
        .join(&thread_id)
        .join("checkpoints");
    await_checkpoint_cursor(&checkpoints_dir, "b", setup_deadline).await;

    // Sanity: pre-crash, node `a` ran exactly once and the thread started once.
    let pre = projection_events(&h.state_path, &thread_id);
    assert_eq!(
        count_step_started_for_node(&pre, "a"),
        1,
        "pre-crash: node `a` should have started exactly once; events={pre:#?}"
    );
    assert_eq!(
        count_event(&pre, "thread_started"),
        1,
        "pre-crash: exactly one thread_started; events={pre:#?}"
    );

    // 2. Record the process identity, then SIGKILL only the daemon (leaving the
    //    row `running` and the exact attached group alive for restart-owned
    //    teardown).
    let process_identity = read_process_identity(&h.state_path, &thread_id, setup_deadline);
    h.kill_daemon().await.expect("kill daemon");
    exec_task.abort();
    assert!(
        ryeos_app::process::execution_group_alive(&process_identity),
        "restart recovery fixture requires the exact orphan group to remain pin-able"
    );

    // 3. Respawn: startup reconcile must terminate the exact orphan and
    // native-resume the crashed graph.
    h.respawn_with(|cmd| {
        // Crucially do NOT set the block hook on respawn — the resumed run
        // also carries RYEOS_RESUME=1 so the hook is inert, but leaving it
        // unset keeps the intent of the resume pass unambiguous.
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeosd=debug,ryeos_graph_runtime=debug".into()),
        );
    })
    .await
    .expect("respawn daemon");

    // 4. Poll for terminal completion of the SAME thread.
    let resume_deadline = Instant::now() + Duration::from_secs(45);
    let final_events = loop {
        let events = projection_events(&h.state_path, &thread_id);
        if count_event(&events, "thread_completed") >= 1 {
            break events;
        }
        if Instant::now() >= resume_deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "graph thread {thread_id} never reached thread_completed after resume.\n\
                 events={events:#?}\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // ── Blocker-2 invariants ────────────────────────────────────────────

    // Idempotent running → running: the resumed runtime's mark_running must
    // NOT append a second thread_started.
    assert_eq!(
        count_event(&final_events, "thread_started"),
        1,
        "thread_started must be emitted exactly once across crash + resume \
         (idempotent running→running); events={final_events:#?}"
    );

    // Resumed from the checkpoint cursor, not a cold restart: node `a`
    // (completed pre-crash) must NOT run again.
    assert_eq!(
        count_step_started_for_node(&final_events, "a"),
        1,
        "node `a` must run exactly once — resume started at cursor `b`, not a \
         cold restart; events={final_events:#?}"
    );

    // Progressed past the checkpoint to completion.
    assert!(
        count_step_started_for_node(&final_events, "b") >= 1,
        "node `b` must run on the resumed pass; events={final_events:#?}"
    );
    assert!(
        count_event(&final_events, "thread_completed") >= 1,
        "thread must reach thread_completed after resume; events={final_events:#?}"
    );
}
