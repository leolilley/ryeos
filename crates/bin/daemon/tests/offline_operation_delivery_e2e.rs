//! End to end: a signed offline-operation request is delivered to a running
//! node over its authenticated route, queued, and applied at the next
//! startup, where it leaves a node-signed receipt in the ledger.
//!
//! The simplest authority-free operation is used, `node.allow-restart`, since
//! the properties under test are the delivery's and the startup's, not the
//! handler's: the route verifies exactly as the startup applier does; a
//! delivery that does not say `"stop": "now"` leaves the node running; one
//! that does makes a plain operator stop with its own marker reason and no
//! restart inhibition; and the next incarnation applies what was queued.

use ryeosd::test_support as common;

use std::time::Duration;

use common::DaemonHarness;
use lillux::crypto::SigningKey;
use ryeos_engine::roots::RuntimeRoot;
use ryeos_node::lifecycle_marker::{LifecycleMarker, read as read_marker};
use ryeos_node::offline_operation::sign::{Delivery, SignDraft, sign_request_now};
use serde_json::{Value, json};

const OPERATION: &str = "node.allow-restart";

/// A signed GET to an authenticated route of the daemon.
async fn get(daemon: &DaemonHarness, path: &str) -> anyhow::Result<(reqwest::StatusCode, Value)> {
    let user_key = daemon.user_key.as_ref().expect("fast fixture operator key");
    let node_key = daemon.node_key.as_ref().expect("fast fixture node key");
    let mut request = reqwest::Client::new().get(format!("http://{}{path}", daemon.bind));
    for (name, value) in
        common::build_signed_headers_for_bytes(user_key, node_key, "GET", path, b"")
    {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    Ok((status, response.json().await.unwrap_or_else(|_| json!({}))))
}

/// Sign `node.allow-restart` against the facts the running node reports.
async fn signed_delivery(daemon: &DaemonHarness, key: &SigningKey) -> anyhow::Result<Delivery> {
    let (status, report) = get(daemon, "/offline-operations/facts").await?;
    anyhow::ensure!(
        status.is_success(),
        "facts route answered {status}: {report}"
    );
    let facts = report["facts"].as_object().expect("facts object");
    let spec = ryeos_node::offline_operation::table::lookup(OPERATION).expect("operation");
    let pins = ryeos_node::offline_operation::facts::MANDATORY_FACTS
        .iter()
        .chain(spec.preconditions)
        .map(|fact| {
            (
                (*fact).to_owned(),
                facts[*fact].as_str().expect("fact value").to_owned(),
            )
        })
        .collect();
    sign_request_now(
        SignDraft {
            node_fingerprint: report["node_fingerprint"]
                .as_str()
                .expect("node fingerprint")
                .to_owned(),
            operation: OPERATION.to_owned(),
            arguments: json!({}),
            inputs: Default::default(),
            expected_result: Default::default(),
            pins,
            lifetime_minutes: None,
            remote: None,
        },
        key,
    )
}

fn body(delivery: &Delivery, stop: bool) -> Value {
    let mut body = serde_json::to_value(delivery).expect("delivery json");
    if stop {
        body["stop"] = json!("now");
    }
    body
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_submitted_with_stop_is_applied_when_the_node_starts_again() -> anyhow::Result<()>
{
    let (mut daemon, _fixture) = DaemonHarness::start_fast().await?;
    let key = daemon.user_key.clone().expect("fast fixture operator key");
    let delivery = signed_delivery(&daemon, &key).await?;
    let request_digest = delivery.request.digest()?;

    let (status, reply) = daemon
        .post_json("/offline-operations", body(&delivery, true))
        .await?;
    assert!(status.is_success(), "{status}: {reply}");
    assert_eq!(reply["request_digest"], request_digest);
    assert_eq!(reply["stop"], "requested");

    // A plain operator stop: the node exits cleanly with its own reason and
    // inhibits no restart.
    let exit = tokio::time::timeout(Duration::from_secs(120), daemon.child.wait()).await??;
    assert!(exit.success(), "daemon exited with {exit}");
    let state = RuntimeRoot::new(daemon.state_path.clone()).state();
    match read_marker(&state) {
        Some(LifecycleMarker::Exited { reason, .. }) => {
            assert_eq!(reason, "offline_operation_pending")
        }
        other => panic!("expected an exit marker, found {other:?}"),
    }
    assert!(
        ryeos_node::lifecycle_marker::read_restart_inhibition(&state)?.is_none(),
        "the stop must not inhibit a restart"
    );
    // Queued, not yet applied.
    let before = ryeos_node::offline_operation::read_ledger(&daemon.state_path)?;
    assert_eq!(before.last_sequence, 0);

    daemon.respawn_with(|_| {}).await?;

    let after = ryeos_node::offline_operation::read_ledger(&daemon.state_path)?;
    assert_eq!(after.last_sequence, 1);
    assert!(after.request_digests.contains(&request_digest));
    let (status, report) = get(&daemon, "/offline-operations").await?;
    assert!(status.is_success(), "{status}: {report}");
    assert!(report["queued"].as_array().expect("queued").is_empty());
    let receipts = report["receipts"].as_array().expect("receipts");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["request_digest"], request_digest);
    assert_eq!(receipts[0]["operation"], OPERATION);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_without_stop_waits_and_a_forged_one_is_refused() -> anyhow::Result<()> {
    let (daemon, _fixture) = DaemonHarness::start_fast().await?;
    let key = daemon.user_key.clone().expect("fast fixture operator key");
    let delivery = signed_delivery(&daemon, &key).await?;

    // A forged signature is refused with its reason, and nothing is queued.
    let mut forged = delivery.clone();
    forged.request.body.arguments = json!({ "tampered": true });
    let (status, reply) = daemon
        .post_json("/offline-operations", body(&forged, false))
        .await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{reply}");
    assert_eq!(reply["reason"], "bad_signature");
    let (_, report) = get(&daemon, "/offline-operations").await?;
    assert!(report["queued"].as_array().expect("queued").is_empty());

    // A request for another node is refused.
    let mut elsewhere = delivery.clone();
    elsewhere.request.body.node_fingerprint = "c".repeat(64);
    let elsewhere = Delivery {
        request: ryeos_node::offline_operation::contract::SignedRequest::sign(
            elsewhere.request.body,
            &key,
        )?,
        inputs: elsewhere.inputs,
    };
    let (status, reply) = daemon
        .post_json("/offline-operations", body(&elsewhere, false))
        .await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{reply}");
    assert_eq!(reply["reason"], "wrong_node");

    // Without `stop` the request is queued and the node keeps serving.
    let (status, reply) = daemon
        .post_json("/offline-operations", body(&delivery, false))
        .await?;
    assert!(status.is_success(), "{status}: {reply}");
    assert_eq!(reply["stop"], "not_requested");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (status, report) = get(&daemon, "/offline-operations").await?;
    assert!(status.is_success(), "the node stopped without being asked");
    assert_eq!(report["queued"].as_array().expect("queued").len(), 1);

    // Submitting it again is idempotent.
    let (status, reply) = daemon
        .post_json("/offline-operations", body(&delivery, false))
        .await?;
    assert!(status.is_success(), "{status}: {reply}");
    assert_eq!(reply["already_queued"], true);
    Ok(())
}
