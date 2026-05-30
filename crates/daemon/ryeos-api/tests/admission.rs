mod test_state;

use serde_json::json;
use std::sync::Arc;

use ryeos_api::handlers::{admission_status, admission_submit};

fn store_subject(state: &ryeos_app::state::AppState) -> String {
    let cas = lillux::cas::CasStore::new(state.state_store.cas_root().unwrap());
    cas.store_object(&json!({
        "kind": "chain_state",
        "schema": 1,
        "chain_root_id": "T-admission",
        "prev_chain_state_hash": null,
        "last_event_hash": null,
        "last_chain_seq": 0,
        "updated_at": "2026-05-30T00:00:00Z",
        "threads": {}
    }))
    .unwrap()
}

#[tokio::test]
async fn admission_submit_writes_attestation_and_status_reads_it() {
    let (_tmp, state) = test_state::build_test_state();
    let subject_hash = store_subject(&state);
    let state = Arc::new(state);

    let submitted = admission_submit::handle(
        admission_submit::Request {
            subject_hash: subject_hash.clone(),
            policy: "test.policy.v1".to_string(),
            claim: "accepted".to_string(),
            max_objects: 16,
            max_object_bytes: 4096,
            max_links_per_object: 16,
        },
        state.clone(),
    )
    .await
    .unwrap();

    assert_eq!(submitted["subject_hash"], subject_hash);
    assert_eq!(submitted["policy"], "test.policy.v1");
    assert_eq!(submitted["claim"], "accepted");
    assert_eq!(submitted["reused_existing"], false);
    let attestation_hash = submitted["attestation_hash"].as_str().unwrap().to_string();
    assert!(lillux::valid_hash(&attestation_hash));

    let status = admission_status::handle(
        admission_status::Request {
            subject_hash: subject_hash.clone(),
            policy: "test.policy.v1".to_string(),
        },
        state.clone(),
    )
    .await
    .unwrap();

    assert_eq!(status["status"], "accepted");
    assert_eq!(status["attestation_hash"], attestation_hash);
    assert_eq!(status["attestation"]["subject_hash"], subject_hash);
    assert_eq!(status["attestation"]["policy"], "test.policy.v1");

    let repeated = admission_submit::handle(
        admission_submit::Request {
            subject_hash,
            policy: "test.policy.v1".to_string(),
            claim: "accepted".to_string(),
            max_objects: 16,
            max_object_bytes: 4096,
            max_links_per_object: 16,
        },
        state,
    )
    .await
    .unwrap();
    assert_eq!(repeated["reused_existing"], true);
    assert_eq!(repeated["attestation_hash"], attestation_hash);
}

#[tokio::test]
async fn admission_status_reports_missing_head() {
    let (_tmp, state) = test_state::build_test_state();
    let status = admission_status::handle(
        admission_status::Request {
            subject_hash: "ab".repeat(32),
            policy: "test.policy.v1".to_string(),
        },
        Arc::new(state),
    )
    .await
    .unwrap();

    assert_eq!(status["status"], "missing");
}
