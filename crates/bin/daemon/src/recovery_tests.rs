use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
use ryeos_app::state::AppState;
use ryeos_app::state_store::{
    FinalizeThreadRecord, InProcessHandlerControl, NewEventRecord, NewThreadRecord,
};
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

fn seed_in_process_handler_record(
    state: &AppState,
    record: NewThreadRecord,
) -> InProcessHandlerControl {
    let owner = state
        .state_store
        .register_in_process_handler(&record.thread_id)
        .unwrap();
    state
        .state_store
        .create_in_process_root_with_events_and_launch_metadata(
            &record,
            vec![NewEventRecord {
                event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                storage_class: "indexed".to_string(),
                payload: serde_json::json!({}),
            }],
            &RuntimeLaunchMetadata::default()
                .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
                .with_in_process_lifecycle_authority(
                    ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
                ),
            &owner,
        )
        .unwrap();
    owner
}

fn seed_in_process_handler(state: &AppState, thread_id: &str) -> InProcessHandlerControl {
    seed_in_process_handler_record(
        state,
        thread_record(
            thread_id,
            None,
            ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        ),
    )
}

fn seed_in_process_reservation_without_root(state: &AppState, thread_id: &str, phase: &str) {
    let launch_metadata = in_process_launch_metadata_json();
    let connection = rusqlite::Connection::open(&state.config.db_path).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    transaction
        .execute(
            "INSERT INTO thread_runtime (
                thread_id, chain_root_id, pid, pgid, metadata, launch_metadata
             ) VALUES (?1, ?1, NULL, NULL, NULL, ?2)",
            rusqlite::params![thread_id, launch_metadata],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO in_process_handler_reservation (
                thread_id, phase, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 1, 1)",
            rusqlite::params![thread_id, phase],
        )
        .unwrap();
    transaction.commit().unwrap();
}

fn in_process_launch_metadata_json() -> String {
    let value = serde_json::to_value(
        RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
            .with_in_process_lifecycle_authority(
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
            ),
    )
    .unwrap();
    lillux::canonical_json(&value).unwrap()
}

fn seed_in_process_reservation_for_existing_root(state: &AppState, thread_id: &str, phase: &str) {
    let connection = rusqlite::Connection::open(&state.config.db_path).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    assert_eq!(
        transaction
            .execute(
                "UPDATE thread_runtime
                    SET launch_metadata = ?2
                  WHERE thread_id = ?1",
                rusqlite::params![thread_id, in_process_launch_metadata_json()],
            )
            .unwrap(),
        1
    );
    transaction
        .execute(
            "INSERT INTO in_process_handler_reservation (
                thread_id, phase, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 1, 1)",
            rusqlite::params![thread_id, phase],
        )
        .unwrap();
    transaction.commit().unwrap();
}

fn seed_pending_in_process_reservation_without_root(state: &AppState, thread_id: &str) {
    seed_in_process_reservation_without_root(state, thread_id, "pending");
}

fn regress_in_process_reservation_to_pending(state: &AppState, thread_id: &str) {
    let connection = rusqlite::Connection::open(&state.config.db_path).unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE in_process_handler_reservation
                    SET phase = 'pending', updated_at_ms = 1
                  WHERE thread_id = ?1 AND phase = 'running'",
                rusqlite::params![thread_id],
            )
            .unwrap(),
        1
    );
}

fn set_in_process_reservation_phase(state: &AppState, thread_id: &str, phase: &str) {
    let connection = rusqlite::Connection::open(&state.config.db_path).unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE in_process_handler_reservation
                    SET phase = ?2, updated_at_ms = 1
                  WHERE thread_id = ?1",
                rusqlite::params![thread_id, phase],
            )
            .unwrap(),
        1
    );
}

fn delete_in_process_reservation(state: &AppState, thread_id: &str) {
    let connection = rusqlite::Connection::open(&state.config.db_path).unwrap();
    assert_eq!(
        connection
            .execute(
                "DELETE FROM in_process_handler_reservation WHERE thread_id = ?1",
                rusqlite::params![thread_id],
            )
            .unwrap(),
        1
    );
}

fn assert_service_interrupted(state: &AppState, thread_id: &str, recovery_mode: &str) {
    let thread = state.state_store.get_thread(thread_id).unwrap().unwrap();
    assert_eq!(thread.status, "failed");
    assert_eq!(
        thread
            .runtime
            .launch_metadata
            .as_ref()
            .and_then(|metadata| metadata.launch_driver),
        Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
    );
    let result = state
        .state_store
        .get_thread_result(thread_id)
        .unwrap()
        .unwrap();
    assert_eq!(result.outcome_code.as_deref(), Some("service_interrupted"));
    let error = result.error.unwrap();
    assert_eq!(error["code"].as_str(), Some("service_interrupted"));
    assert_eq!(error["recovery_mode"].as_str(), Some(recovery_mode));
}

#[test]
fn exact_terminal_postcommit_repair_republishes_the_persisted_terminal_event() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-postcommit-repair";
    let owner = seed_in_process_handler(&state, thread_id);
    let mut subscriber = state.event_streams.subscribe(thread_id);
    let terminal = FinalizeThreadRecord {
        status: "completed".to_string(),
        outcome_code: Some("success".to_string()),
        result_json: Some(serde_json::json!({"ok": true})),
        error_json: None,
        artifacts: Vec::new(),
        final_cost: None,
        managed_envelope: None,
        result_project_snapshot_hash: None,
    };

    state
        .state_store
        .finalize_in_process_handler_owned(thread_id, &owner, &terminal)
        .unwrap();
    assert!(
        state
            .state_store
            .delete_thread_projection_events_for_test(thread_id)
            .unwrap()
            > 0
    );
    assert!(state
        .state_store
        .latest_thread_events(thread_id, 1)
        .unwrap()
        .is_empty());
    assert!(state
        .state_store
        .delete_thread_projection_row_for_test(thread_id)
        .unwrap());
    assert!(state.state_store.get_thread(thread_id).unwrap().is_none());
    assert!(matches!(
        subscriber.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    state
        .threads
        .repair_recorded_service_terminal_postcommit(
            &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                thread_id: thread_id.to_string(),
                status: terminal.status.clone(),
                outcome_code: terminal.outcome_code.clone(),
                result: terminal.result_json.clone(),
                error: terminal.error_json.clone(),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            },
        )
        .unwrap();

    let published = subscriber.try_recv().unwrap();
    assert_eq!(
        published.event_type,
        ryeos_state::event_types::THREAD_COMPLETED
    );
    assert!(state
        .state_store
        .settle_in_process_handler_reservation_owned(thread_id, &owner)
        .unwrap());
    owner.mark_terminal_confirmed();
    assert!(state
        .state_store
        .delete_terminal_in_process_handler_reservation_owned(thread_id, &owner)
        .unwrap());
    assert!(state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap());
}

#[test]
fn shutdown_authoritative_audit_refuses_to_run_while_an_owner_is_active() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-shutdown-owner-gate";
    let owner = seed_in_process_handler(&state, thread_id);

    let error = super::audit_ownerless_in_process_reservations(&state)
        .expect_err("shutdown audit must not race a registered handler owner");
    assert!(error
        .to_string()
        .contains("requires an empty in-process owner registry"));
    assert_eq!(
        state
            .state_store
            .get_authoritative_root_thread_snapshot(thread_id)
            .unwrap()
            .unwrap()
            .status,
        ryeos_state::objects::ThreadStatus::Running
    );
    assert_eq!(
        state
            .state_store
            .in_process_handler_reservation(thread_id)
            .unwrap()
            .unwrap()
            .phase,
        ryeos_app::runtime_db::InProcessHandlerReservationPhase::Running
    );
    assert!(state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap());
}

#[test]
fn shutdown_authoritative_audit_repairs_and_retires_an_ownerless_terminal() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-shutdown-terminal-repair";
    let owner = seed_in_process_handler(&state, thread_id);
    state
        .state_store
        .finalize_in_process_handler_owned(
            thread_id,
            &owner,
            &FinalizeThreadRecord {
                status: "completed".to_string(),
                outcome_code: Some("success".to_string()),
                result_json: Some(serde_json::json!({"ok": true})),
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
            },
        )
        .unwrap();
    assert!(state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap());

    assert!(super::audit_ownerless_in_process_reservations(&state).unwrap());
    assert!(state
        .state_store
        .in_process_handler_reservation(thread_id)
        .unwrap()
        .is_none());
    assert_eq!(
        state
            .state_store
            .get_authoritative_root_thread_snapshot(thread_id)
            .unwrap()
            .unwrap()
            .status,
        ryeos_state::objects::ThreadStatus::Completed
    );
}

#[tokio::test]
async fn reservation_reconciliation_accepts_a_preconverged_discarded_pending_birth() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-race-discarded-birth";
    seed_pending_in_process_reservation_without_root(&state, thread_id);
    assert!(state
        .state_store
        .discard_uncommitted_in_process_handler_birth(thread_id)
        .unwrap());

    let report = super::reconcile::reconcile_active_threads(&state)
        .await
        .unwrap();
    assert!(report.resume_intents.is_empty());
    assert!(state
        .state_store
        .in_process_handler_reservation(thread_id)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn reservation_reconciliation_accepts_a_preconverged_retired_terminal() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-race-retired-terminal";
    let owner = seed_in_process_handler(&state, thread_id);
    state
        .state_store
        .finalize_in_process_handler_owned(
            thread_id,
            &owner,
            &FinalizeThreadRecord {
                status: "completed".to_string(),
                outcome_code: Some("success".to_string()),
                result_json: Some(serde_json::json!({"ok": true})),
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
            },
        )
        .unwrap();
    assert!(state
        .state_store
        .settle_in_process_handler_reservation_owned(thread_id, &owner)
        .unwrap());
    owner.mark_terminal_confirmed();
    assert!(state
        .state_store
        .delete_terminal_in_process_handler_reservation_owned(thread_id, &owner)
        .unwrap());
    assert!(state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap());

    let report = super::reconcile::reconcile_active_threads(&state)
        .await
        .unwrap();
    assert!(report.resume_intents.is_empty());
    assert_eq!(
        state
            .state_store
            .get_authoritative_root_thread_snapshot(thread_id)
            .unwrap()
            .unwrap()
            .status,
        ryeos_state::objects::ThreadStatus::Completed
    );
}

#[tokio::test]
async fn startup_reconciliation_discards_pending_in_process_birth_without_root() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-pending-no-root";
    seed_pending_in_process_reservation_without_root(&state, thread_id);

    let report = super::reconcile::reconcile_active_threads(&state)
        .await
        .unwrap();

    assert!(report.resume_intents.is_empty());
    assert!(state
        .state_store
        .get_launch_metadata(thread_id)
        .unwrap()
        .is_none());
    assert!(state
        .state_store
        .in_process_handler_reservations_after(
            None,
            ryeos_app::runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
        )
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn startup_reconciliation_rejects_committed_reservation_phases_without_a_root() {
    for (phase, expected) in [
        ("running", "running in-process reservation"),
        (
            "terminal_confirmed",
            "terminal-confirmed in-process reservation",
        ),
    ] {
        let (_tmpdir, state) = build_test_state();
        let thread_id = format!("T-service-{phase}-no-root");
        seed_in_process_reservation_without_root(&state, &thread_id, phase);

        let error = super::reconcile::reconcile_active_threads(&state)
            .await
            .expect_err("committed reservation phase without CAS root must fail closed");
        assert!(error.to_string().contains(expected), "error={error:#}");
    }
}

#[tokio::test]
async fn startup_reconciliation_rejects_terminal_phase_against_running_root() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-terminal-phase-running-root";
    let owner = seed_in_process_handler(&state, thread_id);
    set_in_process_reservation_phase(&state, thread_id, "terminal_confirmed");
    state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap();

    let error = super::reconcile::reconcile_active_threads(&state)
        .await
        .expect_err("terminal reservation phase must contradict a running CAS root");
    assert!(
        error
            .to_string()
            .contains("terminal-confirmed in-process reservation"),
        "error={error:#}"
    );
}

#[tokio::test]
async fn startup_reconciliation_rejects_every_reservation_phase_against_created_root() {
    for phase in ["pending", "running", "terminal_confirmed"] {
        let (_tmpdir, state) = build_test_state();
        let thread_id = format!("T-service-{phase}-created-root");
        state
            .state_store
            .create_thread_for_test(&thread_record(
                &thread_id,
                None,
                ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            ))
            .unwrap();
        seed_in_process_reservation_for_existing_root(&state, &thread_id, phase);

        let error = super::reconcile::reconcile_active_threads(&state)
            .await
            .expect_err("an in-process reservation cannot authorize a Created root");
        assert!(
            error
                .to_string()
                .contains("contradicts authoritative status created"),
            "phase={phase}, error={error:#}"
        );
    }
}

#[tokio::test]
async fn startup_reconciliation_rejects_nonterminal_in_process_root_without_reservation() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-running-without-reservation";
    let owner = seed_in_process_handler(&state, thread_id);
    delete_in_process_reservation(&state, thread_id);
    state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap();

    let error = super::reconcile::reconcile_active_threads(&state)
        .await
        .expect_err("running in-process root without reservation must fail closed");
    assert!(
        error.to_string().contains("has no durable reservation"),
        "error={error:#}"
    );
}

#[tokio::test]
async fn startup_reconciliation_advances_pending_committed_root_before_owner_loss_settlement() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-pending-committed";
    let owner = seed_in_process_handler(&state, thread_id);
    regress_in_process_reservation_to_pending(&state, thread_id);
    state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap();

    let report = super::reconcile::reconcile_active_threads(&state)
        .await
        .unwrap();

    assert!(report.resume_intents.is_empty());
    assert_service_interrupted(&state, thread_id, "startup");
    assert!(state
        .state_store
        .in_process_handler_reservations_after(
            None,
            ryeos_app::runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
        )
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn startup_reconciliation_fails_running_in_process_handler_without_resume() {
    let (_tmpdir, state) = build_test_state();
    let owner = seed_in_process_handler(&state, "T-service-running");
    state
        .state_store
        .unregister_in_process_handler("T-service-running", &owner)
        .unwrap();

    let report = super::reconcile::reconcile_active_threads(&state)
        .await
        .unwrap();

    assert!(report.resume_intents.is_empty());
    assert!(!report.active_thread_ids.contains("T-service-running"));
    assert_service_interrupted(&state, "T-service-running", "startup");
    assert!(state
        .state_store
        .in_process_handler_reservations_after(
            None,
            ryeos_app::runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
        )
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn reservation_reconciliation_terminalizes_ownerless_root_without_projection_visibility() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-stale-projection";
    let owner = seed_in_process_handler(&state, thread_id);
    state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap();
    state
        .state_store
        .set_thread_projection_status_for_test(thread_id, "completed")
        .unwrap();

    let report = super::reconcile::reconcile_active_threads(&state)
        .await
        .unwrap();

    assert!(!report.active_thread_ids.contains(thread_id));
    assert_service_interrupted(&state, thread_id, "startup");
    assert!(state
        .state_store
        .in_process_handler_reservations_after(
            None,
            ryeos_app::runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
        )
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn live_reconciliation_preserves_a_registered_in_process_handler() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-owned";
    let owner = seed_in_process_handler(&state, thread_id);

    let report = super::reconcile::reconcile_live_threads(&state)
        .await
        .unwrap();

    assert!(report.resume_intents.is_empty());
    assert!(report.active_thread_ids.contains(thread_id));
    assert_eq!(
        state
            .state_store
            .get_thread(thread_id)
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert!(state
        .state_store
        .is_in_process_handler_active(thread_id)
        .unwrap());
    assert!(state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap());
}

#[tokio::test]
async fn live_reconciliation_fails_an_unregistered_in_process_handler_idempotently() {
    let (_tmpdir, state) = build_test_state();
    let thread_id = "T-service-orphan";
    let mut record = thread_record(
        thread_id,
        None,
        ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
    );
    record.usage_subject = Some(ryeos_state::UsageSubject {
        namespace: "test".to_string(),
        subject: "service-orphan".to_string(),
    });
    record.usage_subject_asserted_by = Some("user:test".to_string());
    let owner = seed_in_process_handler_record(&state, record);
    state
        .state_store
        .unregister_in_process_handler(thread_id, &owner)
        .unwrap();

    let first = super::reconcile::reconcile_live_threads(&state)
        .await
        .unwrap();
    let second = super::reconcile::reconcile_live_threads(&state)
        .await
        .unwrap();

    assert!(first.resume_intents.is_empty());
    assert!(second.resume_intents.is_empty());
    assert_service_interrupted(&state, thread_id, "live");
    assert!(state
        .state_store
        .in_process_handler_reservations_after(
            None,
            ryeos_app::runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
        )
        .unwrap()
        .is_empty());
    assert_eq!(
        state
            .state_store
            .get_thread(thread_id)
            .unwrap()
            .unwrap()
            .project_authority,
        Some(ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS)
    );
    let events = state
        .state_store
        .latest_thread_events(thread_id, 16)
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "thread_failed")
            .count(),
        1
    );
    let created = events
        .iter()
        .find(|event| event.event_type == "thread_created")
        .unwrap();
    assert_eq!(
        created.payload["usage_subject"]["namespace"].as_str(),
        Some("test")
    );
    assert_eq!(
        created.payload["usage_subject"]["subject"].as_str(),
        Some("service-orphan")
    );
    assert_eq!(
        created.payload["usage_subject_asserted_by"].as_str(),
        Some("user:test")
    );
}
