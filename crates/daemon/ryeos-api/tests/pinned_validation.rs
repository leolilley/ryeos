mod test_state;

use std::sync::Arc;

use axum::http::StatusCode;
use ryeos_api::routes::compile::{CompiledLimits, CompiledRoute, RouteDispatchContext};
use ryeos_api::routes::invocation::RoutePrincipal;
use ryeos_api::routes::response_modes::execute_mode::CompiledExecuteMode;
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use ryeos_state::objects::{ProjectFile, ProjectSnapshot, ProjectTree};
use ryeos_state::project_sync::{PROJECT_SNAPSHOT_CONFIG_RELATIVE, ProjectSyncScope};
use serde_json::{Value, json};

// Exercise the real execute response mode and retained bundle engine. The
// subject is an installed service so a successful validation can be clearly
// distinguished from invoking its handler (which returns health, not schema).
#[tokio::test]
async fn current_head_validation_uses_pinned_authority_without_invoking_or_publishing() {
    let (_node, state) = test_state::build_test_state_with_bundles();
    let project = tempfile::tempdir().unwrap();
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let mut principal = RoutePrincipal::anonymous(operator.principal_id(), "ryeos_signed");
    principal.verified = true;
    principal.authorized_key_class = Some(AuthorizedKeyPrincipalClass::LocalClient);
    principal.scopes = vec!["ryeos.execute.service.health/status".to_owned()];
    let principal_key = ryeos_state::refs::principal_storage_key(&principal.id)
        .unwrap()
        .to_owned();
    let project_hash = ryeos_state::refs::deployed_project_key(project.path().to_str().unwrap());
    let config_path = project.path().join(PROJECT_SNAPSHOT_CONFIG_RELATIVE);
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let config = b"schema: 1\nexclusions: []\n";
    std::fs::write(&config_path, config).unwrap();
    let policy = ryeos_state::project_sync::capture_snapshot_policy(
        project.path(),
        &state.ignore_matcher,
        ProjectSyncScope::FullProject,
    )
    .unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let cas = authority.cas_store().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let file = ProjectFile {
        blob_hash: cas.store_blob(config).unwrap(),
        size: config.len() as u64,
        normalized_mode: 0o644,
    };
    let tree = ProjectTree {
        files: [(
            PROJECT_SNAPSHOT_CONFIG_RELATIVE.to_owned(),
            cas.store_object(&file.to_value()).unwrap(),
        )]
        .into(),
    };
    let snapshot = ProjectSnapshot {
        project_tree_hash: cas.store_object(&tree.to_value()).unwrap(),
        effective_policy_hash: cas.store_object(&policy.to_value()).unwrap(),
        parent_hashes: Vec::new(),
        message: None,
        created_at: lillux::time::iso8601_now(),
        source: "test".to_owned(),
    };
    let head = cas.store_object(&snapshot.to_value()).unwrap();
    state
        .state_store
        .write_project_head_ref(
            &principal_key,
            &project_hash,
            &head,
            &ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity),
            &guard,
        )
        .unwrap();
    drop(guard);

    // Live source drift must not replace the exact generation selected above.
    std::fs::write(&config_path, "not valid snapshot policy").unwrap();
    let raw = serde_json::from_value(json!({
        "id": "core/execute", "path": "/execute", "methods": ["POST"],
        "auth": "ryeos_signed", "response": {"mode": "execute"},
        "request": {"body": "json"}
    }))
    .unwrap();
    let route = CompiledRoute {
        id: "core/execute".to_owned(),
        source_file: "test/execute.yaml".into(),
        path_pattern: "/execute".to_owned(),
        methods: vec![axum::http::Method::POST],
        auth_invoker: ryeos_api::routes::invokers::compile_auth_invoker(
            "ryeos_signed",
            None,
            "core/execute",
        )
        .unwrap(),
        limits: CompiledLimits {
            body_bytes_max: 4096,
            timeout_ms: 30_000,
            concurrent_max: 1,
        },
        response_mode: Arc::new(CompiledExecuteMode),
        raw_response: raw,
        semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
    };
    let (request_parts, _) = axum::http::Request::builder()
        .method("POST")
        .uri("/execute")
        .body(())
        .unwrap()
        .into_parts();
    let response = route.response_mode.handle(&route, RouteDispatchContext {
        captures: Default::default(),
        request_parts,
        body_raw: serde_json::to_vec(&json!({
            "item_ref": "service:health/status",
            "ref_bindings": {},
            "project_path": project.path(),
            "execution_policy": ExecutionPolicy::local_pinned_current_head(ExecutionResponse::Wait).exclude_operator_vault(),
            "validate_only": true,
            "parameters": {},
        })).unwrap(),
        principal,
        state: state.clone(),
        launch_timings: None,
        webhook_dedupe: Arc::new(ryeos_api::routes::webhook_dedupe::WebhookDedupeStore::new()),
    }).await.unwrap();
    let status = response.status();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{body:#}");
    assert_eq!(body["validated"], true, "{body:#}");
    assert_eq!(body["item_ref"], "service:health/status");
    assert!(
        body.get("status").is_none(),
        "handler must not run: {body:#}"
    );
    assert!(state.state_store.list_threads(10).unwrap().is_empty());
    // A CoW launch policy must not allocate a never-launched workspace just
    // to inspect source. Shared immutable cache materialization is allowed.
    assert!(
        !state
            .config
            .runtime_root()
            .cache()
            .join("executions")
            .exists()
    );
    let retained_head = state
        .state_store
        .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))
        .unwrap();
    assert_eq!(retained_head.as_deref(), Some(head.as_str()));
    assert_eq!(
        std::fs::read_to_string(config_path).unwrap(),
        "not valid snapshot policy"
    );
}
