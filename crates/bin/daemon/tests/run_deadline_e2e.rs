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
