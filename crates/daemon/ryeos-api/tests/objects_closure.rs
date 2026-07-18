mod test_state;

use base64::Engine as _;
use serde_json::{json, Value};
use std::sync::Arc;

use ryeos_api::handlers::{objects_closure_describe, objects_closure_get};

fn request(root: String, max_objects: usize) -> objects_closure_describe::Request {
    objects_closure_describe::Request {
        roots: vec![root],
        max_objects,
        max_blobs: 16,
        max_object_bytes: 1024,
        max_total_object_bytes: 4096,
        max_blob_bytes: 1024,
        max_total_blob_bytes: 4096,
        max_response_bytes: 8192,
        max_links_per_object: 16,
        allow_incomplete: false,
    }
}

fn store_fixture(state: &ryeos_app::state::AppState) -> (String, String, Vec<u8>) {
    let cas = lillux::cas::CasStore::new(state.state_store.cas_root().unwrap());
    let blob = b"hello closure".to_vec();
    let blob_hash = cas.store_blob(&blob).unwrap();
    let file_hash = cas
        .store_object(
            &ryeos_state::objects::ProjectFile {
                blob_hash: blob_hash.clone(),
                size: blob.len() as u64,
                normalized_mode: 0o644,
            }
            .to_value(),
        )
        .unwrap();
    let tree_hash = cas
        .store_object(
            &ryeos_state::objects::ProjectTree {
                files: std::collections::BTreeMap::from([(
                    ".ai/directives/test/closure.md".to_string(),
                    file_hash,
                )]),
            }
            .to_value(),
        )
        .unwrap();
    let policy_hash = cas
        .store_object(
            &ryeos_state::objects::ProjectSnapshotPolicy::new(
                ryeos_state::project_sync::ProjectSyncScope::FullProject,
                Vec::new(),
                Vec::new(),
                std::collections::BTreeMap::new(),
            )
            .unwrap()
            .to_value(),
        )
        .unwrap();
    let snapshot_hash = cas
        .store_object(&json!({
            "kind": "project_snapshot",
            "schema": ryeos_state::objects::ProjectSnapshot::SCHEMA,
            "project_tree_hash": tree_hash,
            "effective_policy_hash": policy_hash,
            "message": null,
            "parent_hashes": [],
            "created_at": "2026-05-29T00:00:00Z",
            "source": "test",
        }))
        .unwrap();

    (snapshot_hash, blob_hash, blob)
}

fn values<'a>(entries: &'a [Value], kind: &str) -> Vec<&'a Value> {
    entries
        .iter()
        .filter(|entry| entry.get("kind").and_then(|v| v.as_str()) == Some(kind))
        .collect()
}

#[tokio::test]
async fn closure_describe_reports_objects_and_blobs() {
    let (_tmp, state) = test_state::build_test_state();
    let (snapshot_hash, blob_hash, _blob) = store_fixture(&state);

    let value =
        objects_closure_describe::handle(request(snapshot_hash.clone(), 16), Arc::new(state))
            .await
            .unwrap();

    assert_eq!(value["complete"], true);
    assert_eq!(value["roots"], json!([snapshot_hash]));
    assert_eq!(value["object_hashes"].as_array().unwrap().len(), 3);
    assert_eq!(value["blob_hashes"], json!([blob_hash]));
}

#[tokio::test]
async fn closure_get_returns_present_entries() {
    let (_tmp, state) = test_state::build_test_state();
    let (snapshot_hash, blob_hash, blob) = store_fixture(&state);

    let value = objects_closure_get::handle(request(snapshot_hash, 16), Arc::new(state))
        .await
        .unwrap();

    assert_eq!(value["closure"]["complete"], true);
    assert_eq!(value["closure"]["counts"]["objects"], 3);
    assert_eq!(value["closure"]["counts"]["blobs"], 1);

    let entries = value["entries"].as_array().unwrap();
    assert_eq!(values(entries, "object").len(), 3);

    let blob_entries = values(entries, "blob");
    assert_eq!(blob_entries.len(), 1);
    assert_eq!(blob_entries[0]["hash"], blob_hash);
    assert_eq!(
        blob_entries[0]["data"],
        base64::engine::general_purpose::STANDARD.encode(blob)
    );
}

#[tokio::test]
async fn closure_get_enforces_blob_byte_budget() {
    let (_tmp, state) = test_state::build_test_state();
    let (snapshot_hash, _blob_hash, _blob) = store_fixture(&state);
    let mut req = request(snapshot_hash, 16);
    req.max_blob_bytes = 1;

    let err = objects_closure_get::handle(req, Arc::new(state))
        .await
        .unwrap_err();

    assert!(format!("{err:#}").contains("max_blob_bytes=1"));
}

#[tokio::test]
async fn closure_get_enforces_object_byte_budget() {
    let (_tmp, state) = test_state::build_test_state();
    let (snapshot_hash, _blob_hash, _blob) = store_fixture(&state);
    let mut req = request(snapshot_hash, 16);
    req.max_total_object_bytes = 1;

    let err = objects_closure_get::handle(req, Arc::new(state))
        .await
        .unwrap_err();

    assert!(err.to_string().contains("exceeds max_total_object_bytes"));
}

#[tokio::test]
async fn closure_get_rejects_incomplete_by_default() {
    let (_tmp, state) = test_state::build_test_state();
    let missing = "12".repeat(32);

    let err = objects_closure_get::handle(request(missing, 16), Arc::new(state))
        .await
        .unwrap_err();

    assert!(err.to_string().contains("object closure is incomplete"));
}

#[tokio::test]
async fn closure_describe_enforces_max_objects() {
    let (_tmp, state) = test_state::build_test_state();
    let (snapshot_hash, _blob_hash, _blob) = store_fixture(&state);

    let err = objects_closure_describe::handle(request(snapshot_hash, 2), Arc::new(state))
        .await
        .unwrap_err();

    assert!(err.to_string().contains("exceeds max_objects"));
}
