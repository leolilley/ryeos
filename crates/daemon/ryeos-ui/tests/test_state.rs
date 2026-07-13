//! Shared test state builder for handler tests.
//!
//! Provides two modes:
//! - `build_test_state()`: empty engine (fast, for error-path tests)
//! - `build_test_state_with_bundles()`: full engine with workspace
//!   bundles (slower, for happy-path tests; requires populated bundles)

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
        sandbox_enabled: false,
        tool_env_passthrough: Vec::new(),
    };
    let identity = ryeos_app::identity::NodeIdentity::create(&key_path).unwrap();
    let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
        &identity,
    ));
    let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
    let state_store = Arc::new(
        ryeos_app::state_store::StateStore::new(
            runtime_state_dir,
            runtime_db_path,
            signer,
            write_barrier.clone(),
        )
        .unwrap(),
    );
    let kind_profiles = Arc::new(ryeos_app::kind_profiles::KindProfileRegistry::build(None));
    let events = Arc::new(ryeos_app::event_store_service::EventStoreService::new(
        state_store.clone(),
    ));
    let event_streams = Arc::new(ryeos_app::event_stream::ThreadEventHub::new(16));
    let threads = Arc::new(
        ryeos_app::thread_lifecycle::ThreadLifecycleService::new(
            state_store.clone(),
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

    let engine = ryeos_engine::engine::Engine::new(
        ryeos_engine::kind_registry::KindRegistry::empty(),
        ryeos_engine::parsers::ParserDispatcher::new(
            ryeos_engine::parsers::ParserRegistry::empty(),
            Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
        ),
        Vec::new(),
    );

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
/// Suitable for happy-path topology/session tests that need real kind schemas,
/// parsers, handlers, runtimes, and bundled items.
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
        sandbox_enabled: false,
        tool_env_passthrough: Vec::new(),
    };
    let identity = ryeos_app::identity::NodeIdentity::create(&key_path).unwrap();
    let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
        &identity,
    ));
    let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
    let state_store = Arc::new(
        ryeos_app::state_store::StateStore::new(
            runtime_state_dir,
            runtime_db_path,
            signer,
            write_barrier.clone(),
        )
        .unwrap(),
    );
    let kind_profiles = Arc::new(ryeos_app::kind_profiles::KindProfileRegistry::build(None));
    let events = Arc::new(ryeos_app::event_store_service::EventStoreService::new(
        state_store.clone(),
    ));
    let event_streams = Arc::new(ryeos_app::event_stream::ThreadEventHub::new(16));
    let threads = Arc::new(
        ryeos_app::thread_lifecycle::ThreadLifecycleService::new(
            state_store.clone(),
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

    let engine = build_live_bundle_engine();

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
    let (parser_tools, _) =
        ryeos_engine::parsers::ParserRegistry::load_base(&bundle_roots, &trust_store, &kinds)
            .expect("load parser tools");
    let native_handlers = ryeos_engine::test_support::load_live_handler_registry();
    let parser_dispatcher = ryeos_engine::parsers::ParserDispatcher::new(
        parser_tools,
        std::sync::Arc::clone(&native_handlers),
    );
    let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &native_handlers)
        .expect("derive composers");

    ryeos_engine::engine::Engine::new(kinds, parser_dispatcher, bundle_roots)
        .with_trust_store(trust_store)
        .with_composers(composers)
}

// Test fixture: one argument per AppState component under test.
#[allow(clippy::too_many_arguments)]
fn build_app_state(
    tmpdir: tempfile::TempDir,
    config: ryeos_app::config::Config,
    identity: ryeos_app::identity::NodeIdentity,
    state_store: Arc<ryeos_app::state_store::StateStore>,
    engine: ryeos_engine::engine::Engine,
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
        state_store,
        engine: Arc::new(engine),
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
