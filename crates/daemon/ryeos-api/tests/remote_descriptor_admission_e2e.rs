mod test_state;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use ryeos_api::handlers::{
    remote_admit, remote_bind_project, remote_configure, remote_doctor, remote_list, remote_status,
};
use ryeos_api::remote::config::{self, RemoteConfig, RemoteProjectBinding};
use ryeos_app::state::AppState;
use ryeos_state::project_sync::ProjectSyncScope;

#[derive(Clone)]
struct MockRemote {
    public_key: Value,
    claim_count: Arc<AtomicUsize>,
    signed_contact_count: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct MockContacts {
    claim_count: Arc<AtomicUsize>,
    signed_contact_count: Arc<AtomicUsize>,
}

async fn start_mock_remote(public_key: Value) -> Result<(String, MockContacts)> {
    let state = MockRemote {
        public_key,
        claim_count: Arc::new(AtomicUsize::new(0)),
        signed_contact_count: Arc::new(AtomicUsize::new(0)),
    };
    let contacts = MockContacts {
        claim_count: state.claim_count.clone(),
        signed_contact_count: state.signed_contact_count.clone(),
    };
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
            "/health",
            get(|| async { Json(json!({ "status": "healthy" })) }),
        )
        .route(
            "/threads",
            get(|State(state): State<MockRemote>| async move {
                state.signed_contact_count.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "threads": [] }))
            }),
        )
        .route(
            "/project/status",
            post(|State(state): State<MockRemote>| async move {
                state.signed_contact_count.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "status": "found" }))
            }),
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
    Ok((format!("http://{addr}"), contacts))
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

    let app_root = local_state.config.app_root.clone();
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
    let remotes = config::load_remotes(&app_root).unwrap();
    assert!(remotes.is_empty());
}

#[tokio::test]
async fn remote_configure_rejects_credential_url_before_contact_or_persistence() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let app_root = local_state.config.app_root.clone();

    let error = remote_configure::handle(
        remote_configure::Request {
            remote: Some("unsafe".to_owned()),
            url: Some("https://user:do-not-retain@example.invalid".to_owned()),
            descriptor: None,
        },
        Arc::new(local_state),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("must not contain credentials"));
    assert!(!error.to_string().contains("do-not-retain"));
    assert!(config::load_remotes(&app_root).unwrap().is_empty());
}

#[tokio::test]
async fn remote_configure_normalizes_direct_url_before_persistence() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let public_key = public_key_response(&remote_state);
    let (url, _contacts) = start_mock_remote(public_key).await.unwrap();
    let app_root = local_state.config.app_root.clone();

    let result = remote_configure::handle(
        remote_configure::Request {
            remote: Some("normalized".to_owned()),
            url: Some(format!("{url}/")),
            descriptor: None,
        },
        Arc::new(local_state),
    )
    .await
    .unwrap();

    assert_eq!(result["url"], url);
    assert_eq!(
        config::load_remotes(&app_root).unwrap()["normalized"].url,
        url
    );
}

#[tokio::test]
async fn remote_admit_refuses_to_send_token_on_live_identity_mismatch() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_pinned_tmp, pinned_state) = test_state::build_test_state();
    let (_live_tmp, live_state) = test_state::build_test_state();
    let live_public_key = public_key_response(&live_state);
    let (url, contacts) = start_mock_remote(live_public_key).await.unwrap();

    let pinned_public_key = public_key_response(&pinned_state);
    let mut remotes = HashMap::new();
    remotes.insert(
        "prod".to_string(),
        remote_config("prod", &url, &pinned_public_key),
    );
    config::save_remotes(&local_state.config.app_root, &remotes).unwrap();

    let result = remote_admit::handle(
        remote_admit::Request {
            remote: "prod".to_string(),
            token: "secret-token".to_string(),
            label: Some("dev-machine".to_string()),
            scopes: vec!["ryeos.execute.service.objects/has".to_string()],
        },
        Arc::new(local_state),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(contacts.claim_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn remote_admit_refuses_to_send_token_on_live_vault_mismatch() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let live_public_key = public_key_response(&remote_state);
    let (url, contacts) = start_mock_remote(live_public_key.clone()).await.unwrap();

    let mut stale = remote_config("prod", &url, &live_public_key);
    stale.vault_fingerprint = "vault-stale-fingerprint".to_string();
    let mut remotes = HashMap::new();
    remotes.insert("prod".to_string(), stale);
    config::save_remotes(&local_state.config.app_root, &remotes).unwrap();

    let result = remote_admit::handle(
        remote_admit::Request {
            remote: "prod".to_string(),
            token: "secret-token".to_string(),
            label: Some("dev-machine".to_string()),
            scopes: vec!["ryeos.execute.service.objects/has".to_string()],
        },
        Arc::new(local_state),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(contacts.claim_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn remote_admit_ignores_project_shadow_before_releasing_token() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_operator_tmp, operator_state) = test_state::build_test_state();
    let (_attacker_tmp, attacker_state) = test_state::build_test_state();
    let operator_public_key = public_key_response(&operator_state);
    let attacker_public_key = public_key_response(&attacker_state);
    let (operator_url, operator_contacts) = start_mock_remote(operator_public_key.clone())
        .await
        .unwrap();
    let (attacker_url, attacker_contacts) = start_mock_remote(attacker_public_key.clone())
        .await
        .unwrap();

    let mut operator_remotes = HashMap::new();
    operator_remotes.insert(
        "prod".to_string(),
        remote_config("prod", &operator_url, &operator_public_key),
    );
    config::save_remotes(&local_state.config.app_root, &operator_remotes).unwrap();

    let project_root = tempfile::tempdir().unwrap();
    let mut project_remotes = HashMap::new();
    project_remotes.insert(
        "prod".to_string(),
        remote_config("prod", &attacker_url, &attacker_public_key),
    );
    config::save_remotes(project_root.path(), &project_remotes).unwrap();

    let result = remote_admit::handle(
        remote_admit::Request {
            remote: "prod".to_string(),
            token: "secret-token".to_string(),
            label: Some("dev-machine".to_string()),
            scopes: vec!["ryeos.execute.service.objects/has".to_string()],
        },
        Arc::new(local_state),
    )
    .await
    .unwrap();

    assert_eq!(result["url"], operator_url);
    assert_eq!(operator_contacts.claim_count.load(Ordering::SeqCst), 1);
    assert_eq!(attacker_contacts.claim_count.load(Ordering::SeqCst), 0);
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

    assert_eq!(result["scope"], "operator");
    assert_eq!(result["remote_project_path"], "/srv/project");

    let operator_remotes = config::load_remotes(&local_state.config.app_root).unwrap();
    let operator_remote = operator_remotes.get("prod").unwrap();
    assert_eq!(operator_remote.url, "https://project.example.com");
    let local_key = project_root.to_string_lossy().to_string();
    assert_eq!(
        operator_remote
            .project_bindings
            .get(&local_key)
            .unwrap()
            .remote_project_path,
        "/srv/project"
    );

    let project_remotes_after = config::load_remotes(&project_root).unwrap();
    assert!(project_remotes_after["prod"].project_bindings.is_empty());
}

#[tokio::test]
async fn remote_list_retains_configured_site_coordinate() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let public_key = public_key_response(&remote_state);
    let mut remotes = HashMap::new();
    remotes.insert(
        "prod".to_string(),
        remote_config("prod", "https://target.example", &public_key),
    );
    config::save_remotes(&local_state.config.app_root, &remotes).unwrap();

    let result = remote_list::handle(
        remote_list::Request {
            project_path: None,
            no_project: true,
        },
        Arc::new(local_state),
    )
    .await
    .unwrap();

    assert_eq!(result["remotes"][0]["site_id"], public_key["site_id"]);
}

#[tokio::test]
async fn remote_doctor_rejects_stale_configured_site_coordinate() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let public_key = public_key_response(&remote_state);
    let (url, contacts) = start_mock_remote(public_key).await.unwrap();
    let project = tempfile::tempdir().unwrap();
    let project_path = project.path().canonicalize().unwrap();
    let mut stale = remote_config("prod", &url, &public_key_response(&remote_state));
    stale.site_id = "site:stale-coordinate".to_string();
    stale.project_bindings.insert(
        project_path.to_string_lossy().to_string(),
        RemoteProjectBinding {
            remote_project_path: "/srv/project".to_string(),
            sync_scope: ProjectSyncScope::FullProject,
        },
    );
    let mut remotes = HashMap::new();
    remotes.insert("prod".to_string(), stale);
    config::save_remotes(&local_state.config.app_root, &remotes).unwrap();
    let local_state = Arc::new(local_state);

    let result = remote_doctor::handle(
        remote_doctor::Request {
            remote: "prod".to_string(),
            project: Some(project_path.clone()),
        },
        local_state.clone(),
    )
    .await
    .unwrap();
    let identity = result["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "remote_identity")
        .unwrap();

    assert_eq!(identity["ok"], false);
    assert_eq!(identity["configured_site_matches"], false);
    assert_eq!(identity["pinned_identity_matches"], false);
    assert_eq!(result["auth"]["signed_probe"], "skipped_identity_mismatch");

    let status = remote_status::handle(
        remote_status::Request {
            remote: "prod".to_string(),
            project_path: Some(project_path),
            no_project: false,
        },
        local_state,
    )
    .await
    .unwrap();
    assert_eq!(status["remote"]["configured_site_matches"], false);
    assert_eq!(status["remote"]["pinned_identity_matches"], false);
    assert_eq!(status["auth"]["signed_probe"], "skipped_identity_mismatch");
    assert_eq!(contacts.signed_contact_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn remote_doctor_and_status_reject_stale_configured_vault_without_signed_contact() {
    let (_local_tmp, local_state) = test_state::build_test_state();
    let (_remote_tmp, remote_state) = test_state::build_test_state();
    let public_key = public_key_response(&remote_state);
    let (url, contacts) = start_mock_remote(public_key.clone()).await.unwrap();
    let mut stale = remote_config("prod", &url, &public_key);
    stale.vault_fingerprint = "vault-stale-fingerprint".to_string();
    let mut remotes = HashMap::new();
    remotes.insert("prod".to_string(), stale);
    config::save_remotes(&local_state.config.app_root, &remotes).unwrap();
    let local_state = Arc::new(local_state);

    let result = remote_doctor::handle(
        remote_doctor::Request {
            remote: "prod".to_string(),
            project: None,
        },
        local_state.clone(),
    )
    .await
    .unwrap();
    let identity = result["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "remote_identity")
        .unwrap();
    assert_eq!(identity["configured_vault_matches"], false);
    assert_eq!(identity["pinned_identity_matches"], false);
    assert_eq!(result["auth"]["signed_probe"], "skipped_identity_mismatch");

    let status = remote_status::handle(
        remote_status::Request {
            remote: "prod".to_string(),
            project_path: None,
            no_project: true,
        },
        local_state,
    )
    .await
    .unwrap();
    assert_eq!(status["remote"]["configured_vault_matches"], false);
    assert_eq!(status["remote"]["pinned_identity_matches"], false);
    assert_eq!(status["auth"]["signed_probe"], "skipped_identity_mismatch");
    assert_eq!(contacts.signed_contact_count.load(Ordering::SeqCst), 0);
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
    let path = state.config.app_root.join(format!("{name}.remote.yaml"));
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
