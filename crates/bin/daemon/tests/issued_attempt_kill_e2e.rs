//! A node killed while a provider attempt is Issued, end to end against the
//! scripted chat stub.
//!
//! The attempt's outcome is unknown: the request may have reached the
//! provider and its answer was never recorded. A directive that does not
//! declare `provider_contact: repeatable` must not repeat the contact, so the
//! resumed run fails with the typed code `provider_attempt_outcome_unknown`
//! and the stub sees exactly one request. They need populated bundles
//! (`-f bundles=branch`, or `-f bundles=next` on a branch with the same
//! bundles).

use ryeosd::test_support as common;

#[path = "common/stub_rig.rs"]
mod stub_rig;
use stub_rig::*;

use std::time::{Duration, Instant};

use common::DaemonHarness;
use serde_json::json;

/// Hard spend limit so the stub route is paid and attempts are reserved.
const FINITE_BUDGET: &str = "limits:\n  spend_usd: \"3\"\n";

/// `(state, budget_charge_usd_nanos)` of every provider attempt in the node's
/// accounting ledger, in attempt order.
fn ledger_states(h: &DaemonHarness) -> Vec<(String, Option<i64>)> {
    let db = h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(ryeos_app::accounting_db::ACCOUNTING_DB_FILENAME);
    ryeos_app::test_support::read_existing_accounting(&db, |connection| {
        let mut statement = connection.prepare(
            "SELECT state, budget_charge_usd_nanos \
             FROM provider_attempt_reservation ORDER BY turn, attempt_number",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kill_while_a_provider_attempt_is_issued_fails_typed_without_a_second_contact() {
    // The stub holds the one request for longer than the test runs.
    let script = "steps:\n  - delay_ms: 120000\n    text: \"never delivered\"\n";
    let mut rig = start_rig(
        script,
        RigOptions {
            header_extra: FINITE_BUDGET,
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on_with(&rig, json!({})).await.expect("launch");

    // Kill only when the stub holds the request and the ledger records the
    // attempt as Issued: the request was sent, its answer never recorded.
    let issued_until = Instant::now() + Duration::from_secs(90);
    loop {
        let received = rig.stub.requests().await.len();
        let states = ledger_states(&rig.h);
        if received == 1 && states.len() == 1 && states[0].0 == "issued" {
            break;
        }
        assert!(
            Instant::now() < issued_until,
            "the attempt never reached Issued with the request held: {received} requests, {states:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    rig.h.kill_daemon().await.expect("kill the node mid-call");
    rig.h.respawn_with(|_| {}).await.expect("restart the node");

    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    let detail = thread_detail(&rig.h, &id, 6000).await;
    assert_eq!(status, "failed", "detail: {detail}");
    assert!(
        detail.contains("\"code\":\"provider_attempt_outcome_unknown\""),
        "the failure carries the typed code, not directive_failed: {detail}"
    );
    assert!(
        detail.contains("provider_contact: repeatable"),
        "the failure names the cure: {detail}"
    );
    assert_eq!(
        rig.stub.requests().await.len(),
        1,
        "the resumed run never contacted the provider again"
    );
    rig.h.kill_daemon().await.expect("stop the node");
    let states = ledger_states(&rig.h);
    assert_eq!(states.len(), 1, "one attempt row: {states:?}");
    assert_eq!(states[0].0, "charged_reserved_maximum", "{states:?}");
}
