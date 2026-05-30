mod test_state;

use std::collections::HashMap;
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
    objects_closure_get, remote_import_admitted_head,
};
use ryeos_api::remote::config::{self, RemoteConfig};
use ryeos_app::state::AppState;
use ryeos_state::{CasEntryKind, CasEntryState};

fn store_subject(state: &AppState) -> String {
    let cas = lillux::cas::CasStore::new(state.state_store.cas_root().unwrap());
    cas.store_object(&json!({
        "kind": "chain_state",
        "schema": 1,
        "chain_root_id": "T-remote-import-e2e",
        "prev_chain_state_hash": null,
        "last_event_hash": null,
        "last_chain_seq": 0,
        "updated_at": "2026-05-30T00:00:00Z",
        "threads": {}
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
                |State(state): State<Arc<AppState>>, Json(body): Json<Value>| async move {
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
    config::save_remotes(&local.config.system_space_dir, &remotes).unwrap();
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
