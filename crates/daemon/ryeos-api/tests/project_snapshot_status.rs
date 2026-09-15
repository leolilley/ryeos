mod test_state;

use std::sync::Arc;

use ryeos_api::handlers::project_snapshot_status::{DESCRIPTOR, Request};
use ryeos_api::registry::build_service_registry;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use serde_json::{Value, json};

const CAP: &str = "ryeos.execute.service.project/snapshot-status";

fn local_operator(state: &ryeos_app::state::AppState) -> HandlerContext {
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    HandlerContext::new_with_authority(
        operator.principal_id(),
        vec![CAP.to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

#[test]
fn command_and_service_keep_offline_read_only_ownership_and_authored_scan_default() {
    let command: ryeos_runtime::CommandDef = serde_yaml::from_str(include_str!(
        "../../../../bundles/core/.ai/node/commands/snapshot-status.yaml"
    ))
    .unwrap();
    let service: Value = serde_yaml::from_str(include_str!(
        "../../../../bundles/core/.ai/services/project/snapshot-status.yaml"
    ))
    .unwrap();
    let callback_tool: Value = serde_yaml::from_str(include_str!(
        "../../../../bundles/core/.ai/tools/core/snapshot-status.yaml"
    ))
    .unwrap();
    let ryeos_runtime::CommandDispatch::ExecuteRef {
        execute,
        availability,
    } = &command.dispatch
    else {
        panic!("expected ordinary service dispatch")
    };
    assert_eq!(execute, DESCRIPTOR.service_ref);
    assert_eq!(*availability, ryeos_runtime::CommandAvailability::Both);
    assert_eq!(service["availability"], "both");
    assert_eq!(service["endpoint"], DESCRIPTOR.endpoint);
    assert_eq!(service["state_access"], "read_only_existing");
    assert_eq!(service["record_thread"], false);
    let metadata = serde_json::from_value(service.clone()).unwrap();
    assert_eq!(
        ryeos_app::service_registry::extract_standalone_state_access(&metadata).unwrap(),
        ryeos_app::service_registry::StandaloneStateAccess::ReadOnlyExisting,
    );
    assert!(!ryeos_app::service_registry::extract_record_thread(&metadata).unwrap());
    assert_eq!(service["required_caps"], json!([CAP]));
    assert_eq!(DESCRIPTOR.required_caps, [CAP]);
    assert_eq!(
        DESCRIPTOR.availability,
        ryeos_api::ServiceAvailability::Both
    );
    assert!(service.get("local_execute").is_none());
    assert_eq!(command.defaults["time_budget_ms"], 5000);
    assert_eq!(
        command.defaults["time_budget_ms"],
        callback_tool["config_schema"]["properties"]["time_budget_ms"]["default"]
    );
    assert_eq!(
        callback_tool["requires"]["capabilities"]["manifest"]["runtime_authority"]["project_snapshots"],
        json!(["status"])
    );
    assert_eq!(
        callback_tool["required_caps"],
        json!(["ryeos.read.project.live"])
    );

    let project = command.project.as_ref().unwrap();
    assert_eq!(
        project.resolution,
        ryeos_runtime::CommandProjectResolution::Required
    );
    assert!(project.request_project_path);
    assert_eq!(project.bind_parameter.as_deref(), Some("project_path"));
    let contract =
        ryeos_runtime::InvocationInputContract::from_lightweight_schema_value(&service["schema"])
            .unwrap()
            .unwrap();
    for (argv, expected_budget, include_unchanged) in [
        (vec![], 5000, false),
        (
            vec!["--time-budget-ms", "0", "--include-unchanged"],
            0,
            true,
        ),
    ] {
        let argv = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let mut params =
            ryeos_runtime::arg_binder::bind_argv_with_command(&argv, Some(&command)).unwrap();
        // The signed project policy binds the resolved project independently
        // from option parsing; preserve that exact service parameter.
        params["project_path"] = json!("/project");
        let params =
            ryeos_runtime::arg_binder::normalize_params_with_contract(params, Some(&contract))
                .unwrap();
        let request: Request = serde_json::from_value(params).unwrap();
        assert_eq!(request.time_budget_ms, expected_budget);
        assert_eq!(request.include_unchanged, include_unchanged);
    }
}

#[test]
fn service_request_requires_explicit_budget_and_refuses_unknown_authority() {
    for params in [
        json!({"project_path":"/project"}),
        json!({"project_path":"/project","time_budget_ms":-1}),
        json!({"project_path":"/project","time_budget_ms":0,"principal":"fp:caller"}),
        json!({"project_path":"/project","time_budget_ms":0,"thread_id":"T-synthetic"}),
    ] {
        assert!(serde_json::from_value::<Request>(params).is_err());
    }
}

#[tokio::test]
async fn unauthorized_callers_are_refused_before_project_path_observation() {
    let (_tmp, state) = test_state::build_test_state();
    let local = local_operator(&state);
    let contexts = [
        HandlerContext::anonymous(),
        HandlerContext::new_with_authority(
            local.fingerprint.clone(),
            vec![CAP.to_owned()],
            true,
            Some(AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:remote".into()),
        ),
        HandlerContext::new_with_authority(
            local.fingerprint.clone(),
            vec![CAP.to_owned()],
            true,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            Some("site:remote".into()),
        ),
        HandlerContext::new_with_authority(
            state.identity.principal_id(),
            vec![CAP.to_owned()],
            true,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        ),
    ];
    let state = Arc::new(state);
    let registry = build_service_registry();
    for caller in contexts {
        let error = registry.get(DESCRIPTOR.endpoint).unwrap()(
            json!({"project_path":"relative","time_budget_ms":0}),
            caller,
            state.clone(),
        )
        .await
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("snapshot status requires the configured local operator"),
            "{error}"
        );
        assert!(
            !error.contains("project_path must be an absolute"),
            "{error}"
        );
    }
    let error = registry.get(DESCRIPTOR.endpoint).unwrap()(
        json!({"project_path":"relative","time_budget_ms":0}),
        local,
        state,
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("project_path must be an absolute path"));
}

#[tokio::test]
async fn local_operator_status_reuses_principal_head_comparison_without_creating_a_head() {
    let (_tmp, state) = test_state::build_test_state();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("edited.txt"), b"current workspace edit").unwrap();
    let caller = local_operator(&state);
    let expected_principal = ryeos_state::refs::principal_storage_key(&caller.fingerprint)
        .unwrap()
        .to_owned();
    let state = Arc::new(state);
    let registry = build_service_registry();
    let result = registry.get(DESCRIPTOR.endpoint).unwrap()(
        json!({"project_path":project.path(),"time_budget_ms":0}),
        caller,
        state.clone(),
    )
    .await
    .unwrap();
    assert_eq!(result["kind"], "snapshot_status");
    assert_eq!(result["principal_key"], expected_principal);
    assert_eq!(result["baseline"], "principal_head");
    assert_eq!(result["scan_complete"], true);
    assert_eq!(result["dirty"], true);
    assert_eq!(result["counts"]["added"], 1);
    assert_eq!(result["changes"][0]["path"], "edited.txt");
    assert!(result["head_snapshot_hash"].is_null());
    assert!(result["deployed_snapshot_hash"].is_null());
    let head = state
        .state_store
        .with_state_db(|db| {
            db.read_project_head(
                &expected_principal,
                result["project_hash"].as_str().unwrap(),
            )
        })
        .unwrap();
    assert!(head.is_none());
}

#[tokio::test]
async fn status_composes_capture_policy_and_reports_policy_only_changes_without_cas_writes() {
    use ryeos_state::objects::{ProjectFile, ProjectSnapshot, ProjectTree};
    use ryeos_state::project_sync::{PROJECT_SNAPSHOT_CONFIG_RELATIVE, ProjectSyncScope};
    use std::collections::BTreeMap;

    let (_tmp, mut state) = test_state::build_test_state();
    let project = tempfile::tempdir().unwrap();
    for (path, bytes) in [
        (
            PROJECT_SNAPSHOT_CONFIG_RELATIVE,
            "schema: 1\nexclusions: [\"/.local/\"]\n",
        ),
        ("script", "#!/bin/sh\nexit 0\n"),
        ("state/kept", "ordinary project source"),
        (".local/discard", "project-excluded"),
        ("cache/discard", "node-excluded"),
        (".ai/state/discard", "structural floor"),
        (".ai/.bundles.lock", "structural floor"),
    ] {
        let path = project.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    // The ordinary state/ directory is not RyeOS runtime state. Its exclusion
    // must come from policy, not an ad hoc reserved pathname in the preview.
    let executable =
        lillux::open_pinned_regular_file_no_follow(&project.path().join("script")).unwrap();
    executable.set_mode(0o755).unwrap();
    let old_matcher =
        ryeos_app::ignore::IgnoreMatcher::from_config(&ryeos_app::ignore::IgnoreConfig {
            patterns: Vec::new(),
        })
        .unwrap();
    let old_policy = ryeos_state::project_sync::capture_snapshot_policy(
        project.path(),
        &old_matcher,
        ProjectSyncScope::FullProject,
    )
    .unwrap();
    state.ignore_matcher = Arc::new(
        ryeos_app::ignore::IgnoreMatcher::from_config(&ryeos_app::ignore::IgnoreConfig {
            patterns: vec!["cache/".to_owned()],
        })
        .unwrap(),
    );
    let current_policy = ryeos_state::project_sync::capture_snapshot_policy(
        project.path(),
        &state.ignore_matcher,
        ProjectSyncScope::FullProject,
    )
    .unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let cas = authority.cas_store().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let old_policy_hash = cas.store_object(&old_policy.to_value()).unwrap();
    let mut files = BTreeMap::new();
    for path in [PROJECT_SNAPSHOT_CONFIG_RELATIVE, "script", "state/kept"] {
        let bytes = std::fs::read(project.path().join(path)).unwrap();
        let file = ProjectFile {
            blob_hash: cas.store_blob(&bytes).unwrap(),
            size: bytes.len() as u64,
            normalized_mode: if path == "script" { 0o755 } else { 0o644 },
        };
        files.insert(path.to_owned(), cas.store_object(&file.to_value()).unwrap());
    }
    let tree = ProjectTree { files };
    let snapshot = ProjectSnapshot {
        project_tree_hash: cas.store_object(&tree.to_value()).unwrap(),
        effective_policy_hash: old_policy_hash.clone(),
        parent_hashes: Vec::new(),
        message: None,
        created_at: lillux::time::iso8601_now(),
        source: "test".to_owned(),
    };
    let head = cas.store_object(&snapshot.to_value()).unwrap();
    let caller = local_operator(&state);
    let principal = ryeos_state::refs::principal_storage_key(&caller.fingerprint).unwrap();
    let project_path = project.path().canonicalize().unwrap();
    let project_hash = ryeos_state::refs::deployed_project_key(project_path.to_str().unwrap());
    state
        .state_store
        .write_project_head_ref(
            principal,
            &project_hash,
            &head,
            &ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity),
            &guard,
        )
        .unwrap();
    drop(guard);
    let inventory = || {
        let root = lillux::PinnedDirectory::open(cas.root()).unwrap().unwrap();
        let mut paths = Vec::new();
        root.visit_regular_files_bounded(
            lillux::DirectoryTraversalBudget::new(1024, 16),
            |_, _| Ok(false),
            |relative, _| {
                paths.push(relative.to_path_buf());
                Ok(())
            },
        )
        .unwrap();
        paths
    };
    let before = inventory();
    let result = ryeos_api::handlers::project_snapshot_status::handle(
        Request {
            project_path,
            include_unchanged: true,
            time_budget_ms: 0,
        },
        caller.clone(),
        Arc::new(state),
    )
    .await
    .unwrap();
    assert_eq!(inventory(), before);
    assert_eq!(result["head_snapshot_hash"], head);
    assert_eq!(result["head_effective_policy_hash"], old_policy_hash);
    assert_eq!(
        result["effective_policy_hash"],
        ryeos_state::objects::canonical_value_digest(&current_policy.to_value()).unwrap()
    );
    assert_eq!(result["scan_complete"], true);
    assert_eq!(result["policy_changed"], true);
    assert_eq!(result["dirty"], true);
    assert_eq!(
        result["counts"],
        json!({"added":0,"modified":0,"deleted":0,"unchanged":3})
    );
    let observed = result["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        vec![PROJECT_SNAPSHOT_CONFIG_RELATIVE, "script", "state/kept"]
    );
}
