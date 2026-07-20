//! Continuation-authority projection against the REAL signed standard bundle.
//!
//! The lighter `server.rs` unit tests build `KindProfileRegistry::build(None)`
//! (only internal, non-continuable profiles), so they can pin the projection
//! WIRING but always observe `supports_continuation: false`. This integration
//! test loads the actual shipped kind schemas — where `directive_run`
//! (continuation + operator follow-up) and `graph_run` (machine continuation,
//! NO operator follow-up) live — and asserts the daemon-authored
//! `execution.{supports_continuation, supports_operator_followup}` reflect that
//! contrast through `threads.get` and `threads.list`.
//!
//! This is the honest form of the contrast: it asserts the values the bundle
//! actually ships, not a synthetic profile injected by a test constructor.

use std::path::PathBuf;
use std::sync::Arc;

use ryeos_app::event_store_service::EventStoreService;
use ryeos_app::event_stream::{ThreadEventHub, DEFAULT_EVENT_STREAM_CAPACITY};
use ryeos_app::identity::NodeIdentity;
use ryeos_app::kind_profiles::KindProfileRegistry;
use ryeos_app::state_store::{NodeIdentitySigner, StateStore};
use ryeos_app::thread_lifecycle::{ThreadCreateParams, ThreadLifecycleService};
use ryeos_app::write_barrier::WriteBarrier;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::trust::TrustStore;
use tempfile::TempDir;

fn captured_policy(item_ref: &str) -> ryeos_state::objects::CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    ryeos_state::objects::CapturedThreadHistoryPolicy {
        retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
        canonical_item_ref: item_ref.to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
            node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
        },
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("bundles").is_dir())
        .expect("workspace root with bundles/ directory")
        .to_path_buf()
}

/// A `ThreadLifecycleService` whose `KindProfileRegistry` is derived from the
/// REAL signed `core` + `standard` bundle kind schemas (so `directive_run` /
/// `graph_run` and their continuation flags are present), backed by a temp
/// state store. The temp dir is returned so it outlives the service.
fn lifecycle_with_real_kinds() -> (TempDir, Arc<ThreadLifecycleService>) {
    std::env::set_var("HOSTNAME", "testhost");
    let tmp = TempDir::new().unwrap();

    let trust_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/trusted_signers");
    let trust_store = TrustStore::load_from_dir(&trust_dir).expect("load fixture trust store");

    let root = workspace_root();
    let kinds = KindRegistry::load_base(
        &[
            root.join("bundles/core/.ai/node/engine/kinds"),
            root.join("bundles/standard/.ai/node/engine/kinds"),
        ],
        &trust_store,
    )
    .expect("load live core + standard kind schemas");
    let kind_profiles = Arc::new(KindProfileRegistry::build(Some(&kinds)));

    let identity = NodeIdentity::create(&tmp.path().join("identity").join("node-key.pem")).unwrap();
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let state_store = Arc::new(
        StateStore::new_with_head_trust(
            tmp.path().to_path_buf(),
            tmp.path().join(".ai").join("state"),
            tmp.path().join("runtime.sqlite3"),
            signer,
            WriteBarrier::new(),
            Arc::new(head_trust),
        )
        .unwrap(),
    );
    let events = Arc::new(EventStoreService::new(state_store.clone()));
    let event_streams = Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY));
    let engine = Arc::new(ryeos_engine::engine::Engine::new(
        ryeos_engine::kind_registry::KindRegistry::empty(),
        ryeos_engine::parsers::ParserDispatcher::new(
            ryeos_engine::parsers::ParserRegistry::empty(),
            Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
        ),
        Vec::new(),
    ));
    let threads = Arc::new(
        ThreadLifecycleService::new(state_store, engine, kind_profiles, events, event_streams)
            .unwrap(),
    );
    (tmp, threads)
}

fn create_params(thread_id: &str, kind: &str) -> ThreadCreateParams {
    let item_ref = match kind {
        "directive_run" => "directive:test/item",
        "graph_run" => "graph:test/item",
        other => panic!("unsupported fixture kind: {other}"),
    };
    ThreadCreateParams {
        thread_id: thread_id.to_string(),
        chain_root_id: thread_id.to_string(),
        kind: kind.to_string(),
        item_ref: item_ref.to_string(),
        executor_ref: "test/executor".to_string(),
        launch_mode: "wait".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some("user:test".to_string()),
        project_root: None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: Some(captured_policy(item_ref)),
    }
}

#[test]
fn get_thread_view_reflects_real_kind_continuation_authority() {
    let (_tmp, threads) = lifecycle_with_real_kinds();

    // directive_run ships `supports_continuation: true`.
    threads
        .create_thread_for_test(&create_params("T-dir", "directive_run"))
        .unwrap();
    let dir = serde_json::to_value(
        threads
            .get_thread_view("T-dir")
            .unwrap()
            .expect("directive thread"),
    )
    .unwrap();
    assert_eq!(dir["kind"], "directive_run");
    assert_eq!(
        dir["execution"]["supports_continuation"], true,
        "directive_run is continuable in the standard bundle: {dir:#?}"
    );
    assert_eq!(
        dir["execution"]["supports_operator_followup"], true,
        "directive_run accepts operator follow-up: {dir:#?}"
    );

    // graph_run is machine-continuable (segment-budget cut + checkpoint resume)
    // but folds no conversation, so it advertises continuation WITHOUT operator
    // follow-up.
    threads
        .create_thread_for_test(&create_params("T-graph", "graph_run"))
        .unwrap();
    let graph = serde_json::to_value(
        threads
            .get_thread_view("T-graph")
            .unwrap()
            .expect("graph thread"),
    )
    .unwrap();
    assert_eq!(graph["kind"], "graph_run");
    assert_eq!(
        graph["execution"]["supports_continuation"], true,
        "graph_run is machine-continuable: {graph:#?}"
    );
    assert_eq!(
        graph["execution"]["supports_operator_followup"], false,
        "graph_run refuses operator follow-up: {graph:#?}"
    );
}

#[test]
fn thread_list_reflects_real_kind_continuation_authority() {
    let (_tmp, threads) = lifecycle_with_real_kinds();
    threads
        .create_thread_for_test(&create_params("T-dir", "directive_run"))
        .unwrap();
    threads
        .create_thread_for_test(&create_params("T-graph", "graph_run"))
        .unwrap();

    let listing = threads.list_threads_filtered(100, None).unwrap();
    let rows = listing["threads"].as_array().expect("threads array");
    let fact = |id: &str, key: &str| {
        rows.iter()
            .find(|r| r["thread_id"] == id)
            .unwrap_or_else(|| panic!("row {id} missing from list: {listing:#?}"))["execution"][key]
            .clone()
    };

    // directive: continuation + operator follow-up.
    assert_eq!(
        fact("T-dir", "supports_continuation"),
        true,
        "directive_run row continuable"
    );
    assert_eq!(
        fact("T-dir", "supports_operator_followup"),
        true,
        "directive_run row accepts operator follow-up"
    );
    // graph: machine continuation, NO operator follow-up.
    assert_eq!(
        fact("T-graph", "supports_continuation"),
        true,
        "graph_run row machine-continuable"
    );
    assert_eq!(
        fact("T-graph", "supports_operator_followup"),
        false,
        "graph_run row refuses operator follow-up"
    );
}
