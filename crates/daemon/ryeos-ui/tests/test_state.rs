//! Shared test state builder for handler tests.
//!
//! Provides two modes:
//! - `build_test_state()`: empty engine for paths that reject before item
//!   resolution;
//! - `build_test_state_with_live_bundles()`: full engine with signed workspace
//!   bundles for paths that resolve or execute canonical item refs.
//!
//! Both return the Store owner between the directory and the application,
//! so `let (dir, _store_owner, state) = ...` drops the application, and every
//! Store `Arc` it holds, before the owner closes the Store and before the
//! directory is removed. The owner stays on the thread that built it.

use std::sync::Arc;

use ryeos_app::state::AppState;
use ryeos_app::test_support::{NativeTestState, TestStateStore};
use ryeos_engine::kind_registry::KindRegistry;

/// Authored test fixture consumed through the real Config resolver/compiler.
/// Test code signs this temporary project input with the established publisher;
/// it does not fabricate an effective Config identity or an admitted binding.
#[allow(dead_code)]
pub async fn live_presentation(
    state: &AppState,
) -> (
    String,
    ryeos_app::handler_context::HandlerContext,
    ryeos_client_base::ui::UiPresentationProducer,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    live_presentation_seeded(state, None).await
}

async fn live_presentation_seeded(
    state: &AppState,
    initial_surface: Option<&str>,
) -> (
    String,
    ryeos_app::handler_context::HandlerContext,
    ryeos_client_base::ui::UiPresentationProducer,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    let root = state.config.app_root.clone();
    live_presentation_in_project(state, &root, &root, initial_surface).await
}

/// The presentation fixture rooted in `project_root`, which holds the signed
/// shared Config. `spelling` is the path the launch names that project by; it
/// may be any equivalent path of the canonical root.
async fn live_presentation_in_project(
    state: &AppState,
    project_root: &std::path::Path,
    spelling: &std::path::Path,
    initial_surface: Option<&str>,
) -> (
    String,
    ryeos_app::handler_context::HandlerContext,
    ryeos_client_base::ui::UiPresentationProducer,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    let mut body = String::from(
        "category: test\nversion: '1'\nshared_presentation:\n  schema: 1\n  title: Test presentation\n  launcher: launch\n  dialogs:\n    - id: launch\n      title: Launch\n      view: 'view:ryeos/ui/launcher'\n  capabilities:\n    sources: []\n    affordances: []\n",
    );
    if let Some(surface) = initial_surface {
        // JSON scalars are valid YAML scalars; no unescaped authored ref is
        // interpolated into this signed temporary Config fixture.
        body.push_str(&format!("  initial_surfaces:\n    - id: fixture\n      title: Fixture Surface\n      surface: {}\n", serde_json::to_string(surface).unwrap()));
    }
    let key = lillux::crypto::load_signing_key(
        &ryeos_engine::test_support::workspace_root().join(".dev-keys/PUBLISHER_DEV.pem"),
    )
    .expect("established development publisher key");
    let config_dir = project_root.join(".ai/config/test");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("presentation.yaml"),
        lillux::signature::sign_content(&body, &key, "#", None),
    )
    .unwrap();
    let principal = format!("fp:{}", "ab".repeat(32));
    let project_root = project_root.canonicalize().unwrap();
    // This verified non-operator principal selects only its private, explicitly
    // registered temporary project; no test touches the user's local registry.
    let resolver =
        ryeos_app::principal::HostedPrincipalResolver::for_app_root(&state.config.app_root);
    {
        let store = ryeos_app::principal::PrincipalStore::locked_with(&resolver, &principal)
            .await
            .unwrap();
        store
            .write_yaml(
                &store.paths().projects_config(),
                &ryeos_ui::handlers::ui_projects::ProjectsFile {
                    version: 1,
                    projects: vec![ryeos_ui::handlers::ui_projects::ProjectEntry {
                        local_id: "prj_fixture".into(),
                        name: "Fixture".into(),
                        root: project_root.display().to_string(),
                        added_at: lillux::time::iso8601_now(),
                        tags: vec![],
                    }],
                },
            )
            .unwrap();
    }
    let minted = ryeos_ui::handlers::ui_launch_mint::handle(
        ryeos_ui::handlers::ui_launch_mint::Request {
            ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.into(),
            surface_ref: "config:test/presentation".into(),
            project_path: Some(spelling.display().to_string()),
            user_principal_id: Some(principal.clone()),
        },
        ryeos_app::handler_context::HandlerContext::new(principal, vec!["*".into()], true),
        Arc::new(state.clone()),
    )
    .await
    .expect("compile real signed shared Config fixture");
    claim_minted_presentation(state, &minted).await
}

/// A projectless presentation under the shipped shared Config, for a verified
/// non-operator principal. Its dialogs carry the real project registry
/// affordances, and nothing in it names a project.
#[allow(dead_code)]
pub async fn live_shipped_presentation(
    state: &AppState,
) -> (
    String,
    ryeos_app::handler_context::HandlerContext,
    ryeos_client_base::ui::UiPresentationProducer,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    let principal = format!("fp:{}", "ab".repeat(32));
    let minted = ryeos_ui::handlers::ui_launch_mint::handle(
        ryeos_ui::handlers::ui_launch_mint::Request {
            ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.into(),
            surface_ref: "config:ryeos/ui/presentation".into(),
            project_path: None,
            user_principal_id: Some(principal.clone()),
        },
        ryeos_app::handler_context::HandlerContext::new(principal, vec!["*".into()], true),
        Arc::new(state.clone()),
    )
    .await
    .expect("compile the shipped shared Config");
    claim_minted_presentation(state, &minted).await
}

/// Activate a minted session and claim its first presentation's producer.
async fn claim_minted_presentation(
    state: &AppState,
    minted: &serde_json::Value,
) -> (
    String,
    ryeos_app::handler_context::HandlerContext,
    ryeos_client_base::ui::UiPresentationProducer,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    let id = minted["session_id"].as_str().unwrap().to_owned();
    let ui = ryeos_ui::state::get_ui_state(state).unwrap();
    assert_eq!(
        ui.browser_sessions
            .consume_launch_token(minted["token"].as_str().unwrap()),
        Some(id.clone())
    );
    let (ctx, root) = session_context_and_root(state, &id);
    let producer = claim_presentation_in_session(state, &ctx, &root).await;
    (id, ctx, producer, root)
}

fn session_context_and_root(
    state: &AppState,
    id: &str,
) -> (
    ryeos_app::handler_context::HandlerContext,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    let session = ryeos_ui::state::get_ui_state(state)
        .unwrap()
        .browser_sessions
        .get_session(id)
        .unwrap();
    let root = session.attachments[&session.root_attachment_id].coordinate();
    let ctx = ryeos_app::handler_context::HandlerContext::new(
        format!("session:{id}"),
        session.granted_caps.clone(),
        false,
    );
    (ctx, root)
}

/// A further independently owned presentation in an existing session, with
/// its producer claimed.
#[allow(dead_code)]
pub async fn additional_presentation(
    state: &AppState,
    session_id: &str,
) -> ryeos_client_base::ui::UiPresentationProducer {
    let (ctx, root) = session_context_and_root(state, session_id);
    claim_presentation_in_session(state, &ctx, &root).await
}

async fn claim_presentation_in_session(
    state: &AppState,
    ctx: &ryeos_app::handler_context::HandlerContext,
    root: &ryeos_ui::browser_session::BindingAttachmentCoordinate,
) -> ryeos_client_base::ui::UiPresentationProducer {
    let mut request = serde_json::to_value(root).unwrap();
    request["action"] = serde_json::json!("prepare");
    request["creation_request_id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
    let prepared = (ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR.handler)(
        request,
        ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("prepare independently owned presentation");
    let observed = (ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR.handler)(
        serde_json::json!({
            "action":"observe", "presentation_id":prepared["presentation_id"],
            "thread_id":prepared["thread_id"], "chain_root_id":prepared["chain_root_id"]
        }),
        ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("observe exact initialization CAS predecessor");
    let observed: ryeos_client_base::ui::UiPresentationProducerObservation =
        serde_json::from_value(observed).unwrap();
    let mut claim = ryeos_client_base::ui::UiSeatControlRequest::Claim {
        schema: 2,
        operation_id: String::new(),
        presentation_id: observed.presentation_id,
        thread_id: observed.thread_id,
        chain_root_id: observed.chain_root_id,
        predecessor: observed.control_predecessor,
        intent_nonce: uuid::Uuid::new_v4().to_string(),
        claim: ryeos_client_base::ui::UiSeatProducerClaim::Fresh,
    };
    claim
        .assign_identity()
        .expect("derive exact control intent");
    let claimed = (ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR.handler)(
        serde_json::to_value(claim).unwrap(),
        ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("claim exact presentation producer");
    ryeos_client_base::ui::UiPresentationProducer {
        presentation_id: claimed["presentation_id"].as_str().unwrap().into(),
        thread_id: claimed["thread_id"].as_str().unwrap().into(),
        producer_incarnation: claimed["producer_incarnation"].as_str().unwrap().into(),
    }
}

#[allow(dead_code)]
pub async fn live_presentation_with_surface(
    state: &AppState,
    surface_ref: &str,
) -> (
    String,
    ryeos_app::handler_context::HandlerContext,
    ryeos_client_base::ui::UiPresentationProducer,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    let root = state.config.app_root.clone();
    live_presentation_with_surface_in_project(state, &root, &root, surface_ref).await
}

/// The presentation fixture rooted in `project_root`, returning the
/// coordinate of its one authored initial Surface rather than the root.
#[allow(dead_code)]
pub async fn live_presentation_with_surface_in_project(
    state: &AppState,
    project_root: &std::path::Path,
    spelling: &std::path::Path,
    surface_ref: &str,
) -> (
    String,
    ryeos_app::handler_context::HandlerContext,
    ryeos_client_base::ui::UiPresentationProducer,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    let (id, ctx, producer, _) =
        live_presentation_in_project(state, project_root, spelling, Some(surface_ref)).await;
    let session = ryeos_ui::state::get_ui_state(state)
        .unwrap()
        .browser_sessions
        .get_session(&id)
        .unwrap();
    let membership = &session.presentations[&producer.presentation_id];
    assert_eq!(membership.surfaces.len(), 1, "one authored initial Surface");
    let coordinate = membership
        .surfaces
        .values()
        .next()
        .unwrap()
        .coordinate
        .clone();
    (id, ctx, producer, coordinate)
}

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
        presentation:
            ryeos_client_base::presentation::UiCompositionPresentation::from_effective_surface(
                serde_json::json!({"name": "test-surface"}),
            )
            .expect("fixture presentation"),
        granted_caps: vec!["ui.read".into()],
        user_principal_id,
        project_authority: project_root.map(|project_root| {
            Arc::new(
                lillux::PinnedDirectory::open(std::path::Path::new(project_root))
                    .expect("open fixture project")
                    .expect("fixture project exists"),
            )
        }),
        registered_project_id: None,
    }
}

#[allow(dead_code)]
pub fn mint_launch(
    state: &AppState,
    mut context: ryeos_ui::browser_session::LaunchContext,
) -> (String, String) {
    let identity = state
        .engine
        .with_checked_bundle_generation(|generation| -> anyhow::Result<String> {
            Ok(generation.request_engine_generation_identity().to_owned())
        })
        .expect("live engine generation");
    {
        let binding = &mut Arc::make_mut(&mut context.compiled_binding).binding;
        binding.node_policy_generation_digest = state.node_policy.generation_digest().to_owned();
        binding.request_engine_generation_identity = identity;
    }
    let (session_id, token, _) = ryeos_ui::state::get_ui_state(state)
        .expect("UI state")
        .browser_sessions
        .mint_token(context, 16, state.node_policy.generation_digest())
        .expect("mint test browser session");
    (session_id, token)
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
pub fn build_test_state() -> (tempfile::TempDir, TestStateStore, AppState) {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let native = NativeTestState::open(tmpdir.path()).expect("open native test state");
    let state_store = Arc::clone(native.store());
    let (store_owner, node) = native.into_parts();
    let (config, identity, write_barrier) = node.into_parts();
    let engine = Arc::new(ryeos_engine::engine::Engine::new(
        ryeos_engine::kind_registry::KindRegistry::empty(),
        ryeos_engine::parsers::ParserDispatcher::new(
            ryeos_engine::parsers::ParserRegistry::empty(),
            Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
        ),
        Vec::new(),
    ));
    let kind_profiles = Arc::new(
        ryeos_app::kind_profiles::KindProfileRegistry::build(None).with_test_profile(
            "seat_session",
            ryeos_app::kind_profiles::ThreadKindProfile {
                root_executable: false,
                supports_interrupt: false,
                supports_continuation: false,
                supports_operator_followup: false,
                retirement_fence: true,
            },
        ),
    );
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
        store_owner,
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
pub fn build_test_state_with_live_bundles() -> (tempfile::TempDir, TestStateStore, AppState) {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let native = NativeTestState::open(tmpdir.path()).expect("open native test state");
    let state_store = Arc::clone(native.store());
    let (store_owner, node) = native.into_parts();
    let (config, identity, write_barrier) = node.into_parts();
    let engine = Arc::new(build_live_bundle_engine());
    let kind_profiles = Arc::new(
        ryeos_app::kind_profiles::KindProfileRegistry::build(Some(&engine.kinds))
            .with_test_profile(
                "seat_session",
                ryeos_app::kind_profiles::ThreadKindProfile {
                    root_executable: false,
                    supports_interrupt: false,
                    supports_continuation: false,
                    supports_operator_followup: false,
                    retirement_fence: true,
                },
            ),
    );
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
        store_owner,
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
    let operator =
        ryeos_node_runtime::identity::NodeIdentity::load(&state.config.operator_signing_key_path)
            .expect("load fixture operator");
    ryeos_app::handler_context::HandlerContext::new_with_authority(
        operator.principal_id(),
        scopes,
        true,
        Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

#[allow(dead_code)]
fn build_live_bundle_engine() -> ryeos_engine::engine::Engine {
    let trust_store = ryeos_engine::test_support::live_trust_store();
    let core_bundle = ryeos_engine::test_support::core_bundle_root();
    let workflow_bundles = ryeos_engine::test_support::workflow_bundle_roots();
    let ryeos_ui_bundle = ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui");

    let mut kind_roots = vec![core_bundle.join(".ai/node/engine/kinds")];
    kind_roots.extend(
        workflow_bundles
            .iter()
            .map(|root| root.join(".ai/node/engine/kinds")),
    );
    kind_roots.push(ryeos_ui_bundle.join(".ai/node/engine/kinds"));
    let kinds =
        KindRegistry::load_base(&kind_roots, &trust_store).expect("load live kind registry");

    let mut bundle_roots = vec![core_bundle];
    bundle_roots.extend(workflow_bundles);
    bundle_roots.push(ryeos_ui_bundle);
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
    store_owner: TestStateStore,
    config: ryeos_node_runtime::config::Config,
    identity: ryeos_node_runtime::identity::NodeIdentity,
    state_store: Arc<ryeos_app::state_store::StateStore>,
    engine: Arc<ryeos_engine::engine::Engine>,
    threads: Arc<ryeos_app::thread_lifecycle::ThreadLifecycleService>,
    events: Arc<ryeos_app::event_store_service::EventStoreService>,
    commands: Arc<ryeos_app::command_service::CommandService>,
    write_barrier: ryeos_app::write_barrier::WriteBarrier,
    event_streams: Arc<ryeos_app::event_stream::ThreadEventHub>,
) -> (tempfile::TempDir, TestStateStore, AppState) {
    let service_descriptors = service_descriptors();
    let snapshot = ryeos_app::node_config::NodeConfigSnapshot {
        external_execution: Vec::new(),
        runtime_snapshot_production: Vec::new(),
        runtime_snapshot_qualification: Vec::new(),
        guest_runtime_materialization: Vec::new(),
        bundles: vec![],
        routes: test_ui_routes(),
        commands: vec![],
    };
    let test_command_registry =
        Arc::new(ryeos_runtime::CommandRegistry::from_records(&[], &Default::default()).unwrap());
    let test_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new());

    let controller_lifetime = store_owner.controller_lifetime();
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
        controller_lifetime,
        extensions: {
            let ext = ryeos_app::extension_state::ExtensionState::new();
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
        external_placement_backends: Arc::new(Default::default()),
        external_candidate_connectors: Arc::new(Default::default()),
        external_provider_configurations: Arc::new(Default::default()),
        external_candidate_imports: Arc::new(Default::default()),
        node_policy: Arc::new(
            ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(vec![
                Arc::new(
                    ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::test_policy(),
                ),
                Arc::new(
                    ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy {
                        schema: 4,
                        max_session_bootstrap_bytes: 4194304,
                        max_live_binding_attachments_per_session: 16,
                        max_presentations_per_session: 16,
                        max_surface_instances_per_presentation: 16,
                        shared_presentation: ryeos_node_runtime::node_policy::sections::ui_browser_sessions::SharedPresentationPolicyLimits {
                            max_declaration_bytes: 65536,
                            max_dialogs: 16,
                            max_initial_surfaces: 16,
                            max_label_bytes: 128,
                            max_mount_id_bytes: 64,
                        },
                        seat: ryeos_node_runtime::node_policy::sections::ui_browser_sessions::SeatPolicy {
                            kind: "seat_session".to_owned(),
                            max_replay_frame_events: 64,
                            max_replay_frame_bytes: 4194304,
                        },
                    },
                ),
            ]),
        ),
        vault: Arc::new(ryeos_app::vault::EmptyVault),
        provider_env: std::sync::Arc::new(ryeos_app::vault::ProviderEnvironment::none()),
        command_registry: test_command_registry,
        authorizer: test_auth,
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
        scoped_producer_authorities: Arc::new(Default::default()),
        scoped_producer_processes: Arc::new(Default::default()),
        qualification_workspace_custody: Arc::new(Default::default()),
    };

    (tmpdir, store_owner, state)
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
        route(
            "ui/session/seat/append",
            "/ui/api/session/seat/append",
            "POST",
            ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.service_ref,
            serde_json::json!({}),
        ),
    ]
}
