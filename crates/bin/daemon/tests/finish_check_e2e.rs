//! The signed finish check end to end against the scripted chat stub: a reply
//! with no tool call does not end the run while a `finish_check` hook rejects
//! it, the model is shown only the hook's signed text, and a check that cannot
//! be satisfied, cannot run or answers outside its shape ends the run failed
//! with a typed code, never as a success.
//!
//! Every test here shows the mechanism: the runtime sends a different request
//! and ends the run differently. None shows that a real model does better work.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

#[path = "common/stub_rig.rs"]
mod stub_rig;

use std::time::Duration;

use serde_json::Value;
use stub_rig::{RigOptions, chain_event_list, launch_on, start_rig, wait_terminal};

const MAX_WAIT: Duration = Duration::from_secs(120);

fn execution() -> String {
    "category: \"ryeos-runtime\"\nretries: 3\nbackoff_base_ms: 100\nbackoff_max_ms: 1000\n"
        .to_string()
}

/// A `finish_check` hook running `tool`, with one signed text and `limit`
/// rejections allowed in the run.
fn check_header(tool: &str, limit: u32) -> &'static str {
    Box::leak(
        format!(
            "hooks:\n  - id: finish\n    event: finish_check\n    result: verdict\n    steer:\n      limit: {limit}\n      texts:\n        unread: \"SIGNED_WORDS attempt {{attempt}} turn {{turn}} left {{remaining}}\"\n    action:\n      primary: execute\n      item_id: \"tool:{tool}/{tool}\"\n      ref_bindings: {{}}\n      params: {{}}\n"
        )
        .into_boxed_str(),
    )
}

fn options(header: &'static str) -> RigOptions {
    RigOptions {
        header_extra: header,
        exec_yaml: Some(execution()),
        marker_sleep_seconds: 0,
        ..RigOptions::default()
    }
}

async fn run(script: &str, options: RigOptions) -> (stub_rig::Rig, String, String) {
    let rig = start_rig(script, options).await.expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, MAX_WAIT).await;
    (rig, id, status)
}

async fn record(rig: &stub_rig::Rig, thread_id: &str) -> String {
    rig.h
        .get_thread(thread_id)
        .await
        .ok()
        .flatten()
        .map(|record| record.to_string())
        .unwrap_or_default()
}

fn finish_check_inputs(events: &[(String, String, Value)]) -> Vec<&Value> {
    events
        .iter()
        .filter(|(_, kind, payload)| {
            kind == "cognition_in"
                && payload["origin"] == "harness"
                && payload["kind"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("finish_check_turn_"))
        })
        .map(|(_, _, payload)| payload)
        .collect()
}

const DONE_CALL_DONE: &str = "steps:\n  - text: done\n  - tool_calls:\n      - name: noop\n        arguments: {}\n  - text: done\n";

#[tokio::test(flavor = "multi_thread")]
async fn without_a_check_a_plain_done_ends_the_run_at_turn_one() {
    let (rig, _, status) = run(DONE_CALL_DONE, options("")).await;
    assert_eq!(status, "completed");
    assert_eq!(rig.stub.requests().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_finish_takes_another_turn_with_only_the_signed_words() {
    let (rig, id, status) = run(DONE_CALL_DONE, options(check_header("verdict_once", 2))).await;
    assert_eq!(status, "completed");
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 3, "the rejected finish, the call, the finish");
    let second = bodies[1]["messages"].to_string();
    assert!(
        second.contains("SIGNED_WORDS attempt 1 turn 1 left 1"),
        "{second}"
    );
    let events = chain_event_list(&rig.h, &id).await;
    let inputs = finish_check_inputs(&events);
    assert_eq!(inputs.len(), 1, "{inputs:?}");
    assert_eq!(inputs[0]["kind"], "finish_check_turn_1_attempt_1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_that_never_passes_ends_the_run_failed_with_its_code() {
    let script = "steps:\n  - repeat: 6\n    text: done\n";
    let (rig, id, status) = run(script, options(check_header("verdict_never", 2))).await;
    assert_eq!(status, "failed", "never a success");
    assert_eq!(rig.stub.requests().await.len(), 3);
    let record = record(&rig, &id).await;
    assert!(
        record.contains("finish_check_retries_exhausted"),
        "{record}"
    );
    let events = chain_event_list(&rig.h, &id).await;
    assert_eq!(finish_check_inputs(&events).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_result_carrying_free_text_fails_the_run_and_the_text_never_reaches_the_model() {
    let script = "steps:\n  - repeat: 3\n    text: done\n";
    let (rig, id, status) = run(script, options(check_header("verdict_free", 2))).await;
    assert_eq!(status, "failed");
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert!(!bodies[0].to_string().contains("FREE_TEXT_FROM_A_TOOL"));
    let record = record(&rig, &id).await;
    assert!(record.contains("finish_check_invalid"), "{record}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_the_hook_did_not_sign_fails_the_run() {
    let script = "steps:\n  - repeat: 3\n    text: done\n";
    let (rig, id, status) = run(script, options(check_header("verdict_unknown", 2))).await;
    assert_eq!(status, "failed");
    assert_eq!(rig.stub.requests().await.len(), 1);
    assert!(record(&rig, &id).await.contains("finish_check_invalid"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_that_cannot_run_fails_the_run_closed() {
    let script = "steps:\n  - repeat: 3\n    text: done\n";
    let (rig, id, status) = run(script, options(check_header("verdict_broken", 2))).await;
    assert_eq!(status, "failed");
    assert_eq!(rig.stub.requests().await.len(), 1);
    assert!(record(&rig, &id).await.contains("finish_check_failed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejection_with_no_turn_left_ends_with_the_turn_limits_own_code() {
    let script = "steps:\n  - repeat: 3\n    text: done\n";
    let mut opts = options(check_header("verdict_never", 2));
    opts.limits = Some((1, 5));
    let (rig, id, status) = run(script, opts).await;
    assert_eq!(status, "failed");
    assert_eq!(
        rig.stub.requests().await.len(),
        1,
        "a rejection adds no turn"
    );
    let record = record(&rig, &id).await;
    assert!(record.contains("turn_limit_exceeded"), "{record}");
    assert!(
        !record.contains("finish_check_retries_exhausted"),
        "the limit ended the run first: {record}"
    );
}
