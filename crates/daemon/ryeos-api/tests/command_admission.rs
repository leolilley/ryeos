mod test_state;

use ryeos_api::handlers::commands_dispatch::{self, Request};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use ryeos_app::service_registry::ServiceRegistry;
use serde_json::{Value, json};
use std::sync::Arc;

fn setup(
    commands: &[&str],
) -> (
    tempfile::TempDir,
    ryeos_app::state::AppState,
    HandlerContext,
) {
    let (root, mut state) = test_state::build_test_state_with_bundles();
    ryeos_executor::execution::arm_private_materialization_copy_limit(32 * 1024 * 1024).unwrap();
    let bundle = ryeos_engine::test_support::core_bundle_root();
    let records = commands
        .iter()
        .map(|name| {
            serde_yaml::from_str(
                &std::fs::read_to_string(bundle.join(format!(".ai/node/commands/{name}.yaml")))
                    .unwrap(),
            )
            .unwrap()
        })
        .collect::<Vec<ryeos_runtime::CommandDef>>();
    state.command_registry = Arc::new(
        ryeos_runtime::CommandRegistry::from_records(&records, &Default::default()).unwrap(),
    );
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let context = HandlerContext::new_with_authority(
        operator.principal_id(),
        vec!["*".into()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    (root, state, context)
}

fn request(tokens: &[&str]) -> Request {
    Request {
        tokens: tokens.iter().map(|s| s.to_string()).collect(),
        ref_bindings: Default::default(),
        project_path: None,
        arguments: Value::Null,
        launch_id: None,
    }
}

fn echo(
    params: Value,
    context: HandlerContext,
    _state: Arc<ryeos_app::state::AppState>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        Ok(
            json!({"params": params, "root": context.recorded_service_root_id(),
        "principal": context.fingerprint, "origin": context.authenticated_origin_site_id,
        "grant": context.authenticated_grant_authority}),
        )
    })
}

#[tokio::test]
async fn command_capability_does_not_grant_target_capability() {
    let (_root, state, mut context) = setup(&["status"]);
    context.scopes = vec!["ryeos.execute.service.commands/dispatch".into()];
    let error = commands_dispatch::handle(
        request(&["daemon", "status"]),
        context,
        Arc::new(state.clone()),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, HandlerError::Forbidden(_)), "{error:?}");
    assert!(
        error
            .to_string()
            .contains("ryeos.execute.service.node/status"),
        "{error}"
    );
    assert!(state.state_store.list_threads(10).unwrap().is_empty());
}

#[tokio::test]
async fn absent_project_does_not_discover_daemon_app_root() {
    let (_root, mut state, mut context) = setup(&["execute"]);
    // Test app_root contains .ai. With no live-write grant this must still work.
    assert!(state.config.app_root.join(".ai").is_dir());
    context.scopes = vec!["ryeos.execute.service.health/status".into()];
    let mut services = ServiceRegistry::new();
    services.register_raw("health.status", echo);
    state.services = Arc::new(services);
    let value = commands_dispatch::handle(
        request(&["execute", "service:health/status", "{}"]),
        context,
        Arc::new(state),
    )
    .await
    .unwrap();
    assert_eq!(value["result"]["params"], json!({}), "{value:#}");
}

#[tokio::test]
async fn structured_target_consumes_only_its_own_coordinate() {
    let (_root, mut state, mut context) = setup(&["execute"]);
    context.scopes = vec!["ryeos.execute.service.health/status".into()];
    let mut services = ServiceRegistry::new();
    services.register_raw("health.status", echo);
    state.services = Arc::new(services);
    let mut req = request(&["execute", "--no-project"]);
    req.arguments = json!({"item_ref": "service:health/status"});
    let value = commands_dispatch::handle(req, context, Arc::new(state))
        .await
        .unwrap();
    assert_eq!(value["result"]["params"], json!({}), "{value:#}");
}

#[tokio::test]
async fn nested_recorded_service_gets_its_own_root() {
    let (_root, mut state, context) = setup(&["execute"]);
    let context = context
        .with_recorded_service_root_id("T-parent-command".into())
        .unwrap();
    let mut services = ServiceRegistry::new();
    services.register_raw("federation.capabilities", echo);
    state.services = Arc::new(services);
    let value = commands_dispatch::handle(
        request(&[
            "execute",
            "service:federation/capabilities",
            "--no-project",
            "{}",
        ]),
        context,
        Arc::new(state.clone()),
    )
    .await
    .unwrap();
    let body = value.get("result").unwrap_or(&value);
    let root = body["root"]
        .as_str()
        .expect("target receives its own born root");
    assert_ne!(root, "T-parent-command");
    assert!(state.state_store.get_thread(root).unwrap().is_some());
}

#[tokio::test]
async fn descriptor_pinning_reaches_real_capture_admission() {
    let (_root, state, context) = setup(&["remote-worker-run"]);
    let project = tempfile::tempdir().unwrap();
    // Deliberately invalid snapshot policy: pinned admission must reject it
    // before contacting the workflow. A live-authority downgrade misses this.
    let policy = project
        .path()
        .join(ryeos_state::project_sync::PROJECT_SNAPSHOT_CONFIG_RELATIVE);
    std::fs::create_dir_all(policy.parent().unwrap()).unwrap();
    std::fs::write(policy, "not a snapshot policy").unwrap();
    let mut req = request(&[
        "remote",
        "worker",
        "run",
        "worker_workflow:codex/test",
        "test-credential",
    ]);
    req.project_path = Some(project.path().to_string_lossy().into_owned());
    req.arguments = json!({"task": {}, "target_product_selections": []});
    let error = commands_dispatch::handle(req, context, Arc::new(state.clone()))
        .await
        .unwrap_err();
    let text = format!("{error:?}");
    assert!(
        text.contains("snapshot") || text.contains("policy"),
        "{text}"
    );
    assert!(
        !text.contains("live project"),
        "must fail pinned capture, not live admission: {text}"
    );
    assert!(state.state_store.list_threads(10).unwrap().is_empty());
}

#[tokio::test]
async fn shipped_worker_command_retains_a_real_pinned_generation() {
    let (_root, mut state, context) = setup(&["remote-worker-run"]);
    let project = tempfile::tempdir().unwrap();
    let policy = project
        .path()
        .join(ryeos_state::project_sync::PROJECT_SNAPSHOT_CONFIG_RELATIVE);
    std::fs::create_dir_all(policy.parent().unwrap()).unwrap();
    std::fs::write(policy, "schema: 1\nexclusions: []\n").unwrap();
    let mut services = ServiceRegistry::new();
    services.register_raw("remote-worker-workflows.start", echo);
    state.services = Arc::new(services);
    let mut req = request(&[
        "remote",
        "worker",
        "run",
        "worker_workflow:codex/test",
        "test-credential",
    ]);
    req.project_path = Some(project.path().to_string_lossy().into_owned());
    req.arguments = json!({"task": {}, "target_product_selections": []});
    let value = commands_dispatch::handle(req, context, Arc::new(state.clone()))
        .await
        .unwrap();
    let body = value.get("result").unwrap_or(&value);
    let root = body["root"].as_str().expect("born workflow invocation");
    let thread = state.state_store.get_thread(root).unwrap().unwrap();
    assert!(
        thread
            .project_authority
            .as_ref()
            .and_then(|p| p.subject_base_snapshot_hash())
            .is_some(),
        "{thread:?}"
    );
    assert_eq!(thread.status, "completed");
    assert_eq!(body["params"]["credential_profile_id"], "test-credential");
}

#[tokio::test]
async fn accepted_command_requires_caller_retained_launch_before_effects() {
    let (_root, state, context) = setup(&["execute"]);
    let error = commands_dispatch::handle(
        request(&[
            "execute",
            "service:health/status",
            "--no-project",
            "--async",
        ]),
        context,
        Arc::new(state.clone()),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("caller-retained launch_id"),
        "{error}"
    );
    assert!(state.state_store.list_threads(10).unwrap().is_empty());
}

#[tokio::test]
async fn forwarded_operator_keeps_verified_authority_without_second_wire_assertion() {
    let (_root, mut state, mut context) = setup(&["execute"]);
    let origin = format!("site:{}", "a".repeat(64));
    context.authorized_key_class = Some(AuthorizedKeyPrincipalClass::RemoteOperator);
    context.authenticated_origin_site_id = Some(origin.clone());
    context.authenticated_grant_authority = Some(
        ryeos_engine::principal_contract::AuthenticatedGrantAuthority {
            principal_grant_hash: "b".repeat(64),
            forwarding: Some(
                ryeos_engine::principal_contract::ForwardingAuthorityEvidence {
                    source_node_fingerprint: "c".repeat(64),
                    source_node_grant_hash: "d".repeat(64),
                },
            ),
        },
    );
    let expected_grant = serde_json::to_value(&context.authenticated_grant_authority).unwrap();
    let expected_principal = context.fingerprint.clone();
    let context = context
        .with_recorded_service_root_id("T-forwarded-command".into())
        .unwrap();
    let mut services = ServiceRegistry::new();
    services.register_raw("federation.capabilities", echo);
    state.services = Arc::new(services);
    let value = commands_dispatch::handle(
        request(&["execute", "service:federation/capabilities", "--no-project"]),
        context,
        Arc::new(state),
    )
    .await
    .unwrap();
    assert_eq!(value["result"]["origin"], origin);
    assert_eq!(value["result"]["principal"], expected_principal);
    assert_eq!(value["result"]["grant"], expected_grant);
    assert_ne!(value["result"]["root"], "T-forwarded-command");
}

#[tokio::test]
async fn command_parameters_use_the_selected_callee_types() {
    let (_root, mut state, context) = setup(&["execute"]);
    let mut services = ServiceRegistry::new();
    services.register_raw("threads.list", echo);
    services.register_raw("threads.tail", echo);
    state.services = Arc::new(services);
    for (tokens, expected) in [
        (
            vec![
                "execute",
                "service:threads/list",
                "--limit",
                "7",
                "--no-project",
            ],
            json!({"limit":7}),
        ),
        (
            vec![
                "execute",
                "service:threads/tail",
                "--thread-id",
                "T-test",
                "--thread-only=false",
                "--no-project",
            ],
            json!({"thread_id":"T-test", "thread_only":false}),
        ),
    ] {
        let value =
            commands_dispatch::handle(request(&tokens), context.clone(), Arc::new(state.clone()))
                .await
                .unwrap();
        assert_eq!(value["result"]["params"], expected, "{value:#}");
    }
}

#[tokio::test]
async fn rejected_accepted_launch_retains_coordinate_and_refuses_duplicate_delivery() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CONTACTS: AtomicUsize = AtomicUsize::new(0);
    fn count(
        params: Value,
        context: HandlerContext,
        state: Arc<ryeos_app::state::AppState>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Value>> + Send>> {
        CONTACTS.fetch_add(1, Ordering::SeqCst);
        echo(params, context, state)
    }
    let (_root, mut state, context) = setup(&["execute"]);
    let mut services = ServiceRegistry::new();
    services.register_raw("federation.capabilities", count);
    state.services = Arc::new(services);
    let launch_id = "L-0123456789abcdef0123456789abcdef";
    let make_request = || {
        let mut req = request(&[
            "execute",
            "service:federation/capabilities",
            "--no-project",
            "--async",
        ]);
        req.launch_id = Some(launch_id.into());
        req
    };
    let initial =
        commands_dispatch::handle(make_request(), context.clone(), Arc::new(state.clone()))
            .await
            .unwrap_err();
    assert!(
        format!("{initial:?}").contains("persists a pre-minted thread root"),
        "{initial:?}"
    );
    let duplicate =
        commands_dispatch::handle(make_request(), context.clone(), Arc::new(state.clone()))
            .await
            .unwrap_err();
    assert!(
        matches!(duplicate, HandlerError::Conflict(_)),
        "{duplicate:?}"
    );
    let status = ryeos_api::handlers::launch_status::handle(
        ryeos_api::handlers::launch_status::Request {
            launch_id: launch_id.into(),
        },
        context,
        Arc::new(state.clone()),
    )
    .await
    .unwrap();
    assert_eq!(status["launch_id"], launch_id);
    assert_eq!(status["status"], "failed");
    assert!(
        status["thread_id"].is_null(),
        "pre-birth failure has no running root: {status:#}"
    );
    assert_eq!(CONTACTS.load(Ordering::SeqCst), 0);
}
