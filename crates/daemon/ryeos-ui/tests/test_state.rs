//! Shared test state builder for handler tests.
//!
//! Provides two modes:
//! - `build_test_state()`: empty engine (fast, for error-path tests)
//! - `build_test_state_with_live_bundles()`: metadata engine over the workspace
//!   bundles (for item-resolution and topology tests; no built binaries needed)

use std::sync::Arc;

use ryeos_app::state::AppState;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::trust::TrustStore;

/// Build a minimal AppState with an empty engine.
/// Suitable for testing error paths (not found, wrong kind, etc.).
#[allow(dead_code)]
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

/// Build an AppState backed by the live workspace core + standard + RyeOS UI bundles.
/// Suitable for happy-path topology/session tests that need real kind schemas
/// and bundled items. These handlers do not execute parser, composer, or runtime
/// binaries, so the fixture deliberately remains usable in a clean checkout.
#[allow(dead_code)]
pub fn build_test_state_with_live_bundles() -> (tempfile::TempDir, AppState) {
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
            tmpdir.path().to_path_buf(),
            runtime_state_dir,
            runtime_db_path,
            signer,
            write_barrier.clone(),
            Arc::new(head_trust),
        )
        .unwrap(),
    );
    let engine = Arc::new(build_live_bundle_engine());
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
fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("bundles").is_dir())
        .expect("workspace root with bundles/ directory")
        .to_path_buf()
}

#[allow(dead_code)]
fn build_live_bundle_engine() -> ryeos_engine::engine::Engine {
    let workspace = workspace_root();
    let trusted_dir = workspace.join("crates/bin/daemon/tests/fixtures/trusted_signers");
    let trust_store = TrustStore::load_from_dir(&trusted_dir).expect("load test trust store");
    let core_bundle = workspace.join("bundles/core");
    let std_bundle = workspace.join("bundles/standard");
    let ryeos_bundle = workspace.join("bundles/ryeos-ui");

    let kinds = KindRegistry::load_base(
        &[
            core_bundle.join(".ai/node/engine/kinds"),
            std_bundle.join(".ai/node/engine/kinds"),
        ],
        &trust_store,
    )
    .expect("load kind registry");

    let bundle_roots = vec![core_bundle, std_bundle, ryeos_bundle];
    let parser_dispatcher = ryeos_engine::parsers::ParserDispatcher::new(
        ryeos_engine::parsers::ParserRegistry::empty(),
        Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
    );

    ryeos_engine::engine::Engine::new(kinds, parser_dispatcher, bundle_roots)
        .with_trust_store(trust_store.clone())
        .with_node_trust_store(trust_store)
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
        extensions: {
            let mut ext = ryeos_app::extension_state::ExtensionState::new();
            ext.insert(std::sync::Arc::new(ryeos_ui::UiState::new()));
            Arc::new(ext)
        },
        write_barrier: Arc::new(write_barrier),
        started_at: std::time::Instant::now(),
        started_at_iso: String::new(),
        catalog_health: ryeos_app::state::CatalogHealth {
            status: "ok".into(),
            missing_services: vec![],
        },
        services: Arc::new(ryeos_api::registry::build_service_registry_from(
            ryeos_ui::handlers::ALL,
        )),
        service_descriptors: ryeos_ui::handlers::ALL,
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
