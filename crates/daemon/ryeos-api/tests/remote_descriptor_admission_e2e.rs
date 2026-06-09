mod test_state;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use ryeos_api::handlers::{remote_admit, remote_bind_project, remote_configure};
use ryeos_api::remote::config::{self, RemoteConfig};
use ryeos_app::state::AppState;
use ryeos_state::project_sync::ProjectSyncScope;

#[derive(Clone)]
struct MockRemote {
    public_key: Value,
    claim_count: Arc<AtomicUsize>,
}

async fn start_mock_remote(public_key: Value) -> Result<(String, Arc<AtomicUsize>)> {
    let state = MockRemote {
        public_key,
        claim_count: Arc::new(AtomicUsize::new(0)),
    };
    let claim_count = state.claim_count.clone();
    let app = Router::new()
        .route(
            "/public-key",
            get(|State(state): State<MockRemote>| async move { Json(state.public_key) }),
        )
        .route(
            "/ingest-ignore",
            get(|| async { Json(json!({ "patterns": [] })) }),
        )
        .route(
            "/admission/claim",
            post(
                |State(state): State<MockRemote>, Json(_body): Json<Value>| async move {
                    state.claim_count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "admitted": true,
                        "fingerprint": "mock-fingerprint",
                        "label": "mock-label",
                        "scopes": [],
                        "granted_by": "mock",
                        "created_at": "2026-05-31T00:00:00Z"
                    }))
                },
            ),
        )
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}"), claim_count))
}

#[tokio::test]
async fn remote_configure_descriptor_match_writes_config() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let public_key = public_key_response(&remote_state);
    let (url, _claims) = start_mock_remote(public_key.clone()).await.unwrap();
    let descriptor_path = write_descriptor(&local_state, "prod", &url, &public_key);

    let result = remote_configure::handle(
        remote_configure::Request {
            remote: None,
            url: None,
            descriptor: Some(descriptor_path),
        },
        Arc::new(local_state),
    )
    .await
    .unwrap();

    assert_eq!(result["configured"], "prod");
    assert_eq!(result["descriptor_verified"], true);
}

#[tokio::test]
async fn remote_configure_descriptor_mismatch_does_not_write_config() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let (_wrong_tmp, wrong_state) = test_state::build_test_state();
    let live_public_key = public_key_response(&remote_state);
    let pinned_public_key = public_key_response(&wrong_state);
    let (url, _claims) = start_mock_remote(live_public_key).await.unwrap();
    let descriptor_path = write_descriptor(&local_state, "prod", &url, &pinned_public_key);

    let system_space_dir = local_state.config.system_space_dir.clone();
    let result = remote_configure::handle(
        remote_configure::Request {
            remote: None,
            url: None,
            descriptor: Some(descriptor_path),
        },
        Arc::new(local_state),
    )
    .await;

    assert!(result.is_err());
    let remotes = config::load_remotes(&system_space_dir).unwrap();
    assert!(remotes.is_empty());
}

#[tokio::test]
async fn remote_admit_refuses_to_send_token_on_live_identity_mismatch() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_pinned_tmp, pinned_state) = test_state::build_test_state();
    let (_live_tmp, live_state) = test_state::build_test_state();
    let live_public_key = public_key_response(&live_state);
    let (url, claims) = start_mock_remote(live_public_key).await.unwrap();

    let pinned_public_key = public_key_response(&pinned_state);
    let mut remotes = HashMap::new();
    remotes.insert(
        "prod".to_string(),
        remote_config("prod", &url, &pinned_public_key),
    );
    config::save_remotes(&local_state.config.system_space_dir, &remotes).unwrap();

    let result = remote_admit::handle(
        remote_admit::Request {
            remote: "prod".to_string(),
            token: "secret-token".to_string(),
            label: Some("dev-machine".to_string()),
            scopes: vec!["ryeos.execute.service.objects.has".to_string()],
            project_path: None,
            no_project: true,
        },
        Arc::new(local_state),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(claims.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn remote_bind_project_copies_project_only_remote_to_user_config() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let public_key = public_key_response(&remote_state);
    let project_root = tempfile::tempdir().unwrap();
    let project_root = project_root.path().canonicalize().unwrap();

    let mut project_remotes = HashMap::new();
    project_remotes.insert(
        "prod".to_string(),
        remote_config("prod", "https://project.example.com", &public_key),
    );
    config::save_remotes(&project_root, &project_remotes).unwrap();

    let result = remote_bind_project::handle(
        remote_bind_project::Request {
            remote: "prod".to_string(),
            project: project_root.clone(),
            remote_project: "/srv/project".to_string(),
            sync_scope: ProjectSyncScope::FullProject,
        },
        Arc::new(local_state.clone()),
    )
    .await
    .unwrap();

    assert_eq!(result["scope"], "user");
    assert_eq!(result["remote_project_path"], "/srv/project");

    let user_remotes = config::load_remotes(&local_state.config.system_space_dir).unwrap();
    let user_remote = user_remotes.get("prod").unwrap();
    assert_eq!(user_remote.url, "https://project.example.com");
    let local_key = project_root.to_string_lossy().to_string();
    assert_eq!(
        user_remote
            .project_bindings
            .get(&local_key)
            .unwrap()
            .remote_project_path,
        "/srv/project"
    );

    let project_remotes_after = config::load_remotes(&project_root).unwrap();
    assert!(project_remotes_after["prod"].project_bindings.is_empty());
}

fn public_key_response(state: &AppState) -> Value {
    let signing_key = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(state.identity.verifying_key().as_bytes())
    );
    json!({
        "principal_id": state.identity.principal_id(),
        "fingerprint": state.identity.fingerprint(),
        "signing_key": signing_key,
        "vault_fingerprint": "vault-test-fingerprint",
        "site_id": state.threads.site_id().to_string(),
    })
}

fn write_descriptor(
    state: &AppState,
    name: &str,
    url: &str,
    public_key: &Value,
) -> std::path::PathBuf {
    let path = state
        .config
        .system_space_dir
        .join(format!("{name}.remote.yaml"));
    let body = serde_yaml::to_string(&json!({
        "version": 1,
        "name": name,
        "url": url,
        "node": {
            "public_key": public_key["signing_key"].as_str().unwrap(),
            "fingerprint": public_key["fingerprint"].as_str().unwrap(),
        }
    }))
    .unwrap();
    std::fs::write(&path, body).unwrap();
    path
}

fn remote_config(name: &str, url: &str, public_key: &Value) -> RemoteConfig {
    RemoteConfig {
        name: name.to_string(),
        url: url.to_string(),
        principal_id: public_key["principal_id"].as_str().unwrap().to_string(),
        signing_key: public_key["signing_key"].as_str().unwrap().to_string(),
        site_id: public_key["site_id"].as_str().unwrap().to_string(),
        vault_fingerprint: public_key["vault_fingerprint"]
            .as_str()
            .unwrap()
            .to_string(),
        ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
        project_bindings: HashMap::new(),
    }
}
