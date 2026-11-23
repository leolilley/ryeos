//! The signed-record read services over a daemon's signed `/execute` route.
//!
//! The operator (the harness principal) lists the installed record kinds and
//! reads the node's capability report with its verification and signer. A
//! second principal holds the records execute capabilities but not the
//! capability to inspect unverified records, and is refused when it asks.

use ryeosd::test_support as common;

use std::path::Path;

use anyhow::Result;
use common::DaemonHarness;
use common::build_signed_headers_for_bytes;
use common::fast_fixture::{FastFixture, write_authorized_key_with_scopes};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

const VIEWER_SCOPES: &[&str] = &[
    "ryeos.execute.service.records/get",
    "ryeos.execute.service.records/list",
    "ryeos.execute.service.records/kinds",
];

fn viewer_key() -> SigningKey {
    SigningKey::from_bytes(&[73_u8; 32])
}

fn plant_viewer(state_path: &Path, _user: &Path, fixture: &FastFixture) -> Result<()> {
    write_authorized_key_with_scopes(state_path, &viewer_key(), &fixture.node, VIEWER_SCOPES)
}

async fn execute_as_viewer(
    harness: &DaemonHarness,
    item_ref: &str,
    params: Value,
) -> Result<(reqwest::StatusCode, Value)> {
    let path = "/execute";
    let bytes = serde_json::to_vec(&json!({
        "item_ref": item_ref,
        "ref_bindings": {},
        "project_path": null,
        "parameters": params,
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    }))?;
    let mut request = reqwest::Client::new()
        .post(format!("http://{}{path}", harness.bind))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in build_signed_headers_for_bytes(
        &viewer_key(),
        &common::fast_fixture::node_signing_key(),
        "POST",
        path,
        &bytes,
    ) {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let value = response.json().await.unwrap_or_else(|_| json!({}));
    Ok((status, value))
}

async fn operator(harness: &DaemonHarness, item_ref: &str, params: Value) -> Value {
    let (status, body) = harness
        .post_execute(item_ref, ".", params)
        .await
        .expect("post /execute");
    assert!(status.is_success(), "{item_ref}: {status} {body}");
    body.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{item_ref}: no result in {body}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_capability_report_is_listed_read_and_summarised() -> Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(plant_viewer, |_| {}).await?;

    let kinds = operator(&harness, "service:records/kinds", json!({})).await;
    let row = kinds["kinds"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["record_kind"] == "capability_report")
        })
        .unwrap_or_else(|| panic!("capability_report is not listed: {kinds}"));
    assert_eq!(row["usable"], true, "{row}");
    assert_eq!(row["verifier"], "node_signed_json_digest");
    assert_eq!(row["storage"], "node_file");

    let got = operator(
        &harness,
        "service:records/get",
        json!({"record_kind": "capability_report"}),
    )
    .await;
    assert_eq!(got["verify"]["status"], "verified", "{got}");
    assert_eq!(got["verify"]["scheme"], "node_signed_json_digest");
    let signer = got["verify"]["signer"].as_str().expect("a signer");
    assert!(signer.starts_with("fp:"), "{got}");
    assert_eq!(
        Some(signer.trim_start_matches("fp:")),
        got["body"]["node_fingerprint"].as_str(),
        "the signer is the node the report names: {got}"
    );
    assert!(got["verify"]["fresh"].is_boolean(), "{got}");
    assert_eq!(got["verify"]["signer_status"], "node", "{got}");
    assert!(got["verify"].get("signing_principal").is_none(), "{got}");
    assert!(got["body"].is_object(), "{got}");
    assert_eq!(got["truncated"], false);
    let hash = got["hash"].as_str().expect("a hash").to_owned();

    let listed = operator(
        &harness,
        "service:records/list",
        json!({"record_kind": "capability_report"}),
    )
    .await;
    let records = listed["records"].as_array().expect("records");
    assert_eq!(records.len(), 1, "{listed}");
    assert_eq!(records[0]["hash"], json!(hash));
    assert_eq!(records[0]["verify"]["status"], "verified");
    assert_eq!(records[0]["summary"]["/schema"], got["body"]["schema"]);
    assert!(records[0]["summary"]["/composition/host_class"].is_string());

    // A hash that is not the record's, and a kind nobody declared, are absent.
    for params in [
        json!({"record_kind": "capability_report", "hash": "sha256:00"}),
        json!({"record_kind": "not_a_declared_kind"}),
    ] {
        let (status, body) = harness
            .post_execute("service:records/get", ".", params)
            .await?;
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{body}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn asking_for_unverified_records_needs_the_capability() -> Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(plant_viewer, |_| {}).await?;

    // The viewer reads the verified report without the extra capability.
    let (status, body) = execute_as_viewer(
        &harness,
        "service:records/get",
        json!({"record_kind": "capability_report"}),
    )
    .await?;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(body["result"]["verify"]["status"], "verified", "{body}");

    let (status, body) = execute_as_viewer(
        &harness,
        "service:records/get",
        json!({"record_kind": "capability_report", "include_unverified": true}),
    )
    .await?;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.to_string()
            .contains("records_unverified_requires_capability"),
        "{body}"
    );

    // The operator holds every capability and may ask.
    let answer = operator(
        &harness,
        "service:records/get",
        json!({"record_kind": "capability_report", "include_unverified": true}),
    )
    .await;
    assert_eq!(answer["verify"]["status"], "verified", "{answer}");
    Ok(())
}
