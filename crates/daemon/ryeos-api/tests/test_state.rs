//! Shared test state builder for handler tests.
//!
//! Provides two modes:
//! - `build_test_state()`: empty engine (fast, for error-path tests)
//! - `build_test_state_with_bundles()`: full engine with workspace
//!   bundles (slower, for happy-path tests; requires populated bundles)

use std::sync::Arc;

use ryeos_app::state::AppState;

/// Build a minimal AppState with an empty engine.
/// Suitable for testing error paths (not found, wrong kind, etc.).
pub fn build_test_state() -> (tempfile::TempDir, AppState) {
    std::env::set_var("HOSTNAME", "testhost");
    let tmpdir = tempfile::TempDir::new().unwrap();
    let runtime_state_dir = tmpdir.path().join(".ai").join("state");
    let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
    let key_path = tmpdir.path().join("identity").join("node-key.pem");
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
        identity.verifying_key().clone(),
    );
    let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
    let state_store = Arc::new(
        ryeos_app::state_store::StateStore::new_with_head_trust(
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
        .expect("HOSTNAME not set in test environment"),
    );
    let commands = Arc::new(ryeos_app::command_service::CommandService::new(
        state_store.clone(),
        kind_profiles,
        events.clone(),
    ));

    build_app_state(
        tmpdir,
        config,
        identity,
        state_store,
        engine,
        threads,
        events,
        commands,
        write_barrier,
        event_streams,
    )
}

#[allow(dead_code)]
pub fn build_test_state_with_hosted_policy(token_ttl_secs: u64) -> (tempfile::TempDir, AppState) {
    let (tmpdir, mut state) = build_test_state();
    state.node_config = Arc::new(ryeos_app::node_config::NodeConfigSnapshot {
        bundles: vec![],
        routes: vec![],
        commands: vec![],
        hosted_node_policies: vec![
            ryeos_app::node_config::sections::hosted_node::HostedNodePolicyRecord {
                version: "0.1.0".into(),
                schema_version: "1.0.0".into(),
                description: "test hosted policy".into(),
                transport:
                    ryeos_app::node_config::sections::hosted_node::HostedNodeTransportPolicy {
                        public_https_required: true,
                        loopback_http_allowed: true,
                    },
                admission:
                    ryeos_app::node_config::sections::hosted_node::HostedNodeAdmissionPolicy {
                        mode: "one_time_token".into(),
                        token_ttl_secs,
                        reject_wildcard_scopes: true,
                        token_delivery: "out_of_band".into(),
                    },
                descriptor:
                    ryeos_app::node_config::sections::hosted_node::HostedNodeDescriptorPolicy {
                        require_live_identity_match: true,
                        advertised_capabilities: vec![
                            "remote-execute".into(),
                            "bundle-install".into(),
                        ],
                    },
                authorization:
                    ryeos_app::node_config::sections::hosted_node::HostedNodeAuthorizationPolicy {
                        authority: "target_node_authorized_keys".into(),
                        central_bearer_tokens_allowed: false,
                        implicit_cross_node_authority_allowed: false,
                    },
                operations:
                    ryeos_app::node_config::sections::hosted_node::HostedNodeOperationsPolicy {
                        audit_admission_events: true,
                        audit_grant_changes: true,
                        prefer_isolated_node_per_principal: true,
                        shared_daemon_multitenancy_enabled: false,
                    },
                source_file: tmpdir
                    .path()
                    .join(".ai/bundles/hosted-node/.ai/node/hosted/policy.yaml"),
            },
        ],
        command_registration_policy: Default::default(),
    });
    (tmpdir, state)
}

// Test fixture: one argument per AppState component under test.
#[allow(clippy::too_many_arguments)]
fn build_app_state(
    tmpdir: tempfile::TempDir,
    config: ryeos_app::config::Config,
    identity: ryeos_app::identity::NodeIdentity,
    state_store: Arc<ryeos_app::state_store::StateStore>,
    engine: Arc<ryeos_engine::engine::Engine>,
    threads: Arc<ryeos_app::thread_lifecycle::ThreadLifecycleService>,
    events: Arc<ryeos_app::event_store_service::EventStoreService>,
    commands: Arc<ryeos_app::command_service::CommandService>,
    write_barrier: ryeos_app::write_barrier::WriteBarrier,
    event_streams: Arc<ryeos_app::event_stream::ThreadEventHub>,
) -> (tempfile::TempDir, AppState) {
    let snapshot = ryeos_app::node_config::NodeConfigSnapshot {
        bundles: vec![],
        routes: vec![],
        commands: vec![],
        hosted_node_policies: vec![],
        command_registration_policy: Default::default(),
    };
    let test_command_registry =
        Arc::new(ryeos_runtime::CommandRegistry::from_records(&[], &Default::default()).unwrap());
    let test_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new());

    let state = AppState {
        config: Arc::new(config),
        sandbox: Arc::new(ryeos_engine::sandbox::SandboxRuntime::default()),
        state_store,
        engine,
        engine_cache: ryeos_app::engine_cache::EngineCache::new(
            ryeos_app::engine_cache::EngineCacheConfig::default(),
        ),
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
        node_config: Arc::new(snapshot),
        node_history_policy: Arc::new(
            ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::durable_without_config(),
        ),
        vault: Arc::new(ryeos_app::vault::EmptyVault),
        command_registry: test_command_registry,
        authorizer: test_auth,
        scheduler_db: Arc::new(ryeos_scheduler::db::SchedulerDb::new_in_memory().unwrap()),
        scheduler_runtime_gate: Arc::new(tokio::sync::RwLock::new(())),
        scheduler_reload_tx: None,
        ignore_matcher: Arc::new(ryeos_app::ignore::matcher_from_builtins()),
        vault_fingerprint: None,
    };

    (tmpdir, state)
}
