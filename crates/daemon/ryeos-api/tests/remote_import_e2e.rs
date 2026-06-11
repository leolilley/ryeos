mod test_state;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use ryeos_api::handlers::{
    admission_attestations_for_subject, admission_submit, federation_heads_list,
    objects_closure_get, remote_import_admitted_head, remote_sync_admitted_heads,
};
use ryeos_api::remote::config::{self, RemoteConfig};
use ryeos_app::state::AppState;
use ryeos_state::{CasEntryKind, CasEntryState, SyncJobState};

fn store_subject(state: &AppState) -> String {
    store_subject_with_root(state, "T-remote-import-e2e")
}

fn store_subject_with_root(state: &AppState, chain_root_id: &str) -> String {
    let cas = lillux::cas::CasStore::new(state.state_store.cas_root().unwrap());
    cas.store_object(&json!({
        "kind": "chain_state",
        "schema": 1,
        "chain_root_id": chain_root_id,
        "prev_chain_state_hash": null,
        "last_event_hash": null,
        "last_chain_seq": 0,
        "updated_at": "2026-05-30T00:00:00Z",
        "threads": {}
    }))
    .unwrap()
}

fn store_item_subject_with_blob(state: &AppState) -> String {
    store_item_subject_with_blob_content(state, b"remote import dependency")
}

fn store_item_subject_with_blob_content(state: &AppState, content: &[u8]) -> String {
    let cas = lillux::cas::CasStore::new(state.state_store.cas_root().unwrap());
    let blob_hash = cas.store_blob(content).unwrap();
    cas.store_object(&json!({
        "kind": "item_source",
        "item_ref": "directive:test/remote-import",
        "content_blob_hash": blob_hash,
        "integrity": "none"
    }))
    .unwrap()
}

async fn json_handler<T, Fut>(
    state: Arc<AppState>,
    body: Value,
    handle: impl FnOnce(T, Arc<AppState>) -> Fut,
) -> Result<Json<Value>, (StatusCode, Json<Value>)>
where
    T: DeserializeOwned,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let req = serde_json::from_value::<T>(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    handle(req, state).await.map(Json).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })
}

async fn start_remote_server(
    state: Arc<AppState>,
) -> Result<(String, tokio::task::JoinHandle<()>)> {
    start_remote_server_with_closure(state, false).await
}

async fn start_remote_server_with_closure(
    state: Arc<AppState>,
    lie_about_closure: bool,
) -> Result<(String, tokio::task::JoinHandle<()>)> {
    let incomplete_roots = if lie_about_closure {
        None
    } else {
        Some(HashSet::new())
    };
    start_remote_server_with_incomplete_roots(state, incomplete_roots).await
}

async fn start_remote_server_with_incomplete_roots(
    state: Arc<AppState>,
    incomplete_roots: Option<HashSet<String>>,
) -> Result<(String, tokio::task::JoinHandle<()>)> {
    let incomplete_roots = Arc::new(incomplete_roots);
    let app = Router::new()
        .route(
            "/federation/heads/list",
            post(
                |State(state): State<Arc<AppState>>, Json(body): Json<Value>| async move {
                    json_handler::<federation_heads_list::Request, _>(
                        state,
                        body,
                        federation_heads_list::handle,
                    )
                    .await
                },
            ),
        )
        .route(
            "/admission/attestations-for-subject",
            post(
                |State(state): State<Arc<AppState>>, Json(body): Json<Value>| async move {
                    json_handler::<admission_attestations_for_subject::Request, _>(
                        state,
                        body,
                        admission_attestations_for_subject::handle,
                    )
                    .await
                },
            ),
        )
        .route(
            "/objects/closure/get",
            post(
                move |State(state): State<Arc<AppState>>, Json(body): Json<Value>| async move {
                    let root = body
                        .get("roots")
                        .and_then(Value::as_array)
                        .and_then(|roots| roots.first())
                        .and_then(Value::as_str);
                    let should_lie = match (&*incomplete_roots, root) {
                        (None, Some(_)) => true,
                        (Some(roots), Some(root)) => roots.contains(root),
                        _ => false,
                    };
                    if should_lie {
                        return incomplete_closure_response(state, body).await;
                    }
                    json_handler::<ryeos_api::handlers::objects_closure_describe::Request, _>(
                        state,
                        body,
                        objects_closure_get::handle,
                    )
                    .await
                },
            ),
        )
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}"), handle))
}

async fn incomplete_closure_response(
    state: Arc<AppState>,
    body: Value,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let roots = body.get("roots").and_then(Value::as_array).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "roots must be an array" })),
        )
    })?;
    let root = roots
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing root" })),
            )
        })?
        .to_string();
    let cas = lillux::cas::CasStore::new(state.state_store.cas_root().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })?);
    let value = cas.get_object(&root).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })?;
    let Some(value) = value else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "root not found" })),
        ));
    };
    Ok(Json(json!({
        "closure": {
            "roots": [root.clone()],
            "complete": true,
            "object_hashes": [root.clone()],
            "blob_hashes": [],
            "missing_objects": [],
            "missing_blobs": [],
            "malformed_objects": [],
            "unsupported_objects": []
        },
        "object_bytes": 0,
        "blob_bytes": 0,
        "entries": [{
            "hash": root,
            "kind": "object",
            "value": value,
        }]
    })))
}

fn remote_config(name: &str, url: &str, remote: &AppState) -> RemoteConfig {
    RemoteConfig {
        name: name.to_string(),
        url: url.to_string(),
        principal_id: remote.identity.principal_id(),
        signing_key: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD
                .encode(remote.identity.verifying_key().as_bytes())
        ),
        site_id: remote.threads.site_id().to_string(),
        vault_fingerprint: "sha256:test-vault".to_string(),
        ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
        project_bindings: HashMap::new(),
    }
}

fn install_remote(local: &AppState, cfg: RemoteConfig) {
    let mut remotes = HashMap::new();
    remotes.insert(cfg.name.clone(), cfg);
    config::save_remotes(&local.config.app_root, &remotes).unwrap();
}

async fn admit_subject(state: Arc<AppState>, subject_hash: String) -> String {
    let admitted = admission_submit::handle(
        admission_submit::Request {
            subject_hash,
            policy: "local-node-v1".to_string(),
            claim: "accepted".to_string(),
            max_objects: 16,
            max_blobs: 16,
            max_object_bytes: 4096,
            max_blob_bytes: 4096,
            max_total_blob_bytes: 4096,
            max_links_per_object: 16,
        },
        state,
    )
    .await
    .unwrap();
    admitted["attestation_hash"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn import_admitted_head_mirrors_remote_closure_and_records_job() {
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let subject_hash = store_subject(&remote_state);
    let remote_state = Arc::new(remote_state);
    let admitted = admission_submit::handle(
        admission_submit::Request {
            subject_hash: subject_hash.clone(),
            policy: "local-node-v1".to_string(),
            claim: "accepted".to_string(),
            max_objects: 16,
            max_blobs: 16,
            max_object_bytes: 4096,
            max_blob_bytes: 4096,
            max_total_blob_bytes: 4096,
            max_links_per_object: 16,
        },
        remote_state.clone(),
    )
    .await
    .unwrap();
    let attestation_hash = admitted["attestation_hash"].as_str().unwrap().to_string();

    let (base_url, server) = start_remote_server(remote_state.clone()).await.unwrap();
    let (_local_tmp, local_state) = test_state::build_test_state();
    install_remote(
        &local_state,
        remote_config("upstream", &base_url, &remote_state),
    );
    let local_state = Arc::new(local_state);

    let imported = remote_import_admitted_head::handle(
        remote_import_admitted_head::Request {
            remote: "upstream".to_string(),
            project: None,
            policy: "local-node-v1".to_string(),
            subject_hash: Some(subject_hash.clone()),
            limit: 100,
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_response_bytes: Some(64 * 1024),
            max_links_per_object: Some(16),
        },
        local_state.clone(),
    )
    .await
    .unwrap();

    assert_eq!(imported["subject_hash"], subject_hash);
    assert_eq!(imported["attestation_hash"], attestation_hash);
    assert_eq!(imported["head_attestation_hash"], attestation_hash);
    assert_eq!(imported["mirrored_objects"].as_u64().unwrap(), 2);

    let local_cas = lillux::cas::CasStore::new(local_state.state_store.cas_root().unwrap());
    assert!(local_cas.get_object(&subject_hash).unwrap().is_some());
    assert!(local_cas.get_object(&attestation_hash).unwrap().is_some());

    local_state
        .state_store
        .with_state_db(|db| {
            let subject = db
                .get_cas_entry(CasEntryKind::Object, &subject_hash)?
                .expect("subject root should be attributed");
            assert_eq!(subject.state, CasEntryState::Mirrored);
            assert_eq!(subject.source_peer.as_deref(), Some("upstream"));

            let attestation = db
                .get_cas_entry(CasEntryKind::Object, &attestation_hash)?
                .expect("attestation should be attributed");
            assert_eq!(attestation.state, CasEntryState::Mirrored);
            assert_eq!(attestation.source_peer.as_deref(), Some("upstream"));

            let job = db
                .get_sync_job(imported["job_id"].as_str().unwrap())?
                .expect("import job should be recorded");
            assert_eq!(job.state.as_str(), "completed");
            assert_eq!(job.peer.as_deref(), Some("upstream"));
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();

    server.abort();
}

#[tokio::test]
async fn sync_admitted_heads_mirrors_missing_heads_and_is_idempotent() {
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let subject_a = store_subject_with_root(&remote_state, "T-remote-import-e2e-a");
    let subject_b = store_item_subject_with_blob(&remote_state);
    let subject_c = store_subject_with_root(&remote_state, "T-remote-import-e2e-c");
    let remote_state = Arc::new(remote_state);
    let attestation_a = admit_subject(remote_state.clone(), subject_a.clone()).await;
    let attestation_b = admit_subject(remote_state.clone(), subject_b.clone()).await;
    let attestation_c = admit_subject(remote_state.clone(), subject_c.clone()).await;

    let (base_url, server) = start_remote_server(remote_state.clone()).await.unwrap();
    let (_local_tmp, local_state) = test_state::build_test_state();
    install_remote(
        &local_state,
        remote_config("upstream", &base_url, &remote_state),
    );
    let local_state = Arc::new(local_state);

    let first = remote_sync_admitted_heads::handle(
        remote_sync_admitted_heads::Request {
            remote: "upstream".to_string(),
            project: None,
            policy: "local-node-v1".to_string(),
            limit: 100,
            max_imports: Some(2),
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_response_bytes: Some(64 * 1024),
            max_links_per_object: Some(16),
        },
        local_state.clone(),
    )
    .await
    .unwrap();
    assert_eq!(first["listed"].as_u64().unwrap(), 3);
    assert_eq!(first["imported_heads"].as_u64().unwrap(), 2);
    assert_eq!(first["skipped"].as_u64().unwrap(), 1);

    let second = remote_sync_admitted_heads::handle(
        remote_sync_admitted_heads::Request {
            remote: "upstream".to_string(),
            project: None,
            policy: "local-node-v1".to_string(),
            limit: 100,
            max_imports: None,
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_response_bytes: Some(64 * 1024),
            max_links_per_object: Some(16),
        },
        local_state.clone(),
    )
    .await
    .unwrap();
    assert_eq!(second["listed"].as_u64().unwrap(), 3);
    assert_eq!(second["imported_heads"].as_u64().unwrap(), 1);
    assert_eq!(second["skipped"].as_u64().unwrap(), 2);

    let third = remote_sync_admitted_heads::handle(
        remote_sync_admitted_heads::Request {
            remote: "upstream".to_string(),
            project: None,
            policy: "local-node-v1".to_string(),
            limit: 100,
            max_imports: None,
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_response_bytes: Some(64 * 1024),
            max_links_per_object: Some(16),
        },
        local_state.clone(),
    )
    .await
    .unwrap();
    assert_eq!(third["listed"].as_u64().unwrap(), 3);
    assert_eq!(third["imported_heads"].as_u64().unwrap(), 0);
    assert_eq!(third["skipped"].as_u64().unwrap(), 3);

    local_state
        .state_store
        .with_state_db(|db| {
            for hash in [&subject_a, &subject_b, &subject_c] {
                let entry = db
                    .get_cas_entry(CasEntryKind::Object, hash)?
                    .expect("subject should be mirrored");
                assert_eq!(entry.state, CasEntryState::Mirrored);
                assert_eq!(entry.source_peer.as_deref(), Some("upstream"));
            }
            for hash in [&attestation_a, &attestation_b, &attestation_c] {
                let entry = db
                    .get_cas_entry(CasEntryKind::Object, hash)?
                    .expect("attestation should be mirrored");
                assert_eq!(entry.state, CasEntryState::Mirrored);
                assert_eq!(entry.source_peer.as_deref(), Some("upstream"));
            }
            let job = db
                .get_sync_job(first["job_id"].as_str().unwrap())?
                .expect("batch sync job should be recorded");
            assert_eq!(job.state.as_str(), "completed");
            assert_eq!(job.peer.as_deref(), Some("upstream"));
            assert_eq!(job.operation_type, "remote_sync_admitted_heads");
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();

    server.abort();
}

#[tokio::test]
async fn sync_admitted_heads_records_partial_progress_on_later_failure() {
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let good_subject = store_subject_with_root(&remote_state, "T-remote-import-partial-good");
    let mut bad_subject = store_item_subject_with_blob_content(&remote_state, b"bad dependency 0");
    for index in 1..64 {
        if good_subject < bad_subject {
            break;
        }
        bad_subject = store_item_subject_with_blob_content(
            &remote_state,
            format!("bad dependency {index}").as_bytes(),
        );
    }
    assert!(
        good_subject < bad_subject,
        "test setup requires good head to sort before bad head"
    );
    let remote_state = Arc::new(remote_state);
    let good_attestation = admit_subject(remote_state.clone(), good_subject.clone()).await;
    let bad_attestation = admit_subject(remote_state.clone(), bad_subject.clone()).await;

    let (base_url, server) = start_remote_server_with_incomplete_roots(
        remote_state.clone(),
        Some(HashSet::from([bad_subject.clone()])),
    )
    .await
    .unwrap();
    let (_local_tmp, local_state) = test_state::build_test_state();
    install_remote(
        &local_state,
        remote_config("upstream", &base_url, &remote_state),
    );
    let local_state = Arc::new(local_state);

    let err = remote_sync_admitted_heads::handle(
        remote_sync_admitted_heads::Request {
            remote: "upstream".to_string(),
            project: None,
            policy: "local-node-v1".to_string(),
            limit: 100,
            max_imports: None,
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_response_bytes: Some(64 * 1024),
            max_links_per_object: Some(16),
        },
        local_state.clone(),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("incomplete after local verification"),
        "unexpected error: {err:#}"
    );

    local_state
        .state_store
        .with_state_db(|db| {
            let good = db
                .get_cas_entry(CasEntryKind::Object, &good_subject)?
                .expect("good subject should be mirrored before later failure");
            assert_eq!(good.state, CasEntryState::Mirrored);
            let good_attestation_entry = db
                .get_cas_entry(CasEntryKind::Object, &good_attestation)?
                .expect("good attestation should be mirrored before later failure");
            assert_eq!(good_attestation_entry.state, CasEntryState::Mirrored);
            let bad = db
                .get_cas_entry(CasEntryKind::Object, &bad_subject)?
                .expect("bad subject should be staged before local verification fails");
            assert_eq!(bad.state, CasEntryState::Staged);
            let jobs = db.list_sync_jobs_by_state(Some(SyncJobState::Failed), 10)?;
            let job = jobs
                .iter()
                .find(|job| job.operation_type == "remote_sync_admitted_heads")
                .expect("failed batch sync job should be recorded");
            assert_eq!(job.peer.as_deref(), Some("upstream"));
            assert_eq!(
                job.fetched_hashes,
                vec![good_subject.clone(), good_attestation]
            );
            let result = job
                .result
                .as_ref()
                .expect("partial result should be recorded");
            assert_eq!(result["partial"], true);
            assert_eq!(result["imported_heads"].as_u64().unwrap(), 1);
            assert!(result["error"]
                .as_str()
                .unwrap()
                .contains("incomplete after local verification"));
            assert!(!job.fetched_hashes.contains(&bad_attestation));
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();

    server.abort();
}

#[tokio::test]
async fn import_admitted_head_rejects_wrong_pinned_key() {
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let subject_hash = store_subject(&remote_state);
    let remote_state = Arc::new(remote_state);
    admission_submit::handle(
        admission_submit::Request {
            subject_hash: subject_hash.clone(),
            policy: "local-node-v1".to_string(),
            claim: "accepted".to_string(),
            max_objects: 16,
            max_blobs: 16,
            max_object_bytes: 4096,
            max_blob_bytes: 4096,
            max_total_blob_bytes: 4096,
            max_links_per_object: 16,
        },
        remote_state.clone(),
    )
    .await
    .unwrap();

    let (base_url, server) = start_remote_server(remote_state.clone()).await.unwrap();
    let (_local_tmp, local_state) = test_state::build_test_state();
    let mut cfg = remote_config("upstream", &base_url, &remote_state);
    let wrong_key = lillux::crypto::SigningKey::from_bytes(&[42; 32]).verifying_key();
    cfg.signing_key = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(wrong_key.as_bytes())
    );
    cfg.principal_id = format!("fp:{}", lillux::crypto::fingerprint(&wrong_key));
    install_remote(&local_state, cfg);
    let local_state = Arc::new(local_state);

    let err = remote_import_admitted_head::handle(
        remote_import_admitted_head::Request {
            remote: "upstream".to_string(),
            project: None,
            policy: "local-node-v1".to_string(),
            subject_hash: Some(subject_hash),
            limit: 100,
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_response_bytes: Some(64 * 1024),
            max_links_per_object: Some(16),
        },
        local_state,
    )
    .await
    .unwrap_err();

    assert!(
        format!("{err:#}").contains("failed to verify federated head"),
        "unexpected error: {err:#}"
    );

    server.abort();
}

#[tokio::test]
async fn sync_admitted_heads_rejects_wrong_pinned_key_before_importing() {
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let subject_hash = store_subject(&remote_state);
    let remote_state = Arc::new(remote_state);
    admit_subject(remote_state.clone(), subject_hash.clone()).await;

    let (base_url, server) = start_remote_server(remote_state.clone()).await.unwrap();
    let (_local_tmp, local_state) = test_state::build_test_state();
    let mut cfg = remote_config("upstream", &base_url, &remote_state);
    let wrong_key = lillux::crypto::SigningKey::from_bytes(&[43; 32]).verifying_key();
    cfg.signing_key = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(wrong_key.as_bytes())
    );
    cfg.principal_id = format!("fp:{}", lillux::crypto::fingerprint(&wrong_key));
    install_remote(&local_state, cfg);
    let local_state = Arc::new(local_state);

    let err = remote_sync_admitted_heads::handle(
        remote_sync_admitted_heads::Request {
            remote: "upstream".to_string(),
            project: None,
            policy: "local-node-v1".to_string(),
            limit: 100,
            max_imports: None,
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_response_bytes: Some(64 * 1024),
            max_links_per_object: Some(16),
        },
        local_state.clone(),
    )
    .await
    .unwrap_err();

    assert!(
        format!("{err:#}").contains("failed to verify federated head"),
        "unexpected error: {err:#}"
    );
    local_state
        .state_store
        .with_state_db(|db| {
            assert!(db
                .get_cas_entry(CasEntryKind::Object, &subject_hash)?
                .is_none());
            let jobs = db.list_sync_jobs_by_state(Some(SyncJobState::Failed), 10)?;
            let job = jobs
                .iter()
                .find(|job| job.operation_type == "remote_sync_admitted_heads")
                .expect("discovery failure should still record a batch sync job");
            assert_eq!(job.phase, "failed");
            assert_eq!(job.peer.as_deref(), Some("upstream"));
            assert!(job.roots.is_empty());
            assert!(job.heads.is_empty());
            assert!(job.fetched_hashes.is_empty());
            assert!(job
                .last_error
                .as_deref()
                .unwrap()
                .contains("failed to verify federated head"));
            let result = job
                .result
                .as_ref()
                .expect("failed result should be recorded");
            assert_eq!(result["partial"], true);
            assert_eq!(result["imported_heads"].as_u64().unwrap(), 0);
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();

    server.abort();
}

#[tokio::test]
async fn import_admitted_head_rejects_remote_closure_completeness_lie_before_mirroring() {
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let subject_hash = store_item_subject_with_blob(&remote_state);
    let remote_state = Arc::new(remote_state);
    admission_submit::handle(
        admission_submit::Request {
            subject_hash: subject_hash.clone(),
            policy: "local-node-v1".to_string(),
            claim: "accepted".to_string(),
            max_objects: 16,
            max_blobs: 16,
            max_object_bytes: 4096,
            max_blob_bytes: 4096,
            max_total_blob_bytes: 4096,
            max_links_per_object: 16,
        },
        remote_state.clone(),
    )
    .await
    .unwrap();

    let (base_url, server) = start_remote_server_with_closure(remote_state.clone(), true)
        .await
        .unwrap();
    let (_local_tmp, local_state) = test_state::build_test_state();
    install_remote(
        &local_state,
        remote_config("upstream", &base_url, &remote_state),
    );
    let local_state = Arc::new(local_state);

    let err = remote_import_admitted_head::handle(
        remote_import_admitted_head::Request {
            remote: "upstream".to_string(),
            project: None,
            policy: "local-node-v1".to_string(),
            subject_hash: Some(subject_hash.clone()),
            limit: 100,
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_response_bytes: Some(64 * 1024),
            max_links_per_object: Some(16),
        },
        local_state.clone(),
    )
    .await
    .unwrap_err();

    assert!(
        format!("{err:#}").contains("incomplete after local verification"),
        "unexpected error: {err:#}"
    );

    local_state
        .state_store
        .with_state_db(|db| {
            let subject = db
                .get_cas_entry(CasEntryKind::Object, &subject_hash)?
                .expect("subject should be staged before local closure verification fails");
            assert_eq!(subject.state, CasEntryState::Staged);
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();

    server.abort();
}

#[test]
fn import_services_expose_safe_default_and_admin_escape_hatch() {
    assert_eq!(
        remote_import_admitted_head::DESCRIPTOR.endpoint,
        "remote.import-admitted-head"
    );
    assert_eq!(
        remote_import_admitted_head::DESCRIPTOR.required_caps,
        &["ryeos.execute.service.remote/import-admitted-head"]
    );
    assert_eq!(
        ryeos_api::handlers::remote_import_admitted_root::DESCRIPTOR.endpoint,
        "remote.import-admitted-root-advanced"
    );
    assert_eq!(
        ryeos_api::handlers::remote_import_admitted_root::DESCRIPTOR.required_caps,
        &["ryeos.execute.service.remote/admin"]
    );
    assert_eq!(
        remote_sync_admitted_heads::DESCRIPTOR.endpoint,
        "remote.sync-admitted-heads"
    );
    assert_eq!(
        remote_sync_admitted_heads::DESCRIPTOR.required_caps,
        &["ryeos.execute.service.remote/sync-admitted-heads"]
    );
    assert!(!ryeos_api::handlers::ALL
        .iter()
        .any(|descriptor| descriptor.endpoint == "remote.import-admitted-root"));
}
