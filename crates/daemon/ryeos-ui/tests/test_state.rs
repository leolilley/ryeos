//! Shared test state builder for handler tests.
//!
//! Provides two modes:
//! - `build_test_state()`: empty engine for paths that reject before item
//!   resolution;
//! - `build_test_state_with_live_bundles()`: full engine with signed workspace
//!   bundles for paths that resolve or execute canonical item refs.

use std::sync::Arc;

use ryeos_app::state::AppState;
use ryeos_engine::kind_registry::KindRegistry;

#[allow(dead_code)]
pub fn launch_context(
    surface_ref: &str,
    project_root: Option<&str>,
    posture: ryeos_ui::compiled_binding::EffectiveUiPosture,
    user_principal_id: Option<String>,
) -> ryeos_ui::browser_session::LaunchContext {
    use std::collections::BTreeMap;

    use ryeos_api::surface_views::EffectiveUiItemIdentity;
    use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};
    use ryeos_ui::compiled_binding::{CompiledUiBinding, SessionCompiledUiBinding};

    ryeos_ui::browser_session::LaunchContext {
        compiled_binding: Arc::new(SessionCompiledUiBinding {
            binding_digest: "11".repeat(32),
            posture,
            binding: CompiledUiBinding {
                contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.to_string(),
                principal_id: "fp:test".to_string(),
                project_root: project_root.map(str::to_string),
                request_engine_generation_identity: "generation:test".to_string(),
                node_policy_generation_digest: "22".repeat(32),
                surface: EffectiveUiItemIdentity {
                    canonical_ref: surface_ref.to_string(),
                    effective_definition_digest: EffectiveDefinitionDigest::parse("33".repeat(32))
                        .expect("valid fixture digest"),
                    effective_trust_class: TrustClass::TrustedBundle,
                },
                views: BTreeMap::new(),
                sources: BTreeMap::new(),
                affordances: BTreeMap::new(),
                surface_route: None,
                attenuated: Vec::new(),
            },
        }),
        effective_surface: serde_json::json!({"kind": "Surface"}),
        granted_caps: vec!["ui.read".into()],
        user_principal_id,
        project_authority: None,
    }
}

fn service_descriptors() -> &'static [ryeos_app::service_registry::ServiceDescriptor] {
    static DESCRIPTORS: std::sync::OnceLock<Vec<ryeos_app::service_registry::ServiceDescriptor>> =
        std::sync::OnceLock::new();
    DESCRIPTORS
        .get_or_init(|| {
            ryeos_api::handlers::ALL
                .iter()
                .chain(ryeos_ui::handlers::ALL.iter())
                .copied()
                .collect()
        })
        .as_slice()
}

/// Build a minimal AppState with an empty engine.
/// Suitable only for paths that reject before canonical item resolution.
#[allow(dead_code)]
pub fn build_test_state() -> (tempfile::TempDir, AppState) {
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
        authorized_keys_dir: tmpdir.path().join("auth"),
    };
    let identity = ryeos_app::identity::NodeIdentity::create(&key_path).unwrap();
    ryeos_app::identity::NodeIdentity::create(&config.operator_signing_key_path).unwrap();
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
        ryeos_app::thread_lifecycle::ThreadLifecycleService::new_for_test_with_site_id(
            state_store.clone(),
            engine.clone(),
            kind_profiles.clone(),
            events.clone(),
            event_streams.clone(),
            "site:testhost",
        )
        .expect("valid test site identity"),
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
/// Suitable for happy-path topology/session tests that need real kind schemas,
/// verified item resolution, and effective-item composition.
#[allow(dead_code)]
pub fn build_test_state_with_live_bundles() -> (tempfile::TempDir, AppState) {
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
        authorized_keys_dir: tmpdir.path().join("auth"),
    };
    let identity = ryeos_app::identity::NodeIdentity::create(&key_path).unwrap();
    ryeos_app::identity::NodeIdentity::create(&config.operator_signing_key_path).unwrap();
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
    let engine = Arc::new(build_live_bundle_engine());
    let kind_profiles = Arc::new(ryeos_app::kind_profiles::KindProfileRegistry::build(Some(
        &engine.kinds,
    )));
    let events = Arc::new(ryeos_app::event_store_service::EventStoreService::new(
        state_store.clone(),
    ));
    let event_streams = Arc::new(ryeos_app::event_stream::ThreadEventHub::new(16));
    let threads = Arc::new(
        ryeos_app::thread_lifecycle::ThreadLifecycleService::new_for_test_with_site_id(
            state_store.clone(),
            engine.clone(),
            kind_profiles.clone(),
            events.clone(),
            event_streams.clone(),
            "site:testhost",
        )
        .expect("valid test site identity"),
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
pub fn local_operator_context(
    state: &AppState,
    scopes: Vec<String>,
) -> ryeos_app::handler_context::HandlerContext {
    let operator = ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path)
        .expect("load fixture operator");
    ryeos_app::handler_context::HandlerContext::new_with_authority(
        operator.principal_id(),
        scopes,
        true,
        Some(ryeos_app::identity::AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

#[allow(dead_code)]
fn build_live_bundle_engine() -> ryeos_engine::engine::Engine {
    let trust_store = ryeos_engine::test_support::live_trust_store();
    let core_bundle = ryeos_engine::test_support::core_bundle_root();
    let standard_bundle = ryeos_engine::test_support::standard_bundle_root();
    let ryeos_ui_bundle = ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui");

    let kinds = KindRegistry::load_base(
        &[
            core_bundle.join(".ai/node/engine/kinds"),
            standard_bundle.join(".ai/node/engine/kinds"),
        ],
        &trust_store,
    )
    .expect("load live kind registry");

    let bundle_roots = vec![core_bundle, standard_bundle, ryeos_ui_bundle];
    let (parser_tools, _) =
        ryeos_engine::parsers::ParserRegistry::load_base(&bundle_roots, &trust_store, &kinds)
            .expect("load live parser tools");
    let native_handlers = ryeos_engine::test_support::load_live_handler_registry();
    let parser_dispatcher =
        ryeos_engine::parsers::ParserDispatcher::new(parser_tools, Arc::clone(&native_handlers));
    let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &native_handlers)
        .expect("derive live composers");

    ryeos_engine::engine::Engine::new(kinds, parser_dispatcher, bundle_roots)
        .with_trust_store(trust_store.clone())
        .with_node_trust_store(trust_store)
        .with_composers(composers)
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
    let service_descriptors = service_descriptors();
    let snapshot = ryeos_app::node_config::NodeConfigSnapshot {
        bundles: vec![],
        routes: test_ui_routes(),
        commands: vec![],
    };
    let test_command_registry =
        Arc::new(ryeos_runtime::CommandRegistry::from_records(&[], &Default::default()).unwrap());
    let test_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new());

    let state = AppState {
        config: Arc::new(config),
        daemon_build: ryeos_app::build_info::get(),
        isolation: Arc::new(ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring()),
        state_store,
        engine,
        resolution_cache: std::sync::Arc::new(ryeos_app::resolution_cache::ResolutionCache::new(
            128,
        )),
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
            service_descriptors,
        )),
        service_descriptors,
        node_config: Arc::new(snapshot),
        node_policy: Arc::new(
            ryeos_app::node_policy::NodePolicySnapshot::from_test_records(vec![Arc::new(
                ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::test_policy(),
            )]),
        ),
        vault: Arc::new(ryeos_app::vault::EmptyVault),
        command_registry: test_command_registry,
        authorizer: test_auth,
        scheduler_db: Arc::new(ryeos_scheduler::db::SchedulerDb::new_in_memory().unwrap()),
        scheduler_runtime_gate: Arc::new(tokio::sync::RwLock::new(())),
        scheduler_reload_tx: None,
        ignore_matcher: Arc::new(
            ryeos_app::ignore::IgnoreMatcher::from_config(&ryeos_app::ignore::IgnoreConfig {
                patterns: Vec::new(),
            })
            .unwrap(),
        ),
        vault_fingerprint: None,
        accounting: None,
        persistent_sessions: Arc::new(ryeos_app::persistent_session::PersistentSessionPool::new()),
        execution_resources: Arc::new(
            ryeos_app::execution_resources::ExecutionResourcePool::deny_all(),
        ),
    };

    (tmpdir, state)
}

fn test_ui_routes() -> Vec<ryeos_app::route_raw::RawRouteSpec> {
    use std::collections::HashSet;

    use ryeos_app::route_raw::{
        RawLimits, RawRequest, RawRequestBody, RawResponseSpec, RawRouteSpec,
    };

    let route = |id: &str, path: &str, method: &str, source: &str, source_config| RawRouteSpec {
        id: id.to_string(),
        path: path.to_string(),
        methods: HashSet::from([method.to_string()]),
        auth: "none".to_string(),
        auth_config: None,
        limits: RawLimits::default(),
        response: RawResponseSpec {
            mode: "json".to_string(),
            source: Some(source.to_string()),
            source_config,
            status: None,
            content_type: None,
            body_b64: None,
        },
        execute: None,
        request: RawRequest {
            body: RawRequestBody::Json,
        },
        source_file: "/test/route.yaml".into(),
    };
    vec![
        route(
            "ui/launch",
            "/ui/launch/{token}",
            "GET",
            ryeos_ui::handlers::ui_launch::DESCRIPTOR.service_ref,
            serde_json::json!({"token": "${path.token}"}),
        ),
        route(
            "ui/invocations/dispatch",
            "/ui/api/invocations/dispatch",
            "POST",
            ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.service_ref,
            serde_json::json!({}),
        ),
    ]
}
