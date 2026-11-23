//! The observing hook events (`before_tool`, `after_tool`, `turn_incomplete`,
//! `idle`) end to end against the scripted chat stub. Each hook's action runs
//! the marker tool, so the marker count is the number of dispatches.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

#[path = "common/stub_rig.rs"]
mod stub_rig;

use std::time::Duration;

use stub_rig::{RigOptions, launch_on, start_rig, wait_terminal};

const MAX_WAIT: Duration = Duration::from_secs(120);

fn hook(id: &str, event: &str) -> String {
    format!(
        "  - id: {id}\n    event: {event}\n    result: observation\n    action:\n      primary: execute\n      item_id: \"tool:marker/marker\"\n      ref_bindings: {{}}\n      params: {{}}\n"
    )
}

/// A directive header declaring a marker-running hook for each event.
fn hooks(events: &[&str]) -> &'static str {
    let body: String = events
        .iter()
        .map(|event| hook(&format!("on-{event}"), event))
        .collect();
    Box::leak(format!("hooks:\n{body}").into_boxed_str())
}

fn execution(extra: &str) -> String {
    format!(
        "category: \"ryeos-runtime\"\nretries: 3\nbackoff_base_ms: 100\nbackoff_max_ms: 1000\n{extra}"
    )
}

fn options(header: &'static str) -> RigOptions {
    RigOptions {
        header_extra: header,
        exec_yaml: Some(execution("")),
        marker_sleep_seconds: 0,
        ..RigOptions::default()
    }
}

async fn run(script: &str, options: RigOptions) -> (stub_rig::Rig, String) {
    let rig = start_rig(script, options).await.expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, MAX_WAIT).await;
    (rig, status)
}

const TWO_CALLS: &str = "steps:\n  - tool_calls:\n      - name: noop\n        arguments: {}\n      - name: noop\n        arguments: {}\n  - text: done\n";

#[tokio::test(flavor = "multi_thread")]
async fn every_dispatched_call_fires_before_and_after() {
    let (rig, status) = run(TWO_CALLS, options(hooks(&["before_tool", "after_tool"]))).await;
    assert_eq!(status, "completed");
    assert_eq!(
        rig.marker_count(),
        4,
        "two calls, a before and an after each"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hooks_own_tool_call_does_not_fire_the_runs_tool_hooks() {
    // The model calls the marker tool once; each of its two hooks runs the
    // marker tool too. Were a hook's call to re-enter the run's hooks the
    // count would grow past three.
    let script =
        "steps:\n  - tool_calls:\n      - name: marker\n        arguments: {}\n  - text: done\n";
    let (rig, status) = run(script, options(hooks(&["before_tool", "after_tool"]))).await;
    assert_eq!(status, "completed");
    assert_eq!(rig.marker_count(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_refused_after_before_tool_still_gets_after_tool() {
    // One tool call is allowed; the second call of the batch is refused.
    let mut opts = options(hooks(&["before_tool", "after_tool"]));
    opts.limits = Some((10, 1));
    let (rig, status) = run(TWO_CALLS, opts).await;
    assert_eq!(status, "completed");
    assert_eq!(rig.marker_count(), 4, "the refused call is paired too");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_s_observation_dispatches_are_capped() {
    let mut opts = options(hooks(&["before_tool"]));
    opts.exec_yaml = Some(execution("observing_hooks_per_turn: 1\n"));
    let (rig, status) = run(TWO_CALLS, opts).await;
    assert_eq!(status, "completed");
    assert_eq!(
        rig.marker_count(),
        1,
        "the second call's observation is skipped"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_reply_fires_turn_incomplete_with_or_without_a_policy() {
    let script = "steps:\n  - reasoning: \"EMPTY_REPLY_REASONING\"\n  - text: \"the answer\"\n";
    let (rig, status) = run(script, options(hooks(&["turn_incomplete"]))).await;
    assert_eq!(status, "completed");
    assert_eq!(rig.marker_count(), 1, "retried, no policy");

    let mut opts = options(hooks(&["turn_incomplete"]));
    opts.provider_top_extra =
        "empty_replies:\n  max_consecutive: 2\n  message: \"Call a tool or give your answer.\"\n"
            .to_string();
    let (rig, status) = run(script, opts).await;
    assert_eq!(status, "completed");
    assert_eq!(rig.marker_count(), 1, "corrected");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_idle_finding_fires_idle_when_warned_and_when_stopped() {
    let script = "steps:\n  - repeat: 12\n    reasoning: \"EMPTY_REPLY_REASONING\"\n";
    let (rig, status) = run(script, options(hooks(&["idle"]))).await;
    assert_eq!(status, "failed");
    assert_eq!(rig.marker_count(), 2, "one warning, one stop");
}
