//! A node stopped while a provider attempt is Issued, for a directive that
//! declares `provider_contact: repeatable`, end to end against the scripted
//! chat stub.
//!
//! The cut attempt's outcome is unknown, so the resumed run retries it as a
//! mid-stream failure, within the sealed `unknown_outcome_retry` ceiling:
//! one stored advancement per cut, one `provider_retry` event per cut, the
//! cut attempt charged its reserved maximum, and the turn ends typed when the
//! ceiling is spent or the policy does not name `mid_stream`. The route here
//! is priced (a signed tariff), so every attempt is on the accounting ledger.
//!
//! The daemon's native-resume budget (one resume after a crash) is a separate
//! limit that a crash loop is meant to exhaust. These tests re-arm it between
//! stops, as an orderly stop does, so that they exercise the ledger's retry
//! ceiling and not the resume budget.
//!
//! They need populated bundles (`-f bundles=branch`): the retry decision is
//! made by the directive runtime binary the bundles carry.

use ryeosd::test_support as common;

#[path = "common/stub_rig.rs"]
mod stub_rig;
use stub_rig::*;

use std::time::{Duration, Instant};

use common::DaemonHarness;
use serde_json::{Value, json};

fn repeatable_header() -> &'static str {
    "provider_contact: repeatable\nlimits:\n  spend_usd: \"3\"\n"
}

/// The signed execution policy: three status-retry attempts and at most two
/// retries after an unknown outcome, in the shape the PLE directives use.
fn execution_yaml(on: &str) -> String {
    format!(
        "category: \"ryeos-runtime\"\nretries: 3\nbackoff_base_ms: 100\nbackoff_max_ms: 1000\n\
         unknown_outcome_retry:\n  max_retries: 2\n  on: [{on}]\n"
    )
}

const ALL_CLASSES: &str = "timeout, send, mid_stream";

/// One attempt row of the node's accounting ledger.
#[derive(Debug, Clone, PartialEq)]
struct Attempt {
    number: i64,
    state: String,
    reason: Option<String>,
    charge_nanos: Option<i64>,
}

fn accounting_db(h: &DaemonHarness) -> std::path::PathBuf {
    h.state_path
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(ryeos_app::accounting_db::ACCOUNTING_DB_FILENAME)
}

fn attempts(h: &DaemonHarness) -> Vec<Attempt> {
    ryeos_app::test_support::read_existing_accounting(&accounting_db(h), |connection| {
        let mut statement = connection.prepare(
            "SELECT attempt_number, state, reconciliation_reason, budget_charge_usd_nanos \
             FROM provider_attempt_reservation ORDER BY turn, attempt_number",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(Attempt {
                    number: row.get(0)?,
                    state: row.get(1)?,
                    reason: row.get(2)?,
                    charge_nanos: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// How many retry advancements the ledger stored.
fn stored_advancements(h: &DaemonHarness) -> i64 {
    ryeos_app::test_support::read_existing_accounting(&accounting_db(h), |connection| {
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM accounting_operation WHERE operation_kind = 'retry_advance'",
            [],
            |row| row.get(0),
        )?)
    })
}

/// Re-arm the thread's auto-resume budget while the node is down, as an
/// orderly stop does.
fn rearm_resume_budget(h: &DaemonHarness) {
    let db = h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/runtime.sqlite3");
    ryeos_app::test_support::read_existing_runtime(&db, |connection| {
        connection.execute("UPDATE thread_runtime SET resume_attempts = 0", [])?;
        Ok(())
    });
}

/// Wait until the stub holds `requests` requests and the newest attempt of the
/// ledger is Issued as attempt number `requests`.
async fn wait_issued(rig: &Rig, requests: usize) {
    let until = Instant::now() + Duration::from_secs(120);
    loop {
        let received = rig.stub.requests().await.len();
        let rows = attempts(&rig.h);
        if received == requests
            && rows.len() == requests
            && rows.last().is_some_and(|row| row.state == "issued")
        {
            return;
        }
        assert!(
            Instant::now() < until,
            "attempt {requests} never reached Issued with its request held: {received} requests, {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Kill the node while attempt `requests` is Issued, then restart it.
async fn cut_and_restart(rig: &mut Rig, requests: usize) {
    wait_issued(rig, requests).await;
    rig.h.kill_daemon().await.expect("kill the node mid-call");
    rearm_resume_budget(&rig.h);
    rig.h.respawn_with(|_| {}).await.expect("restart the node");
}

async fn launch(rig: &Rig) -> String {
    launch_on_with(rig, json!({})).await.expect("launch")
}

/// The cut attempts of the ledger: charged their reserved maximum by the
/// owner-generation fence.
fn assert_cut(row: &Attempt, number: i64) {
    assert_eq!(row.number, number, "{row:?}");
    assert_eq!(row.state, "charged_reserved_maximum", "{row:?}");
    assert_eq!(
        row.reason.as_deref(),
        Some("owner_generation_fenced"),
        "{row:?}"
    );
    assert!(row.charge_nanos.is_some_and(|nanos| nanos > 0), "{row:?}");
}

fn stream_fragments(events: &[(String, String, Value)]) -> usize {
    events
        .iter()
        .filter(|(_, kind, payload)| {
            kind == "cognition_out"
                && (payload.get("delta").is_some()
                    || payload.get("tool_use").is_some()
                    || payload.get("tool_use_partial").is_some())
        })
        .count()
}

const HOLD: &str = "  - delay_ms: 600000\n    text: \"never delivered\"\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_during_an_issued_attempt_is_retried_and_voids_the_dead_output() {
    // The first attempt streams a few fragments and then stalls; the node is
    // killed while it is held. The retry is the second step.
    let script = "steps:\n  - text: \"a long answer that is cut off part way\"\n    stream: { text_chunk_bytes: 4 }\n    stall: { after_chunks: 5, ms: 600000 }\n  - text: \"done after the restart\"\n";
    let mut rig = start_rig(
        script,
        RigOptions {
            exec_yaml: Some(execution_yaml(ALL_CLASSES)),
            header_extra: repeatable_header(),
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch(&rig).await;

    // Hold the kill until the fragments are on the record.
    let until = Instant::now() + Duration::from_secs(60);
    while stream_fragments(&chain_event_list(&rig.h, &id).await) == 0 {
        assert!(Instant::now() < until, "no fragment was recorded");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let recorded = stream_fragments(&chain_event_list(&rig.h, &id).await);
    cut_and_restart(&mut rig, 1).await;

    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    let detail = thread_detail(&rig.h, &id, 6000).await;
    assert_eq!(status, "completed", "detail: {detail}");
    assert_eq!(
        rig.stub.requests().await.len(),
        2,
        "one contact per attempt"
    );

    let retries = chain_payloads(&rig.h, &id, "provider_retry").await;
    assert_eq!(retries.len(), 1, "one provider_retry per cut: {retries:?}");
    let retry = &retries[0];
    assert_eq!(retry["attempt"], 1, "{retry}");
    assert_eq!(
        retry["retry_advance"]["decision"]["reason"]["kind"], "mid_stream",
        "{retry}"
    );
    assert_eq!(
        retry["supersedes"]["output_events"].as_u64(),
        Some(recorded as u64),
        "the retry voids exactly the fragments the dead attempt left: {retry}"
    );
    eprintln!(
        "EVIDENCE provider_retry supersedes: {}",
        retry["supersedes"]
    );
    assert!(
        recorded > 0,
        "the node keeps stream fragments, so the count is not trivially zero"
    );

    rig.h.kill_daemon().await.expect("stop the node");
    let rows = attempts(&rig.h);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_cut(&rows[0], 1);
    assert_eq!(rows[1].state, "reconciled", "{rows:?}");
    assert_eq!(stored_advancements(&rig.h), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn restarts_during_attempts_one_and_two_are_each_retried_once() {
    let script = format!("steps:\n{HOLD}{HOLD}  - text: \"done after two restarts\"\n");
    let mut rig = start_rig(
        &script,
        RigOptions {
            exec_yaml: Some(execution_yaml(ALL_CLASSES)),
            header_extra: repeatable_header(),
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch(&rig).await;

    cut_and_restart(&mut rig, 1).await;
    cut_and_restart(&mut rig, 2).await;

    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    let detail = thread_detail(&rig.h, &id, 6000).await;
    assert_eq!(status, "completed", "detail: {detail}");
    assert_eq!(rig.stub.requests().await.len(), 3);

    let retries = chain_payloads(&rig.h, &id, "provider_retry").await;
    assert_eq!(retries.len(), 2, "one provider_retry per cut: {retries:?}");
    assert_eq!(retries[0]["attempt"], 1, "{retries:?}");
    assert_eq!(retries[1]["attempt"], 2, "{retries:?}");

    rig.h.kill_daemon().await.expect("stop the node");
    let rows = attempts(&rig.h);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_cut(&rows[0], 1);
    assert_cut(&rows[1], 2);
    assert_eq!(rows[2].state, "reconciled", "{rows:?}");
    assert_eq!(stored_advancements(&rig.h), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_during_the_attempt_past_the_ceiling_ends_the_turn_typed() {
    let script = format!("steps:\n{HOLD}{HOLD}{HOLD}  - text: \"never served\"\n");
    let mut rig = start_rig(
        &script,
        RigOptions {
            exec_yaml: Some(execution_yaml(ALL_CLASSES)),
            header_extra: repeatable_header(),
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch(&rig).await;

    cut_and_restart(&mut rig, 1).await;
    cut_and_restart(&mut rig, 2).await;
    // max_retries is 2: attempt 3 is the last contact, and a cut there has no
    // retry left.
    cut_and_restart(&mut rig, 3).await;

    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    let detail = thread_detail(&rig.h, &id, 6000).await;
    assert_eq!(status, "failed", "detail: {detail}");
    assert!(
        detail.contains("\"code\":\"provider_attempt_outcome_unknown\""),
        "the failure carries the typed code: {detail}"
    );
    assert_eq!(
        rig.stub.requests().await.len(),
        3,
        "no fourth contact is made"
    );

    rig.h.kill_daemon().await.expect("stop the node");
    let rows = attempts(&rig.h);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_cut(&rows[0], 1);
    assert_cut(&rows[1], 2);
    assert_cut(&rows[2], 3);
    assert_eq!(stored_advancements(&rig.h), 2, "the third cut stores none");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_that_does_not_name_mid_stream_leaves_a_restart_cut_unknown() {
    let script = format!("steps:\n{HOLD}  - text: \"never served\"\n");
    let mut rig = start_rig(
        &script,
        RigOptions {
            exec_yaml: Some(execution_yaml("timeout, send")),
            header_extra: repeatable_header(),
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch(&rig).await;

    cut_and_restart(&mut rig, 1).await;

    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    let detail = thread_detail(&rig.h, &id, 6000).await;
    assert_eq!(status, "failed", "detail: {detail}");
    assert!(
        detail.contains("\"code\":\"provider_attempt_outcome_unknown\""),
        "{detail}"
    );
    assert_eq!(rig.stub.requests().await.len(), 1);
    rig.h.kill_daemon().await.expect("stop the node");
    assert_eq!(stored_advancements(&rig.h), 0);
}

/// A planned rollover (the service window reaching its drain margin) cuts a
/// call the provider is still holding. Exit 75, then the next start retries
/// the cut attempt. The window is 300 s with a 60 s margin, so the rollover
/// begins about 180 s after the node starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_planned_rollover_during_a_provider_call_is_retried_after_the_restart() {
    let script = format!("steps:\n{HOLD}  - text: \"done after the rollover\"\n");
    let mut rig = start_rig(
        &script,
        RigOptions {
            exec_yaml: Some(execution_yaml(ALL_CLASSES)),
            header_extra: repeatable_header(),
            paid_provider: true,
            long_service_window: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch(&rig).await;
    wait_issued(&rig, 1).await;

    let status = tokio::time::timeout(Duration::from_secs(300), rig.h.child.wait())
        .await
        .expect("the service window ends within its bound")
        .expect("wait for the node to exit");
    assert_eq!(
        status.code(),
        Some(75),
        "a planned rollover exits 75: {status:?}\n--- daemon stderr ---\n{}",
        rig.h.stderr_so_far()
    );
    let rows = attempts(&rig.h);
    assert_eq!(rows.len(), 1, "{rows:?}");
    eprintln!("EVIDENCE ledger after the planned rollover exit: {rows:?}");
    assert_eq!(
        rows[0].state, "issued",
        "the rollover left the cut attempt issued for the next start to fence: {rows:?}"
    );

    rig.h.respawn_with(|_| {}).await.expect("restart the node");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    let detail = thread_detail(&rig.h, &id, 6000).await;
    assert_eq!(status, "completed", "detail: {detail}");
    assert_eq!(rig.stub.requests().await.len(), 2);
    let retries = chain_payloads(&rig.h, &id, "provider_retry").await;
    assert_eq!(retries.len(), 1, "{retries:?}");

    rig.h.kill_daemon().await.expect("stop the node");
    let rows = attempts(&rig.h);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_cut(&rows[0], 1);
    assert_eq!(rows[1].state, "reconciled", "{rows:?}");
    assert_eq!(stored_advancements(&rig.h), 1);
}
