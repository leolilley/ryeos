use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewThreadRecord;
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, NativeResumeSpec, Principal, ProjectContext,
};

fn build_test_state() -> (tempfile::TempDir, AppState) {
    std::env::set_var("HOSTNAME", "recovery-testhost");
    let tmpdir = tempfile::TempDir::new().unwrap();
    let runtime_state_dir = tmpdir.path().join(".ai/state");
    let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
    let key_path = tmpdir.path().join("identity/node-key.pem");
    let config = ryeos_app::config::Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_path: runtime_db_path.clone(),
        uds_path: tmpdir.path().join("test.sock"),
        app_root: tmpdir.path().to_path_buf(),
        node_signing_key_path: key_path.clone(),
        operator_signing_key_path: tmpdir.path().join("user-key.pem"),
        require_auth: false,
        authorized_keys_dir: tmpdir.path().join("auth"),
        tool_env_passthrough: Vec::new(),
    };
    let identity = ryeos_app::identity::NodeIdentity::create(&key_path).unwrap();
    let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
        &identity,
    ));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
    let state_store = Arc::new(
        ryeos_app::state_store::StateStore::new_with_head_trust(
            tmpdir.path().to_path_buf(),
            runtime_state_dir,
            runtime_db_path,
            signer,
            write_barrier.clone(),
            Arc::new(head_trust),
        )
        .unwrap(),
    );
    let engine = Arc::new(ryeos_engine::engine::Engine::new(
        ryeos_engine::kind_registry::KindRegistry::empty(),
        ryeos_engine::parsers::ParserDispatcher::new(
            ryeos_engine::parsers::ParserRegistry::empty(),
            Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
        ),
        Vec::new(),
    ));
    let kind_profiles = Arc::new(ryeos_app::kind_profiles::KindProfileRegistry::build(None));
    let events = Arc::new(ryeos_app::event_store_service::EventStoreService::new(
        state_store.clone(),
    ));
    let event_streams = Arc::new(ryeos_app::event_stream::ThreadEventHub::new(16));
    let threads = Arc::new(
        ryeos_app::thread_lifecycle::ThreadLifecycleService::new(
            state_store.clone(),
            engine.clone(),
            kind_profiles.clone(),
            events.clone(),
            event_streams.clone(),
        )
        .unwrap(),
    );
    let commands = Arc::new(ryeos_app::command_service::CommandService::new(
        state_store.clone(),
        kind_profiles,
        events.clone(),
    ));
    let node_config = ryeos_app::node_config::NodeConfigSnapshot {
        bundles: vec![],
        routes: vec![],
        commands: vec![],
        hosted_node_policies: vec![],
        command_registration_policy: Default::default(),
    };

    let state = AppState {
        config: Arc::new(config),
        daemon_build: ryeos_app::build_info::get(),
        isolation: Arc::new(ryeos_engine::isolation::IsolationRuntime::default()),
        state_store,
        engine,
        engine_cache: ryeos_app::engine_cache::EngineCache::new(Default::default()),
        identity: Arc::new(identity),
        threads,
        live_input: Arc::new(ryeos_app::live_input_queue::LiveInputQueue::new()),
        events,
        event_streams,
        commands,
        callback_tokens: Arc::new(ryeos_app::callback_token::CallbackCapabilityStore::new()),
        thread_auth: Arc::new(ryeos_app::callback_token::ThreadAuthStore::new()),
        extensions: Arc::new(ryeos_app::extension_state::ExtensionState::new()),
        write_barrier: Arc::new(write_barrier),
        started_at: std::time::Instant::now(),
        started_at_iso: String::new(),
        catalog_health: ryeos_app::state::CatalogHealth {
            status: "ok".into(),
            missing_services: vec![],
        },
        services: Arc::new(ryeos_api::registry::build_service_registry()),
        service_descriptors: ryeos_api::handlers::ALL,
        node_config: Arc::new(node_config),
        node_history_policy: Arc::new(
            ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::durable_without_config(),
        ),
        vault: Arc::new(ryeos_app::vault::EmptyVault),
        command_registry: Arc::new(
            ryeos_runtime::CommandRegistry::from_records(&[], &Default::default()).unwrap(),
        ),
        authorizer: Arc::new(ryeos_runtime::authorizer::Authorizer::new()),
        scheduler_db: Arc::new(ryeos_scheduler::db::SchedulerDb::new_in_memory().unwrap()),
        scheduler_runtime_gate: Arc::new(tokio::sync::RwLock::new(())),
        scheduler_reload_tx: None,
        ignore_matcher: Arc::new(ryeos_app::ignore::matcher_from_builtins()),
        vault_fingerprint: None,
    };
    (tmpdir, state)
}

fn thread_record(
    thread_id: &str,
    project_root: Option<std::path::PathBuf>,
    project_authority: ryeos_state::objects::ExecutionProjectAuthority,
) -> NewThreadRecord {
    NewThreadRecord {
        thread_id: thread_id.to_string(),
        chain_root_id: thread_id.to_string(),
        kind: "graph".to_string(),
        item_ref: "graph:test/recovery".to_string(),
        executor_ref: "executor:test/runtime".to_string(),
        launch_mode: "detached".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some("user:test".to_string()),
        project_root,
        project_authority,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
            canonical_item_ref: "graph:test/recovery".to_string(),
            item_content_hash: "a".repeat(64),
            item_signer_fingerprint: Some("b".repeat(64)),
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: "c".repeat(64),
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy:
                    ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
            },
        }),
    }
}

fn principal() -> EffectivePrincipal {
    EffectivePrincipal::Local(Principal {
        fingerprint: "fp:test".to_string(),
        scopes: vec!["execute".to_string()],
    })
}

fn projectless_resume() -> ResumeContext {
    ResumeContext {
        kind: "graph".to_string(),
        item_ref: "graph:test/recovery".to_string(),
        ref_bindings: BTreeMap::new(),
        launch_mode: "detached".to_string(),
        parameters: serde_json::json!({}),
        project_context: ProjectContext::None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
        stable_project_identity: None,
        local_overlay_root: None,
        original_snapshot_hash: None,
        original_pushed_head_ref: None,
        state_root: None,
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        requested_by: principal(),
        execution_hints: ExecutionHints::default(),
        effective_caps: Vec::new(),
        parent_delegation_caps: None,
        executor_ref: Some("executor:test/runtime".to_string()),
        runtime_ref: Some("runtime:test/graph".to_string()),
    }
}

fn live_resume(project: &std::path::Path) -> ResumeContext {
    let project = project.to_path_buf();
    let authority = ryeos_state::objects::ExecutionProjectAuthority::live(
        project.clone(),
        format!("local:{}", project.display()),
        ryeos_state::objects::LiveProjectAccess::ReadWrite,
        ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted(),
        ryeos_state::objects::EnvironmentAuthority::None,
        Vec::new(),
    )
    .unwrap();
    ResumeContext {
        project_context: ProjectContext::LocalPath {
            path: project.clone(),
        },
        project_authority: authority,
        stable_project_identity: Some(
            ryeos_app::launch_metadata::StableProjectIdentity::from_path(&project, "site:test")
                .unwrap(),
        ),
        ..projectless_resume()
    }
}

fn resume_intent(thread_id: &str) -> super::reconcile::ResumeIntent {
    super::reconcile::ResumeIntent {
        thread_id: thread_id.to_string(),
        chain_root_id: thread_id.to_string(),
        resume_context: projectless_resume(),
        launch_driver: ryeos_state::objects::ExecutionLaunchDriver::DirectItemExecutor,
        prior_status: "created".to_string(),
        kind: super::reconcile::ResumeKind::AdmittedRoot,
    }
}

#[tokio::test]
async fn shared_recovery_dispatch_settles_only_the_unreconstructable_target() {
    let (_tmpdir, state) = build_test_state();
    let bad = "T-bad-recovery";
    state
        .state_store
        .create_thread_for_test(&thread_record(
            bad,
            None,
            ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        ))
        .unwrap();

    super::dispatch_resume_intents(&state, vec![resume_intent(bad)])
        .await
        .unwrap();
    let bad_thread = state.state_store.get_thread(bad).unwrap().unwrap();
    assert_eq!(bad_thread.status, "failed");
    assert_eq!(
        state
            .state_store
            .get_thread_result(bad)
            .unwrap()
            .unwrap()
            .outcome_code
            .as_deref(),
        Some("resume_rebuild_failed")
    );
    super::ensure_recovery_targets_classified(&state, &BTreeSet::from([bad.to_string()])).unwrap();

    let unrelated = "T-unrelated";
    state
        .state_store
        .create_thread_for_test(&thread_record(
            unrelated,
            None,
            ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        ))
        .expect("unrelated project admission remains usable");
    assert_eq!(
        state
            .state_store
            .get_thread(unrelated)
            .unwrap()
            .unwrap()
            .status,
        "created"
    );
}

#[tokio::test]
async fn shared_recovery_dispatch_never_overwrites_a_competing_claim() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-claimed-recovery";
    state
        .state_store
        .create_thread_for_test(&thread_record(
            thread_id,
            None,
            ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        ))
        .unwrap();
    state
        .state_store
        .claim_thread_launch_active(
            thread_id,
            "claim-live",
            ryeos_app::runtime_db::daemon_generation_id(),
        )
        .unwrap()
        .expect("competing launch claim");

    super::dispatch_resume_intents(&state, vec![resume_intent(thread_id)])
        .await
        .unwrap();
    assert_eq!(
        state
            .state_store
            .get_thread(thread_id)
            .unwrap()
            .unwrap()
            .status,
        "created"
    );
    super::ensure_recovery_targets_classified(&state, &BTreeSet::from([thread_id.to_string()]))
        .unwrap();
}

fn seed_live_recovery(
    state: &AppState,
    thread_id: &str,
    project: &std::path::Path,
) -> ResumeContext {
    let resume = live_resume(project);
    state
        .state_store
        .create_thread_for_test(&thread_record(
            thread_id,
            Some(project.to_path_buf()),
            resume.project_authority.clone(),
        ))
        .unwrap();
    state
        .state_store
        .mark_thread_running(thread_id, None)
        .unwrap();
    state
        .state_store
        .seed_launch_metadata(
            thread_id,
            &RuntimeLaunchMetadata::default()
                .with_native_resume(NativeResumeSpec::default())
                .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
                .with_resume_context(resume.clone()),
        )
        .unwrap();
    resume
}

#[tokio::test]
async fn periodic_recovery_waits_only_the_thread_with_a_missing_live_root() {
    let (_tmpdir, state) = build_test_state();
    let projects = tempfile::tempdir().unwrap();
    let project = projects.path().join("missing-project");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(project.join(ryeos_engine::AI_DIR)).unwrap();
    seed_live_recovery(&state, "T-missing-root", &project);
    std::fs::remove_dir_all(&project).unwrap();

    let report = super::reconcile::reconcile_live_threads(&state)
        .await
        .unwrap();
    assert!(report.active_thread_ids.contains("T-missing-root"));
    let waiting = state
        .state_store
        .get_thread("T-missing-root")
        .unwrap()
        .unwrap();
    assert_eq!(waiting.status, "running");
    assert_eq!(
        waiting
            .runtime
            .recovery_wait
            .as_ref()
            .map(|wait| wait.reason.as_str()),
        Some("waiting_for_project_authority")
    );

    state
        .state_store
        .create_thread_for_test(&thread_record(
            "T-other-project",
            None,
            ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        ))
        .expect("unrelated project remains usable during the authority wait");
}

#[tokio::test]
async fn periodic_recovery_permanently_refuses_only_the_structurally_invalid_root() {
    let (_tmpdir, state) = build_test_state();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(ryeos_engine::AI_DIR),
        b"not a directory",
    )
    .unwrap();
    seed_live_recovery(&state, "T-invalid-root", project.path());

    super::reconcile::reconcile_live_threads(&state)
        .await
        .unwrap();
    let refused = state
        .state_store
        .get_thread("T-invalid-root")
        .unwrap()
        .unwrap();
    assert_eq!(refused.status, "failed");
    assert_eq!(
        state
            .state_store
            .get_thread_result("T-invalid-root")
            .unwrap()
            .unwrap()
            .outcome_code
            .as_deref(),
        Some("project_authority_invalid")
    );
    assert!(refused.runtime.recovery_wait.is_none());

    state
        .state_store
        .create_thread_for_test(&thread_record(
            "T-other-after-refusal",
            None,
            ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        ))
        .expect("unrelated project remains usable after permanent refusal");
}
