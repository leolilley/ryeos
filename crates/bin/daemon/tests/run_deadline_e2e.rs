//! The run deadline, end to end against the scripted chat stub.
//!
//! A root launch sets `run_timeout`; the directive runtime derives its wrap-up
//! notice and its hard stop from it. These tests drive that with a stub whose
//! turns are slow enough to cross each boundary in real time, so each takes
//! about a minute. They need populated bundles (`-f bundles=branch`).

use ryeosd::test_support as common;

#[path = "common/stub_rig.rs"]
mod stub_rig;
use stub_rig::*;

use std::time::{Duration, Instant};

use common::DaemonHarness;
use serde_json::{Value, json};

/// A reserve of 5 s and a wrap-up window of 20 s for the probe directive.
const WRAP_UP_HEADER: &str =
    "limits:\n  wrap_up_seconds: 20\n  deadline:\n    reserve_seconds: 5\n";
/// A reserve of 5 s and no wrap-up notice.
const NO_WRAP_UP_HEADER: &str =
    "limits:\n  wrap_up_seconds: 0\n  deadline:\n    reserve_seconds: 5\n";

async fn thread_ids_of_chain(h: &DaemonHarness, root: &str) -> Vec<(String, String)> {
    h.list_threads(2000)
        .await
        .map(|listed| {
            listed
                .into_iter()
                .filter(|t| t.chain_root_id == root || t.thread_id == root)
                .map(|t| (t.thread_id, t.status))
                .collect()
        })
        .unwrap_or_default()
}

async fn wrap_up_inputs(rig: &Rig, root: &str) -> Vec<Value> {
    chain_payloads(&rig.h, root, "cognition_in")
        .await
        .into_iter()
        .filter(|payload| payload.get("kind").and_then(Value::as_str) == Some("deadline_wrap_up"))
        .collect()
}

fn request_text(body: &Value) -> String {
    body.get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .map(|m| m.get("content").map(Value::to_string).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Slow tool turns carry the run across the wrap-up boundary and then the hard
/// stop. The model is told once, and the run settles as a deadline stop before
/// the daemon's own kill would have fired.
#[tokio::test(flavor = "multi_thread")]
async fn wrap_up_notice_then_hard_stop() {
    let script = "steps:\n  - repeat: 300\n    delay_ms: 1000\n    tool_calls:\n      - { id: \"call_{n}\", name: noop_noop, arguments: {} }\n";
    let rig = start_rig(
        script,
        RigOptions {
            header_extra: WRAP_UP_HEADER,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let launched = Instant::now();
    let id = launch_on_with(&rig, json!({"run_timeout": 60}))
        .await
        .expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    let elapsed = launched.elapsed();
    let detail = thread_detail(&rig.h, &id, 4000).await;
    assert_eq!(status, "timed_out", "detail: {detail}");
    // The hard stop is at 60 - 5 = 55 s; the daemon kill would be at 60 s.
    assert!(
        elapsed >= Duration::from_secs(45) && elapsed <= Duration::from_secs(75),
        "stopped after {elapsed:?}"
    );
    assert!(
        detail.contains("hard_stop"),
        "the stop is the runtime's structured deadline result: {detail}"
    );

    let notices = wrap_up_inputs(&rig, &id).await;
    assert_eq!(notices.len(), 1, "exactly one durable wrap-up input");
    assert_eq!(notices[0]["origin"], "harness");

    let requests = rig.stub.requests().await;
    assert!(requests.len() > 10, "{} requests", requests.len());
    let with_notice: Vec<usize> = requests
        .iter()
        .enumerate()
        .filter(|(_, r)| request_text(&r.body).contains("Deadline notice"))
        .map(|(i, _)| i)
        .collect();
    assert!(
        !with_notice.is_empty(),
        "a later request carries the notice"
    );
    assert!(
        with_notice.first().copied().unwrap_or(0) > 5,
        "the first turns carry no notice"
    );
    // Once delivered, every later request keeps it (it is pinned in the
    // transcript) and it is never repeated.
    for window in with_notice.windows(2) {
        assert_eq!(window[1], window[0] + 1, "contiguous from first delivery");
    }
    let repeated = requests
        .iter()
        .map(|r| request_text(&r.body).matches("Deadline notice").count())
        .max()
        .unwrap_or(0);
    assert_eq!(repeated, 1, "the model is told once");
}

/// A model call that never produces output is cut at the hard stop rather than
/// waiting out the stall.
#[tokio::test(flavor = "multi_thread")]
async fn hard_stop_cuts_a_stalled_model_call() {
    let script = "steps:\n  - text: never\n    stall: { after_chunks: 0, ms: 120000 }\n";
    let rig = start_rig(
        script,
        RigOptions {
            header_extra: NO_WRAP_UP_HEADER,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let launched = Instant::now();
    let id = launch_on_with(&rig, json!({"run_timeout": 30}))
        .await
        .expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(100)).await;
    let elapsed = launched.elapsed();
    let detail = thread_detail(&rig.h, &id, 4000).await;
    assert_eq!(status, "timed_out", "detail: {detail}");
    assert!(
        elapsed <= Duration::from_secs(45),
        "stopped after {elapsed:?}, long before the 120 s stall"
    );
    assert!(wrap_up_inputs(&rig, &id).await.is_empty());
}

/// A tool that outlives the run is bounded by the daemon's end of the run: the
/// thread settles `timed_out` and nothing waits for the tool's own 300 s.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_tool_does_not_outlive_the_run() {
    let script = "steps:\n  - tool_calls:\n      - { id: \"call_{n}\", name: marker_marker, arguments: {} }\n  - text: done\n";
    let rig = start_rig(
        script,
        RigOptions {
            header_extra: NO_WRAP_UP_HEADER,
            marker_sleep_seconds: 300,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let launched = Instant::now();
    let id = launch_on_with(&rig, json!({"run_timeout": 30}))
        .await
        .expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(100)).await;
    let elapsed = launched.elapsed();
    let detail = thread_detail(&rig.h, &id, 4000).await;
    assert_eq!(status, "timed_out", "detail: {detail}");
    assert!(
        elapsed <= Duration::from_secs(50),
        "settled after {elapsed:?}"
    );
    assert_eq!(rig.marker_count(), 1, "the tool ran once");
}

/// Segments of one run share one deadline: a context-window continuation does
/// not restart the clock, and the last segment settles as a deadline stop.
#[tokio::test(flavor = "multi_thread")]
async fn continuations_share_one_deadline() {
    // Each reply carries about 250 tokens of its own text, so the live context
    // crosses the 2,000-token window's continuation threshold every few turns.
    // (Tool results are de-duplicated by the result guard and would not grow.)
    let padding = "padding ".repeat(125);
    let script = format!(
        "steps:\n  - repeat: 200\n    delay_ms: 2000\n    text: \"{padding}\"\n    tool_calls:\n      - {{ id: \"call_{{n}}\", name: noop_noop, arguments: {{}} }}\n"
    );
    let rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            header_extra:
                "continuation:\n  carry_turns: 2\nlimits:\n  wrap_up_seconds: 0\n  deadline:\n    reserve_seconds: 5\n",
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let launched = Instant::now();
    let id = launch_on_with(&rig, json!({"run_timeout": 60}))
        .await
        .expect("launch");
    let mut chain;
    loop {
        chain = thread_ids_of_chain(&rig.h, &id).await;
        let all_terminal =
            !chain.is_empty() && chain.iter().all(|(_, s)| TERMINAL.contains(&s.as_str()));
        if (all_terminal && launched.elapsed() > Duration::from_secs(5))
            || launched.elapsed() > Duration::from_secs(120)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let elapsed = launched.elapsed();
    assert!(chain.len() >= 2, "a continuation happened: {chain:?}");
    let statuses: Vec<&str> = chain.iter().map(|(_, s)| s.as_str()).collect();
    let timed_out = statuses.iter().filter(|s| **s == "timed_out").count();
    assert_eq!(
        timed_out, 1,
        "one deadline stop ends the chain: {statuses:?}"
    );
    assert!(
        elapsed <= Duration::from_secs(80),
        "the chain ended after {elapsed:?}, within the one deadline"
    );
}

/// `time_left` is offered to a run with a deadline and answers from the clock.
#[tokio::test(flavor = "multi_thread")]
async fn time_left_reports_the_clock() {
    let script = "steps:\n  - tool_calls:\n      - { id: \"call_a\", name: time_left, arguments: {} }\n  - text: done\n";
    let rig = start_rig(
        script,
        RigOptions {
            header_extra: NO_WRAP_UP_HEADER,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on_with(&rig, json!({"run_timeout": 300}))
        .await
        .expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(60)).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &id, 2000).await
    );

    let requests = rig.stub.requests().await;
    assert_eq!(requests.len(), 2);
    let offered: Vec<String> = requests[0]
        .body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    assert!(offered.iter().any(|n| n == "time_left"), "{offered:?}");
    let answer = request_text(&requests[1].body);
    assert!(answer.contains("remaining_ms"), "{answer}");
    assert!(answer.contains("running"), "{answer}");
}

/// A run without a deadline is not offered `time_left` and is not touched.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_without_a_deadline_is_unchanged() {
    let script = "steps:\n  - tool_calls:\n      - { id: \"call_a\", name: noop_noop, arguments: {} }\n  - text: done\n";
    let rig = start_rig(script, RigOptions::default())
        .await
        .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(60)).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &id, 2000).await
    );
    let requests = rig.stub.requests().await;
    let first_tools = requests[0]
        .body
        .get("tools")
        .map(Value::to_string)
        .unwrap_or_default();
    assert!(!first_tools.contains("time_left"), "{first_tools}");
    assert!(wrap_up_inputs(&rig, &id).await.is_empty());
}

/// A graph that follows a directive child which never finishes. The child ends
/// at the graph's reserve window, the graph reads the deadline outcome, runs
/// its fallback node inside its reserve, and completes before the run ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_followed_child_hits_the_deadline_and_the_graph_falls_back() {
    // The child's model call stalls far past the run.
    let script = "steps:\n  - text: never\n    stall: { after_chunks: 0, ms: 600000 }\n";
    let rig = start_rig(
        script,
        RigOptions {
            header_extra: NO_WRAP_UP_HEADER,
            graph_vault: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    plant_graph(
        &rig,
        "guard",
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.probe/run
      # A follow child cannot hold authority its parent lacks, and the probe
      # directive declares the tool wildcard.
      - "ryeos.execute.tool.*"
limits:
  deadline:
    reserve_seconds: 20
config:
  start: work
  nodes:
    work:
      node_type: action
      follow: true
      action:
        item_id: "directive:probe/run"
        ref_bindings: { model: "directive:probe/run" }
        params: { name: probe }
      on_error: fallback
      next: { type: unconditional, to: done }
    fallback:
      node_type: return
      output:
        route: "fallback"
        outcome_kind: "${state._outcome.outcome_kind}"
    done:
      node_type: return
      output:
        route: "done"
"#,
    )
    .expect("plant graph");
    let launched = Instant::now();
    let id = launch_graph_on(&rig, "graph:guard", json!({"run_timeout": 90}))
        .await
        .expect("launch graph");
    // The graph suspends at the follow, then its resume successor completes.
    let mut chain;
    loop {
        chain = thread_ids_of_chain(&rig.h, &id).await;
        let finished = chain.iter().any(|(_, s)| s == "completed")
            && chain.iter().all(|(_, s)| TERMINAL.contains(&s.as_str()));
        if finished || launched.elapsed() > Duration::from_secs(120) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let elapsed = launched.elapsed();
    if !chain.iter().any(|(_, s)| s == "completed") {
        let mut details = String::new();
        for (thread, _) in &chain {
            details.push_str(&thread_detail(&rig.h, thread, 3000).await);
            details.push('\n');
        }
        panic!(
            "the graph did not complete through its fallback: {chain:?}\n{details}\nstderr: {}",
            stderr_hits(&rig.h, &["graph", "error", "refus", "deadline"], 40)
        );
    }
    // The run deadline is 90 s; the fallback finishes inside it.
    assert!(
        elapsed < Duration::from_secs(90),
        "completed after {elapsed:?}, before the run deadline"
    );
    // The child stopped at the graph's reserve window less its own reserve:
    // 90 - 20 - 5 = 65 s, well before the 90 s deadline.
    assert!(
        elapsed > Duration::from_secs(55),
        "the child was stopped by the deadline, not early: {elapsed:?}"
    );
    let mut result_text = String::new();
    for (thread, status) in &chain {
        if status == "completed" {
            result_text.push_str(&thread_detail(&rig.h, thread, 6000).await);
        }
    }
    assert!(
        result_text.contains("fallback") && result_text.contains("deadline"),
        "the fallback read outcome_kind deadline: {result_text}"
    );
}

/// A child launch that carries `run_timeout` is refused: only a root sets the
/// run deadline.
#[tokio::test(flavor = "multi_thread")]
async fn a_child_cannot_set_its_own_run_deadline() {
    let script = "steps:\n  - text: done\n";
    let rig = start_rig(
        script,
        RigOptions {
            graph_vault: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    plant_graph(
        &rig,
        "asker",
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.probe/run
      # A follow child cannot hold authority its parent lacks, and the probe
      # directive declares the tool wildcard.
      - "ryeos.execute.tool.*"
config:
  start: call
  on_error: continue
  nodes:
    call:
      node_type: action
      follow: true
      action:
        item_id: "directive:probe/run"
        ref_bindings: { model: "directive:probe/run" }
        params: { name: probe, run_timeout: 30 }
      next: { type: unconditional, to: done }
    done:
      node_type: return
      output:
        ok: "yes"
"#,
    )
    .expect("plant graph");
    let id = launch_graph_on(&rig, "graph:asker", json!({}))
        .await
        .expect("launch graph");
    // The followed child is its own chain; wait for every thread to settle.
    let started = Instant::now();
    let mut seen = String::new();
    loop {
        let listed = rig.h.list_threads(2000).await.unwrap_or_default();
        let settled = !listed.is_empty()
            && listed
                .iter()
                .all(|t| TERMINAL.contains(&t.status.as_str()) && t.status != "continued");
        if settled || started.elapsed() > Duration::from_secs(90) {
            for t in &listed {
                seen.push_str(&format!("\n[{} {}] ", t.thread_id, t.status));
                seen.push_str(&thread_detail(&rig.h, &t.thread_id, 3000).await);
                for (thread_id, kind, payload) in chain_event_list(&rig.h, &t.chain_root_id).await {
                    seen.push_str(&format!("\n{thread_id} {kind} {payload}"));
                }
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = id;
    // The detached child's launch refusal is logged by the daemon; the thread
    // records may carry it as well.
    let stderr = rig.h.stderr_so_far();
    let refused = |text: &str| {
        text.contains("run_timeout_root_only") || text.contains("accepted only on the root launch")
    };
    assert!(
        refused(&seen) || refused(&stderr),
        "the refusal names the root-only rule: {}\nstderr hits: {}",
        trunc(&seen, 600),
        stderr_hits(&rig.h, &["detached launch failed", "run_timeout"], 6)
    );
    assert!(
        rig.stub.requests().await.is_empty(),
        "the refused child never reached the model"
    );
}

fn accounting_deadlines(h: &DaemonHarness) -> Vec<i64> {
    let db = h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(ryeos_app::accounting_db::ACCOUNTING_DB_FILENAME);
    ryeos_app::test_support::read_existing_accounting(&db, |connection| {
        let mut statement =
            connection.prepare("SELECT deadline_at_ms FROM execution_resource_budget")?;
        let rows = statement
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// A node restart mid-run keeps the same budget row and deadline, resumes the
/// thread to its hard stop, and does not tell the model twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_mid_run_keeps_the_deadline_and_the_single_wrap_up() {
    let script = "steps:\n  - repeat: 300\n    delay_ms: 1000\n    tool_calls:\n      - { id: \"call_{n}\", name: noop_noop, arguments: {} }\n";
    let mut rig = start_rig(
        script,
        RigOptions {
            header_extra: WRAP_UP_HEADER,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let launched = Instant::now();
    let id = launch_on_with(&rig, json!({"run_timeout": 90}))
        .await
        .expect("launch");
    // The wrap-up window opens at 90 - 5 - 20 = 65 s.
    let wait_until = Instant::now() + Duration::from_secs(100);
    while wrap_up_inputs(&rig, &id).await.is_empty() {
        assert!(Instant::now() < wait_until, "no wrap-up notice arrived");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    rig.h.kill_daemon().await.expect("kill the node mid-run");
    let before = accounting_deadlines(&rig.h);
    assert_eq!(before.len(), 1, "one execution budget row: {before:?}");
    rig.h.respawn_with(|_| {}).await.expect("restart the node");

    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    let elapsed = launched.elapsed();
    let detail = thread_detail(&rig.h, &id, 4000).await;
    assert_eq!(status, "timed_out", "detail: {detail}");
    assert!(
        elapsed >= Duration::from_secs(75) && elapsed <= Duration::from_secs(110),
        "the resumed run stopped at the original hard stop (85 s): {elapsed:?}"
    );
    assert_eq!(
        wrap_up_inputs(&rig, &id).await.len(),
        1,
        "the wrap-up was not repeated after the restart"
    );
    rig.h.kill_daemon().await.expect("stop the node");
    assert_eq!(
        accounting_deadlines(&rig.h),
        before,
        "the restart neither minted nor moved the deadline"
    );
}

/// A node that was down when the run deadline passed settles the thread as a
/// deadline stop at recovery and spawns nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_after_the_deadline_settles_without_spawning() {
    let script = "steps:\n  - repeat: 300\n    delay_ms: 1000\n    tool_calls:\n      - { id: \"call_{n}\", name: noop_noop, arguments: {} }\n";
    let mut rig = start_rig(
        script,
        RigOptions {
            header_extra: NO_WRAP_UP_HEADER,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let launched = Instant::now();
    let id = launch_on_with(&rig, json!({"run_timeout": 40}))
        .await
        .expect("launch");
    tokio::time::sleep(Duration::from_secs(8)).await;
    rig.h.kill_daemon().await.expect("kill the node mid-run");
    // Stay down until the deadline (and the orphaned runtime's own stop) pass.
    let past = Duration::from_secs(50);
    if let Some(remaining) = past.checked_sub(launched.elapsed()) {
        tokio::time::sleep(remaining).await;
    }
    let requests_while_down = rig.stub.requests().await.len();
    rig.h.respawn_with(|_| {}).await.expect("restart the node");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(60)).await;
    let detail = thread_detail(&rig.h, &id, 4000).await;
    assert_eq!(status, "timed_out", "detail: {detail}");
    assert!(
        detail.contains("deadline_elapsed_while_down"),
        "settled by recovery as an elapsed deadline: {detail}"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        rig.stub.requests().await.len(),
        requests_while_down,
        "no model call after the restart"
    );
}
