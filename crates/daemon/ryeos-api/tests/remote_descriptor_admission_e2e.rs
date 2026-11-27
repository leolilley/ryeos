use ryeos_api::test_support as test_state;

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
use ryeos_api::remote::config::{self, PinSource, RemoteConfig, RemoteProjectBinding};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;
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

/// The project owner the mock remote reports for any caller.
const MOCK_REMOTE_OWNER: &str =
    "pid:v1:7777777777777777777777777777777777777777777777777777777777777777";

async fn start_mock_remote(public_key: Value) -> Result<(String, MockContacts)> {
    start_mock_remote_with_owner_route(public_key, true).await
}

async fn start_mock_remote_with_owner_route(
    public_key: Value,
    owner_route: bool,
) -> Result<(String, MockContacts)> {
    let state = MockRemote {
        public_key,
        claim_count: Arc::new(AtomicUsize::new(0)),
        signed_contact_count: Arc::new(AtomicUsize::new(0)),
    };
    let contacts = MockContacts {
        claim_count: state.claim_count.clone(),
        signed_contact_count: state.signed_contact_count.clone(),
    };
    let mut app = Router::new()
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
        );
    if owner_route {
        app = app.route(
            "/project/owner",
            post(|State(state): State<MockRemote>| async move {
                state.signed_contact_count.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "schema": 1,
                    "owner": MOCK_REMOTE_OWNER,
                    "via": { "kind": "solo" },
                }))
            }),
        );
    }
    let app = app.with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}"), contacts))
}

#[tokio::test]
async fn remote_configure_descriptor_match_writes_signed_pin() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    let (url, _claims) = start_mock_remote(public_key.clone()).await.unwrap();
    let descriptor_path = write_descriptor(&local_state, &remote_state, "prod", &url, &public_key);
    let context = operator_context(&local_state);
    let app_root = local_state.config.app_root.clone();

    let result = remote_configure::handle(
        remote_configure::Request {
            remote: None,
            url: None,
            descriptor: Some(descriptor_path),
            expect_fingerprint: None,
        },
        context,
        Arc::new(local_state),
    )
    .await
    .unwrap();

    assert_eq!(result["configured"], "prod");
    assert_eq!(result["descriptor_verified"], true);
    assert_eq!(result["pin_source"], "descriptor");
    let report = config::load_remotes_report(&app_root).unwrap();
    assert!(report.invalid.is_empty());
    assert_eq!(report.remotes["prod"].pin_source, PinSource::Descriptor);
    let record = std::fs::read_to_string(config::remote_pin_path(&app_root, "prod")).unwrap();
    assert!(record.starts_with("# ryeos:signed:"), "{record}");
}

#[tokio::test]
async fn remote_configure_descriptor_mismatch_does_not_write_config() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let wrong_state = remote_identity();
    let live_public_key = public_key_response(&remote_state);
    let pinned_public_key = public_key_response(&wrong_state);
    let (url, _claims) = start_mock_remote(live_public_key).await.unwrap();
    let descriptor_path =
        write_descriptor(&local_state, &wrong_state, "prod", &url, &pinned_public_key);
    let context = operator_context(&local_state);

    let app_root = local_state.config.app_root.clone();
    let result = remote_configure::handle(
        remote_configure::Request {
            remote: None,
            url: None,
            descriptor: Some(descriptor_path),
            expect_fingerprint: None,
        },
        context,
        Arc::new(local_state),
    )
    .await;

    assert!(result.is_err());
    let remotes = config::load_remotes(&app_root).unwrap();
    assert!(remotes.is_empty());
}

#[tokio::test]
async fn remote_configure_refuses_unsigned_v1_descriptor() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    let (url, _claims) = start_mock_remote(public_key.clone()).await.unwrap();
    let path = local_state.config.app_root.join("v1.remote.yaml");
    std::fs::write(
        &path,
        serde_yaml::to_string(&json!({
            "version": 1,
            "name": "prod",
            "url": url,
            "node": {
                "public_key": public_key["signing_key"].as_str().unwrap(),
                "fingerprint": public_key["fingerprint"].as_str().unwrap(),
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let context = operator_context(&local_state);
    let app_root = local_state.config.app_root.clone();

    let error = remote_configure::handle(
        remote_configure::Request {
            remote: None,
            url: None,
            descriptor: Some(path),
            expect_fingerprint: None,
        },
        context,
        Arc::new(local_state),
    )
    .await
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("remote_descriptor_unsigned"),
        "{error:#}"
    );
    assert!(config::load_remotes(&app_root).unwrap().is_empty());
}

#[tokio::test]
async fn remote_configure_refuses_descriptor_signed_by_another_key() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let forger = remote_identity();
    let public_key = public_key_response(&remote_state);
    let (url, _claims) = start_mock_remote(public_key.clone()).await.unwrap();
    // The descriptor pins the remote's key but the forger signed it.
    let path = write_descriptor_signed_by(&local_state, &forger, "prod", &url, &public_key);
    let context = operator_context(&local_state);
    let app_root = local_state.config.app_root.clone();

    let error = remote_configure::handle(
        remote_configure::Request {
            remote: None,
            url: None,
            descriptor: Some(path),
            expect_fingerprint: None,
        },
        context,
        Arc::new(local_state),
    )
    .await
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("remote_descriptor_wrong_signer"),
        "{error:#}"
    );
    assert!(config::load_remotes(&app_root).unwrap().is_empty());
}

#[tokio::test]
async fn remote_configure_requires_the_local_operator() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    let fingerprint = public_key["fingerprint"].as_str().unwrap().to_owned();
    let (url, contacts) = start_mock_remote(public_key).await.unwrap();
    let app_root = local_state.config.app_root.clone();

    let error = remote_configure::handle(
        remote_configure::Request {
            remote: Some("prod".to_owned()),
            url: Some(url),
            descriptor: None,
            expect_fingerprint: Some(fingerprint),
        },
        other_context(),
        Arc::new(local_state),
    )
    .await
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("configured local operator"),
        "{error:#}"
    );
    assert_eq!(contacts.claim_count.load(Ordering::SeqCst), 0);
    assert!(config::load_remotes(&app_root).unwrap().is_empty());
}

#[tokio::test]
async fn remote_configure_rejects_credential_url_before_contact_or_persistence() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let app_root = local_state.config.app_root.clone();
    let context = operator_context(&local_state);

    let error = remote_configure::handle(
        remote_configure::Request {
            remote: Some("unsafe".to_owned()),
            url: Some("https://user:do-not-retain@example.invalid".to_owned()),
            descriptor: None,
            expect_fingerprint: None,
        },
        context,
        Arc::new(local_state),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("must not contain credentials"));
    assert!(!error.to_string().contains("do-not-retain"));
    assert!(config::load_remotes(&app_root).unwrap().is_empty());
}

#[tokio::test]
async fn remote_configure_normalizes_direct_url_and_stores_pin_source() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    // A direct URL needs an out-of-band key pin (no trust on first use).
    let fingerprint = public_key["fingerprint"].as_str().unwrap().to_owned();
    let (url, _contacts) = start_mock_remote(public_key).await.unwrap();
    let app_root = local_state.config.app_root.clone();
    let context = operator_context(&local_state);

    let result = remote_configure::handle(
        remote_configure::Request {
            remote: Some("normalized".to_owned()),
            url: Some(format!("{url}/")),
            descriptor: None,
            expect_fingerprint: Some(fingerprint),
        },
        context,
        Arc::new(local_state),
    )
    .await
    .unwrap();

    assert_eq!(result["url"], url);
    let report = config::load_remotes_report(&app_root).unwrap();
    assert_eq!(
        report.remotes["normalized"].config.url.as_deref(),
        Some(url.as_str())
    );
    assert_eq!(
        report.remotes["normalized"].pin_source,
        PinSource::ExpectedFingerprint
    );
}

#[tokio::test]
async fn remote_admit_refuses_to_send_token_on_live_identity_mismatch() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let pinned_state = remote_identity();
    let live_state = remote_identity();
    let live_public_key = public_key_response(&live_state);
    let (url, contacts) = start_mock_remote(live_public_key).await.unwrap();

    let pinned_public_key = public_key_response(&pinned_state);
    install_remote(
        &local_state,
        &remote_config("prod", &url, &pinned_public_key),
    );

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
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let live_public_key = public_key_response(&remote_state);
    let (url, contacts) = start_mock_remote(live_public_key.clone()).await.unwrap();

    let mut stale = remote_config("prod", &url, &live_public_key);
    stale.vault_fingerprint = "vault-stale-fingerprint".to_string();
    install_remote(&local_state, &stale);

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
async fn remote_admit_refuses_an_edited_pin_before_releasing_token() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let operator_state = remote_identity();
    let attacker_state = remote_identity();
    let operator_public_key = public_key_response(&operator_state);
    let attacker_public_key = public_key_response(&attacker_state);
    let (url, contacts) = start_mock_remote(attacker_public_key.clone())
        .await
        .unwrap();
    install_remote(
        &local_state,
        &remote_config("prod", &url, &operator_public_key),
    );

    // Swap the pinned key in place without re-signing.
    let path = config::remote_pin_path(&local_state.config.app_root, "prod");
    let record = std::fs::read_to_string(&path).unwrap();
    let edited = record.replace(
        operator_public_key["signing_key"].as_str().unwrap(),
        attacker_public_key["signing_key"].as_str().unwrap(),
    );
    assert_ne!(record, edited);
    std::fs::write(&path, edited).unwrap();

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

/// Gives the checkout at `root` a `.ai/manifest.yaml` declaring `name`.
fn declare_project(root: &std::path::Path, name: &str) {
    std::fs::create_dir_all(root.join(".ai")).unwrap();
    std::fs::write(root.join(".ai/manifest.yaml"), format!("name: {name}\n")).unwrap();
}

#[tokio::test]
async fn remote_bind_project_records_binding_in_the_signed_pin() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    let project_root = tempfile::tempdir().unwrap();
    let project_root = project_root.path().canonicalize().unwrap();
    declare_project(&project_root, "bound-project");
    let (url, contacts) = start_mock_remote(public_key.clone()).await.unwrap();
    let remote = remote_config("prod", &url, &public_key);
    install_remote(&local_state, &remote);
    store_operator_delegation(&local_state, &remote);
    let before = config::load_remotes_report(&local_state.config.app_root)
        .unwrap()
        .remotes["prod"]
        .pinned_at
        .clone();
    let context = operator_context(&local_state);

    let result = remote_bind_project::handle(
        remote_bind_project::Request {
            remote: "prod".to_string(),
            project: project_root.clone(),
            remote_project: "/srv/project".to_string(),
            sync_scope: ProjectSyncScope::FullProject,
        },
        context,
        Arc::new(local_state.clone()),
    )
    .await
    .unwrap();

    assert_eq!(result["scope"], "operator");
    assert_eq!(result["remote_project_path"], "/srv/project");
    assert_eq!(result["remote_owner"], MOCK_REMOTE_OWNER);
    assert_eq!(contacts.signed_contact_count.load(Ordering::SeqCst), 1);

    let report = config::load_remotes_report(&local_state.config.app_root).unwrap();
    assert!(report.invalid.is_empty());
    let operator_remote = &report.remotes["prod"];
    assert_eq!(operator_remote.pinned_at, before);
    assert_eq!(operator_remote.config.url.as_deref(), Some(url.as_str()));
    let local_key = project_root.to_string_lossy().to_string();
    let binding = operator_remote
        .config
        .project_bindings
        .get(&local_key)
        .unwrap();
    assert_eq!(binding.remote_project_path, "/srv/project");
    assert_eq!(binding.project_name.as_str(), "bound-project");
    assert_eq!(binding.remote_owner.to_string(), MOCK_REMOTE_OWNER);
}

#[tokio::test]
async fn remote_bind_project_refuses_a_remote_that_reports_no_owner() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    let project_root = tempfile::tempdir().unwrap();
    let project_root = project_root.path().canonicalize().unwrap();
    declare_project(&project_root, "bound-project");
    let (url, _contacts) = start_mock_remote_with_owner_route(public_key.clone(), false)
        .await
        .unwrap();
    let remote = remote_config("prod", &url, &public_key);
    install_remote(&local_state, &remote);
    store_operator_delegation(&local_state, &remote);

    let error = remote_bind_project::handle(
        remote_bind_project::Request {
            remote: "prod".to_string(),
            project: project_root.clone(),
            remote_project: "/srv/project".to_string(),
            sync_scope: ProjectSyncScope::FullProject,
        },
        operator_context(&local_state),
        Arc::new(local_state.clone()),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("remote_owner_unreported"),
        "{error:#}"
    );
    assert!(
        config::load_remotes(&local_state.config.app_root).unwrap()["prod"]
            .project_bindings
            .is_empty()
    );
}

#[tokio::test]
async fn remote_bind_project_refuses_an_unpinned_remote_and_a_non_operator() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    let project_root = tempfile::tempdir().unwrap();
    let project_root = project_root.path().canonicalize().unwrap();
    declare_project(&project_root, "bound-project");
    let request = || remote_bind_project::Request {
        remote: "prod".to_string(),
        project: project_root.clone(),
        remote_project: "/srv/project".to_string(),
        sync_scope: ProjectSyncScope::FullProject,
    };

    let error = remote_bind_project::handle(
        request(),
        operator_context(&local_state),
        Arc::new(local_state.clone()),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("not found"), "{error:#}");

    install_remote(
        &local_state,
        &remote_config("prod", "https://project.example.com", &public_key),
    );
    let error =
        remote_bind_project::handle(request(), other_context(), Arc::new(local_state.clone()))
            .await
            .unwrap_err();
    assert!(
        format!("{error:#}").contains("configured local operator"),
        "{error:#}"
    );
    assert!(
        config::load_remotes(&local_state.config.app_root).unwrap()["prod"]
            .project_bindings
            .is_empty()
    );
}

#[tokio::test]
async fn remote_list_retains_configured_site_coordinate() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    install_remote(
        &local_state,
        &remote_config("prod", "https://target.example", &public_key),
    );

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
    assert_eq!(result["remotes"][0]["pin_source"], "expected_fingerprint");
}

#[tokio::test]
async fn remote_doctor_rejects_stale_configured_site_coordinate() {
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    let (url, contacts) = start_mock_remote(public_key).await.unwrap();
    let project = tempfile::tempdir().unwrap();
    declare_project(project.path(), "stale-remote-project");
    let project_path = project.path().canonicalize().unwrap();
    let mut stale = remote_config("prod", &url, &public_key_response(&remote_state));
    stale.site_id = "site:stale-coordinate".to_string();
    stale.project_bindings.insert(
        project_path.to_string_lossy().to_string(),
        RemoteProjectBinding {
            project_name: ryeos_state::project_identity::ProjectName::new("stale-remote-project")
                .unwrap(),
            remote_owner: format!("pid:v1:{}", "a".repeat(64)).parse().unwrap(),
            remote_project_path: "/srv/project".to_string(),
            sync_scope: ProjectSyncScope::FullProject,
        },
    );
    install_remote(&local_state, &stale);
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
    let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
    let remote_state = remote_identity();
    let public_key = public_key_response(&remote_state);
    let (url, contacts) = start_mock_remote(public_key.clone()).await.unwrap();
    let mut stale = remote_config("prod", &url, &public_key);
    stale.vault_fingerprint = "vault-stale-fingerprint".to_string();
    install_remote(&local_state, &stale);
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

/// The signed identity of a mock remote node. These tests talk to a mock
/// remote, so a remote needs only its node identity, never a node Store; the
/// test process keeps the one Store of its local node.
struct RemoteIdentity {
    identity: ryeos_node_runtime::identity::NodeIdentity,
    _directory: tempfile::TempDir,
}

fn remote_identity() -> RemoteIdentity {
    let directory = tempfile::tempdir().unwrap();
    let identity =
        ryeos_node_runtime::identity::NodeIdentity::create(&directory.path().join("node-key.pem"))
            .unwrap();
    RemoteIdentity {
        identity,
        _directory: directory,
    }
}

fn public_key_response(remote: &RemoteIdentity) -> Value {
    let signing_key = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD
            .encode(remote.identity.verifying_key().as_bytes())
    );
    json!({
        "principal_id": remote.identity.principal_id(),
        "fingerprint": remote.identity.fingerprint(),
        "signing_key": signing_key,
        "vault_fingerprint": "vault-test-fingerprint",
        "site_id": remote.identity.site_id(),
    })
}

/// The configured local operator's request context.
fn operator_context(state: &AppState) -> HandlerContext {
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    HandlerContext::new_with_authority(
        operator.principal_id(),
        vec!["*".into()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

/// Store the operator's delegation to `remote`, signed by the operator key, so
/// the operator's outbound calls to it are made as the operator.
fn store_operator_delegation(state: &AppState, remote: &RemoteConfig) {
    use ryeos_node_runtime::remote_delegation::{DelegationBody, DelegationParties};
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    let body = DelegationBody::new(
        &operator.principal_id(),
        DelegationParties {
            delegate_site: state.threads.site_id(),
            delegate_node: &state.identity.principal_id(),
            audience_site: &remote.site_id,
            audience_node: &remote.principal_id,
        },
        &[
            "ryeos.execute.service.remote/bind-project".to_owned(),
            "ryeos.execute.service.project/owner".to_owned(),
        ],
        lillux::time::unix_secs_now() + 3600,
    );
    let signed =
        ryeos_node_runtime::remote_delegation::SignedDelegation::sign(body, operator.signing_key())
            .unwrap();
    ryeos_node_runtime::remote_delegation::store_delegation(
        &ryeos_engine::roots::RuntimeRoot::new(state.config.app_root.clone()).delegations_dir(),
        &remote.name,
        &signed,
        &state.identity,
        lillux::time::unix_secs_now(),
    )
    .unwrap();
}

/// A verified local client that is not the configured local operator.
fn other_context() -> HandlerContext {
    HandlerContext::new_with_authority(
        "fp:not-the-operator".to_string(),
        vec!["*".into()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

/// Pin a remote as `remote configure` would, signed by the local node key.
fn install_remote(state: &AppState, config: &RemoteConfig) {
    config::write_remote_pin(
        &state.config.app_root,
        state.identity.signing_key(),
        config,
        PinSource::ExpectedFingerprint,
        None,
    )
    .unwrap();
}

/// Write the descriptor of `public_key` signed by `signer`'s node key.
fn write_descriptor_signed_by(
    state: &AppState,
    signer: &RemoteIdentity,
    name: &str,
    url: &str,
    public_key: &Value,
) -> std::path::PathBuf {
    let path = state.config.app_root.join(format!("{name}.remote.yaml"));
    let descriptor = config::RemoteDescriptor {
        version: 2,
        name: name.to_string(),
        url: Some(url.to_string()),
        ingest_ignore: None,
        site_id: public_key["site_id"].as_str().unwrap().to_string(),
        vault_fingerprint: public_key["vault_fingerprint"]
            .as_str()
            .unwrap()
            .to_string(),
        node: config::RemoteDescriptorNode {
            public_key: public_key["signing_key"].as_str().unwrap().to_string(),
            fingerprint: public_key["fingerprint"].as_str().unwrap().to_string(),
        },
        capabilities: vec![],
        admission: None,
        provider: None,
    };
    std::fs::write(
        &path,
        descriptor
            .render_signed(signer.identity.signing_key())
            .unwrap(),
    )
    .unwrap();
    path
}

/// A descriptor of `public_key`, signed by the remote's own node key.
fn write_descriptor(
    state: &AppState,
    remote: &RemoteIdentity,
    name: &str,
    url: &str,
    public_key: &Value,
) -> std::path::PathBuf {
    write_descriptor_signed_by(state, remote, name, url, public_key)
}

fn remote_config(name: &str, url: &str, public_key: &Value) -> RemoteConfig {
    RemoteConfig {
        name: name.to_string(),
        url: Some(url.to_string()),
        reachability: Default::default(),
        principal_id: public_key["principal_id"].as_str().unwrap().to_string(),
        signing_key: public_key["signing_key"].as_str().unwrap().to_string(),
        site_id: public_key["site_id"].as_str().unwrap().to_string(),
        vault_fingerprint: public_key["vault_fingerprint"]
            .as_str()
            .unwrap()
            .to_string(),
        ingest_ignore: Some(ryeos_app::ignore::IgnoreConfig { patterns: vec![] }),
        project_bindings: HashMap::new(),
    }
}

/// A peer's public-key document that lacks a field the pin binds fails
/// `remote configure` loudly, naming the field, and writes no pin. A shaped
/// answer that drops a protocol-required field must never be read as a
/// default.
#[tokio::test]
async fn remote_configure_refuses_a_public_key_document_missing_a_required_field() {
    for field in [
        "principal_id",
        "fingerprint",
        "signing_key",
        "vault_fingerprint",
        "site_id",
    ] {
        let (_local_tmp, _local_state_owner, local_state) = test_state::build_test_state();
        let remote_state = remote_identity();
        let mut public_key = public_key_response(&remote_state);
        let fingerprint = public_key["fingerprint"].as_str().unwrap().to_owned();
        public_key.as_object_mut().unwrap().remove(field);
        let (url, _contacts) = start_mock_remote(public_key).await.unwrap();
        let app_root = local_state.config.app_root.clone();
        let context = operator_context(&local_state);

        let error = remote_configure::handle(
            remote_configure::Request {
                remote: Some("short".to_owned()),
                url: Some(url),
                descriptor: None,
                expect_fingerprint: Some(fingerprint),
            },
            context,
            Arc::new(local_state),
        )
        .await
        .expect_err(field);

        assert!(
            format!("{error:?}").contains(field),
            "the refusal for a missing {field} must name it: {error:?}"
        );
        assert!(
            config::load_remotes(&app_root).unwrap().is_empty(),
            "{field}"
        );
    }
}
