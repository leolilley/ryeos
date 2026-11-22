//! A directive surviving a bad provider turn, end to end against the scripted
//! chat stub: unknown-outcome retries (a stream cut, a request timeout), a
//! directive that does not declare repeatable contact, an exhausted retry
//! allowance, a 504, a malformed tool call, and a turn cut at the output limit.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

#[path = "common/stub_rig.rs"]
mod stub_rig;

use std::time::Duration;

use serde_json::Value;
use stub_rig::{RigOptions, chain_event_list, chain_payloads, launch_on, start_rig, wait_terminal};

const MAX_WAIT: Duration = Duration::from_secs(120);

/// One abandoned attempt's reserved maximum on the paid stub route at the
/// default context window: 0.4 + 0.32768 USD.
const RESERVED_MAX_NANOS: i64 = 727_680_000;

/// Hard spend limit large enough for four reservations.
const FINITE_BUDGET: &str = "limits:\n  spend_usd: \"3\"\n";

fn repeatable_header() -> &'static str {
    "provider_contact: repeatable\nlimits:\n  spend_usd: \"3\"\n"
}

fn once_header() -> &'static str {
    FINITE_BUDGET
}

fn execution(extra: &str) -> String {
    format!(
        "category: \"ryeos-runtime\"\nretries: 3\nbackoff_base_ms: 100\nbackoff_max_ms: 1000\n{extra}"
    )
}

fn unknown_policy(max_retries: u32, on: &str) -> String {
    format!("unknown_outcome_retry:\n  max_retries: {max_retries}\n  on: [{on}]\n")
}

/// `(attempt_number, state, reserved_nanos, charge_nanos)` of every provider
/// attempt in the node's accounting ledger, in attempt order.
fn ledger_rows(h: &ryeosd::test_support::DaemonHarness) -> Vec<(i64, String, i64, Option<i64>)> {
    let db = h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(ryeos_app::accounting_db::ACCOUNTING_DB_FILENAME);
    ryeos_app::test_support::read_existing_accounting(&db, |connection| {
        let mut statement = connection.prepare(
            "SELECT attempt_number, state, reserved_usd_nanos, budget_charge_usd_nanos \
             FROM provider_attempt_reservation ORDER BY turn, attempt_number",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

async fn run_to_terminal(rig: &stub_rig::Rig) -> (String, String) {
    let thread_id = launch_on(rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    (thread_id, status)
}

/// The retry and its abandoned attempt, for a run that cut once and then
/// completed.
async fn assert_one_retry_after(thread_id: &str, rig: &mut stub_rig::Rig, kind: &str) {
    let retries = chain_payloads(&rig.h, thread_id, "provider_retry").await;
    assert_eq!(retries.len(), 1, "one provider_retry expected: {retries:?}");
    let retry = &retries[0];
    assert_eq!(retry["attempt"], 1, "{retry}");
    assert_eq!(
        retry["retry_advance"]["decision"]["reason"]["kind"], kind,
        "{retry}"
    );
    let supersedes = &retry["supersedes"];
    assert_eq!(supersedes["turn"], 1, "{retry}");
    assert_eq!(supersedes["attempt"], 1, "{retry}");
    if kind == "mid_stream" {
        assert!(
            supersedes["output_events"].as_u64().unwrap_or(0) > 0,
            "a cut after live text supersedes that output: {retry}"
        );
    } else {
        assert_eq!(supersedes["output_events"], 0, "{retry}");
    }
    assert_eq!(rig.stub.requests().await.len(), 2);

    rig.h.stop_gracefully().await.expect("stop node");
    let rows = ledger_rows(&rig.h);
    assert_eq!(rows.len(), 2, "two attempts expected: {rows:?}");
    assert_eq!(rows[0].1, "charged_reserved_maximum", "{rows:?}");
    assert_eq!(rows[0].3, Some(RESERVED_MAX_NANOS), "{rows:?}");
    assert_eq!(rows[1].1, "reconciled", "{rows:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mid_stream_cut_is_retried_and_the_abandoned_attempt_is_charged() {
    let script = "steps:\n  - text: \"a long answer that is cut off part way\"\n    stream: { text_chunk_bytes: 4 }\n    cut_after_chunks: 5\n  - text: \"done\"\n";
    let mut rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution(&unknown_policy(2, "mid_stream, timeout"))),
            header_extra: repeatable_header(),
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "completed");
    assert_one_retry_after(&thread_id, &mut rig, "mid_stream").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_timeout_is_retried_and_the_abandoned_attempt_is_charged() {
    let script = "steps:\n  - delay_ms: 6000\n    text: \"too late\"\n  - text: \"done\"\n";
    let mut rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution(&format!(
                "timeout_seconds: 2\n{}",
                unknown_policy(2, "mid_stream, timeout")
            ))),
            header_extra: repeatable_header(),
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "completed");
    assert_one_retry_after(&thread_id, &mut rig, "timeout").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directive_that_does_not_declare_repeatable_contact_is_not_retried() {
    let script = "steps:\n  - text: \"a long answer that is cut off part way\"\n    stream: { text_chunk_bytes: 4 }\n    cut_after_chunks: 5\n  - text: \"done\"\n";
    let rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution(&unknown_policy(2, "mid_stream, timeout"))),
            header_extra: once_header(),
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "failed");
    assert_eq!(rig.stub.requests().await.len(), 1);
    assert!(
        chain_payloads(&rig.h, &thread_id, "provider_retry")
            .await
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_after_unknown_outcomes_stop_at_max_retries() {
    let script = "steps:\n  - repeat: 2\n    text: \"a long answer that is cut off part way\"\n    stream: { text_chunk_bytes: 4 }\n    cut_after_chunks: 5\n  - text: \"never served\"\n";
    let rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution(&unknown_policy(1, "mid_stream"))),
            header_extra: repeatable_header(),
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "failed");
    assert_eq!(rig.stub.requests().await.len(), 2);
    assert_eq!(
        chain_payloads(&rig.h, &thread_id, "provider_retry")
            .await
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_504_is_retried_by_default() {
    let script =
        "steps:\n  - fail: { status: 504, body: \"gateway timeout\" }\n  - text: \"done\"\n";
    let rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution("")),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "completed");
    assert_eq!(rig.stub.requests().await.len(), 2);
    let retries = chain_payloads(&rig.h, &thread_id, "provider_retry").await;
    assert_eq!(retries.len(), 1, "{retries:?}");
    assert_eq!(
        retries[0]["retry_advance"]["decision"]["reason"]["code"],
        504
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_with_malformed_arguments_is_not_dispatched() {
    let script = "steps:\n  - tool_calls:\n      - { id: \"call_bad\", name: marker_marker, arguments: \"{not json\" }\n  - text: \"done\"\n";
    let rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution("")),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "completed");
    assert_eq!(rig.marker_count(), 0, "the tool must not run");

    let events = chain_event_list(&rig.h, &thread_id).await;
    let turn = events
        .iter()
        .find(|(_, kind, payload)| kind == "cognition_out" && payload.get("parts").is_some())
        .map(|(_, _, payload)| payload.clone())
        .expect("a recorded turn");
    assert_eq!(turn["malformed_calls"][0]["call_index"], 0, "{turn}");

    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    let messages = bodies[1]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|message| message["tool_call_id"] == "call_bad")
        .expect("an error result under the same call id");
    assert!(
        result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("not valid JSON"),
        "{result}"
    );
}

fn output_limit_policy(max_consecutive: u32) -> String {
    format!(
        "on_output_limit:\n  max_consecutive: {max_consecutive}\n  message: \"Continue from where it stopped.\"\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_cut_at_the_output_limit_is_recovered() {
    let script =
        "steps:\n  - text: \"first half\"\n    finish_reason: length\n  - text: \"second half\"\n";
    let rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution(&output_limit_policy(2))),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "completed");
    assert_eq!(rig.stub.requests().await.len(), 2);

    let events = chain_event_list(&rig.h, &thread_id).await;
    let finals: Vec<&Value> = events
        .iter()
        .filter(|(_, kind, payload)| kind == "cognition_out" && payload.get("finish").is_some())
        .map(|(_, _, payload)| payload)
        .collect();
    assert_eq!(finals[0]["finish"], "length", "{finals:?}");
    let harness_input = events
        .iter()
        .find(|(_, kind, payload)| {
            kind == "cognition_in"
                && payload["origin"] == "harness"
                && payload["kind"] == "output_limit"
        })
        .expect("a harness input after the cut turn");
    assert!(
        harness_input
            .2
            .to_string()
            .contains("Continue from where it stopped.")
    );

    let bodies = rig.stub.request_bodies().await;
    let last_user = bodies[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .expect("a user message");
    assert!(
        last_user
            .to_string()
            .contains("Continue from where it stopped."),
        "{last_user}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn consecutive_output_limit_turns_are_bounded() {
    let script = "steps:\n  - repeat: 5\n    text: \"cut again\"\n    finish_reason: length\n";
    let rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution(&output_limit_policy(2))),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (_, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "failed");
    // Two recovered turns, then the third ends the thread.
    assert_eq!(rig.stub.requests().await.len(), 3);
}

fn retry_after_script(header: &str) -> String {
    format!(
        "steps:\n  - fail: {{ status: 429, body: \"slow down\", retry_after: \"{header}\" }}\n  - text: \"done\"\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_longer_than_the_backoff_is_waited_out_and_recorded() {
    let rig = start_rig(
        &retry_after_script("3"),
        RigOptions {
            exec_yaml: Some(execution("")),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let thread_id = launch_on(&rig).await.expect("launch");
    let (status, elapsed) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    assert_eq!(status, "completed");
    assert!(
        elapsed >= Duration::from_secs(3),
        "the retry came before the provider's Retry-After: {elapsed:?}"
    );
    assert_eq!(rig.stub.requests().await.len(), 2);
    let retries = chain_payloads(&rig.h, &thread_id, "provider_retry").await;
    assert_eq!(retries.len(), 1, "{retries:?}");
    let decision = &retries[0]["retry_advance"]["decision"];
    assert_eq!(decision["backoff_ms"], 3000, "{decision}");
    assert_eq!(decision["retry_after_ms"], 3000, "{decision}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_over_the_cap_is_refused_with_its_typed_reason() {
    let rig = start_rig(
        &retry_after_script("120"),
        RigOptions {
            exec_yaml: Some(execution("retry_after_max_ms: 5000\n")),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "failed");
    assert_eq!(rig.stub.requests().await.len(), 1);
    assert!(
        chain_payloads(&rig.h, &thread_id, "provider_retry")
            .await
            .is_empty()
    );
    let detail = stub_rig::thread_detail(&rig.h, &thread_id, 4000).await;
    assert!(
        detail.contains("provider_retry_after_exceeds_cap"),
        "{detail}"
    );
    assert!(detail.contains("120000"), "{detail}");
    assert!(detail.contains("5000"), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_cap_ignores_the_retry_after_header() {
    let rig = start_rig(
        &retry_after_script("120"),
        RigOptions {
            exec_yaml: Some(execution("retry_after_max_ms: 0\n")),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (thread_id, status) = run_to_terminal(&rig).await;
    assert_eq!(status, "completed");
    let retries = chain_payloads(&rig.h, &thread_id, "provider_retry").await;
    let decision = &retries[0]["retry_advance"]["decision"];
    assert_eq!(decision["backoff_ms"], 100, "{decision}");
    assert!(decision.get("retry_after_ms").is_none(), "{decision}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_that_would_outrun_the_run_deadline_is_refused() {
    let rig = start_rig(
        &retry_after_script("30"),
        RigOptions {
            exec_yaml: Some(execution("")),
            header_extra: "limits:\n  wrap_up_seconds: 0\n  deadline:\n    reserve_seconds: 20\n",
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let thread_id = stub_rig::launch_on_with(&rig, serde_json::json!({"run_timeout": 40}))
        .await
        .expect("launch");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    assert_eq!(status, "failed");
    assert_eq!(rig.stub.requests().await.len(), 1);
    let detail = stub_rig::thread_detail(&rig.h, &thread_id, 4000).await;
    assert!(
        detail.contains("provider_retry_exceeds_run_deadline"),
        "{detail}"
    );
}
