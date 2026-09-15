mod test_state;

use ryeos_app::process::{ExecutionProcessIdentity, PROCESS_IDENTITY_SCHEMA_VERSION};
use ryeos_app::runtime_db::{
    DedicatedCandidateDisposition, NewCredentialProfile, NewDedicatedSession,
    NewRuntimeWorkspaceOperation, RuntimeActionMode, RuntimeWorkspaceBinding,
    RuntimeWorkspaceOperationPhase, WorkerProcessRecord, WorkerProcessState, WorkspaceBinding,
    WorkspaceState,
};
use ryeos_app::state_store::{
    FinalizeThreadRecord, NewDedicatedSessionCommand, NewEventRecord, NewThreadRecord,
};
use serde_json::{Value, json};

fn store_structured_session_capsule(state: &ryeos_app::state::AppState) -> (String, String, Value) {
    store_structured_session_capsule_with_schema(
        state,
        json!(ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION),
    )
}

fn store_structured_session_capsule_with_schema(
    state: &ryeos_app::state::AppState,
    schema: Value,
) -> (String, String, Value) {
    use ryeos_state::objects::{
        AdmittedDirectCommandClosure, AdmittedExecutionClosure, AdmittedLaunchArtifactIdentity,
        AdmittedPersistentSessionCapsule, AdmittedStructuredSessionProfile,
        DirectExecutableIdentity, DirectRootSourceIdentity, DirectRuntimeIdentity,
        DirectRuntimeSourceSpace, PERSISTENT_SESSION_CAPSULE_KIND,
        PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION, PersistentSessionLifecycleContract,
        PersistentSessionWireContract,
    };

    let contract = json!({"fixture":"terminal-replay"});
    let profile_hash = ryeos_state::objects::canonical_value_digest(&contract).unwrap();
    let schema_hashes =
        std::collections::BTreeMap::from([("request.json".to_owned(), "e".repeat(64))]);
    let exact_program = json!({"fixture":"terminal-replay"});
    let exact_program_hash = ryeos_state::objects::canonical_value_digest(&exact_program).unwrap();
    let executable_blob_hash = "9".repeat(64);
    let capsule = AdmittedPersistentSessionCapsule {
        schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
        kind: PERSISTENT_SESSION_CAPSULE_KIND.to_owned(),
        exact_program,
        exact_program_hash,
        lifecycle: PersistentSessionLifecycleContract {
            max_processes: 1,
            max_inflight_per_process: 1,
            max_address_space_bytes: 64 * 1024 * 1024,
            max_cpu_seconds: 1,
            real_uid_process_limit: 1,
            ready_timeout_ms: 1,
            request_timeout_ms: 1,
            idle_timeout_ms: 1,
        },
        wire: PersistentSessionWireContract {
            channel_env: "RYEOS_SESSION_FD".to_owned(),
            wire_protocol: "ryeos.structured-session".to_owned(),
            wire_version: 2,
            max_frame_bytes: 1024,
        },
        artifact_identity: AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executor_ref: "native:fixture".to_owned(),
            root_subject_source_content_digest: "a".repeat(64),
            root_subject_signer_fingerprint: Some("f".repeat(64)),
            root_subject_source_identity: DirectRootSourceIdentity::Bundle {
                manifest_hash: "b".repeat(64),
                manifest_signer_fingerprint: "f".repeat(64),
            },
            protocol_ref: "protocol:fixture/session".to_owned(),
            protocol_content_hash: "c".repeat(64),
            protocol_signer_fingerprint: "f".repeat(64),
            execution_plan_hash: "d".repeat(64),
            executable_identity: DirectExecutableIdentity::CapturedContent {
                content_hash: executable_blob_hash.clone(),
            },
            runtime_identity: DirectRuntimeIdentity {
                runtime_ref: "runtime:fixture/session".to_owned(),
                runtime_source_space: DirectRuntimeSourceSpace::Bundle,
                runtime_content_hash: "6".repeat(64),
                runtime_signer_fingerprint: "f".repeat(64),
                runtime_bundle_manifest_hash: Some("7".repeat(64)),
                runtime_bundle_signer_fingerprint: Some("f".repeat(64)),
            },
        },
        execution_closure: AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan: json!({}),
            protocol_descriptor_document: "fixture protocol".to_owned(),
            command: AdmittedDirectCommandClosure::ContentAddressed {
                executable_blob_hash: executable_blob_hash.clone(),
                execution_path: ryeos_state::objects::admitted_direct_command_execution_path(
                    &executable_blob_hash,
                    std::path::Path::new("fixture-session"),
                )
                .unwrap(),
            },
            admitted_project_root: None,
        },
        execution_realization_hash: "8".repeat(64),
        source_binding_hash: None,
        retained_product_selections: None,
        structured_session_profile: Some(AdmittedStructuredSessionProfile {
            profile_hash: profile_hash.clone(),
            contract,
            schema_hashes: schema_hashes.clone(),
            baseline_source: "baseline.toml".to_owned(),
            baseline_destination: "config.toml".to_owned(),
        }),
        executable_search: Vec::new(),
        process_environment: std::collections::BTreeMap::new(),
        runtime_ref: "runtime:fixture/session".to_owned(),
        executor_ref: "native:fixture".to_owned(),
    };
    let mut value = capsule.to_value().unwrap();
    value["schema"] = schema;
    if value["schema"] != json!(PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION) {
        // Deliberately opaque nested shape: classifying history must not decode
        // this predecessor as today's launch or protocol authority.
        value["structured_session_profile"] = json!({"obsolete_shape":true});
    }
    let hash = lillux::cas::CasStore::new(state.state_store.cas_root().unwrap())
        .store_object(&value)
        .unwrap();
    (
        hash,
        profile_hash,
        serde_json::to_value(schema_hashes).unwrap(),
    )
}

fn root_thread(thread_id: &str, owner: &str) -> NewThreadRecord {
    let hash = "a".repeat(64);
    NewThreadRecord {
        thread_id: thread_id.to_owned(),
        chain_root_id: thread_id.to_owned(),
        kind: "worker".to_owned(),
        item_ref: "worker:test/hosted".to_owned(),
        executor_ref: "runtime:test".to_owned(),
        launch_mode: "wait".to_owned(),
        current_site_id: "site:testhost".to_owned(),
        origin_site_id: "site:testhost".to_owned(),
        upstream_thread_id: None,
        requested_by: Some(owner.to_owned()),
        project_root: None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
            canonical_item_ref: "worker:test/hosted".to_owned(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(
                ),
            },
        }),
    }
}

fn command_fact(
    root: &str,
    event_type: &str,
    command_sequence: u64,
    request_digest: &str,
    worker_boot_epoch: u64,
    fields: Value,
) -> NewEventRecord {
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_command_fact.v1",
        "chain_root_id":root,
        "placement_thread_id":root,
        "command_sequence":command_sequence,
        "request_digest":request_digest,
        "event_type":event_type,
    }))
    .unwrap();
    let mut payload = fields.as_object().unwrap().clone();
    payload.insert("operation_id".to_owned(), Value::String(operation_id));
    payload.insert("chain_root_id".to_owned(), Value::String(root.to_owned()));
    payload.insert(
        "placement_thread_id".to_owned(),
        Value::String(root.to_owned()),
    );
    payload.insert(
        "command_sequence".to_owned(),
        Value::Number(command_sequence.into()),
    );
    payload.insert(
        "request_digest".to_owned(),
        Value::String(request_digest.to_owned()),
    );
    payload.insert(
        "worker_boot_epoch".to_owned(),
        Value::Number(worker_boot_epoch.into()),
    );
    NewEventRecord {
        event_type: event_type.to_owned(),
        storage_class: "indexed".to_owned(),
        payload: Value::Object(payload),
    }
}

fn turn_start_fact(
    root: &str,
    command_sequence: u64,
    request_digest: &str,
    worker_boot_epoch: u64,
    turn_id: &str,
) -> NewEventRecord {
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_turn_start.v1",
        "chain_root_id":root,
        "placement_thread_id":root,
        "worker_boot_epoch":worker_boot_epoch,
        "turn_id":turn_id,
    }))
    .unwrap();
    let batch_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_command_fact.v1",
        "chain_root_id":root,
        "placement_thread_id":root,
        "command_sequence":command_sequence,
        "request_digest":request_digest,
        "event_type":"hosted_worker_command_observation_batch",
    }))
    .unwrap();
    NewEventRecord {
        event_type: "hosted_session.turn_started".to_owned(),
        storage_class: "indexed".to_owned(),
        payload: json!({
            "schema":1,
            "operation_id":operation_id,
            "origin":"daemon_accepted_worker_observation",
            "chain_root_id":root,
            "placement_thread_id":root,
            "worker_boot_epoch":worker_boot_epoch,
            "turn_id":turn_id,
            "expected":"idle",
            "next":"turn_running",
            "command_sequence":command_sequence,
            "request_digest":request_digest,
            "source":{
                "kind":"command_response",
                "batch_operation_id":batch_operation_id,
                "command_sequence":command_sequence,
                "request_digest":request_digest,
            },
        }),
    }
}

struct CompletedTurnFixture {
    fence: ryeos_app::dedicated_session_service::HostedCommandCompletionFence,
    worker_instance_id: String,
    command_payload: Value,
    request_digest: String,
}

struct PendingTurnFixture {
    worker_instance_id: String,
    command_payload: Value,
    request_digest: String,
    sequence: u64,
    turn_id: String,
    result: Value,
}

fn seed_completed_turn_fixture(
    state: &ryeos_app::state::AppState,
    root: &str,
) -> CompletedTurnFixture {
    seed_completed_turn_fixture_with_progress(state, root, false)
}

fn seed_completed_turn_fixture_with_progress(
    state: &ryeos_app::state::AppState,
    root: &str,
    early_progress: bool,
) -> CompletedTurnFixture {
    let pending = seed_pending_turn_fixture(state, root, early_progress);
    let PendingTurnFixture {
        worker_instance_id,
        command_payload,
        request_digest,
        sequence,
        turn_id,
        result,
    } = pending;
    append_final_turn_batch(state, root, sequence, &request_digest, &result);
    let response_digest = ryeos_state::objects::canonical_value_digest(&result).unwrap();
    state.state_store.append_events(root, root, &[command_fact(
        root, "hosted_command.settled", sequence, &request_digest, 1,
        json!({"schema":1,"origin":"daemon_observed_io", "response_digest":response_digest,"succeeded":true}),
    )]).unwrap();
    state
        .state_store
        .settle_dedicated_command(root, sequence, 1, true, &result)
        .unwrap();
    project_turn_start(state, root, &turn_id);
    complete_turn(state, root, &turn_id);
    let observation =
        ryeos_app::dedicated_session_service::command_observation(state, root, sequence).unwrap();
    CompletedTurnFixture {
        fence: serde_json::from_value(observation["completion_fence"].clone()).unwrap(),
        worker_instance_id,
        command_payload,
        request_digest,
    }
}

fn seed_pending_turn_fixture(
    state: &ryeos_app::state::AppState,
    root: &str,
    early_progress: bool,
) -> PendingTurnFixture {
    seed_pending_turn_fixture_with_schema(
        state,
        root,
        early_progress,
        json!(ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION),
    )
}

fn seed_pending_turn_fixture_with_schema(
    state: &ryeos_app::state::AppState,
    root: &str,
    early_progress: bool,
    schema: Value,
) -> PendingTurnFixture {
    let owner = "fp:test-operator";
    let (capsule_hash, protocol_profile_hash, protocol_schema_hashes) =
        store_structured_session_capsule_with_schema(state, schema);
    let launch_owner = format!("claim-{root}");
    let launch_claim = state
        .state_store
        .reserve_fresh_thread_launch_active(root, &launch_owner, "daemon-test")
        .unwrap()
        .unwrap();
    state
        .state_store
        .create_thread_for_test(&root_thread(root, owner))
        .unwrap();
    state.state_store.mark_thread_running(root, None).unwrap();
    let workspace_id = format!("W-{root}");
    state
        .state_store
        .reserve_execution_workspace(
            &workspace_id,
            &"f".repeat(64),
            &format!("/tmp/{workspace_id}"),
        )
        .unwrap();
    state
        .state_store
        .transition_execution_workspace(
            &workspace_id,
            &[WorkspaceState::Reserved],
            WorkspaceState::Constructing,
            None,
        )
        .unwrap();
    state
        .state_store
        .claim_execution_workspace_construction(&workspace_id, root, &launch_claim.claimed_by)
        .unwrap();
    state
        .state_store
        .prepare_execution_workspace_backend(
            &workspace_id,
            root,
            &launch_claim.claimed_by,
            "test-backend",
            "1",
        )
        .unwrap();
    state
        .state_store
        .bind_execution_workspace(WorkspaceBinding {
            workspace_id: &workspace_id,
            workspace_output_partition_identity: None,
            base_output_capture_hash: None,
            thread_id: root,
            launch_owner: Some(&launch_claim.claimed_by),
            backend_id: Some("test-backend"),
            backend_version: Some("1"),
            pinned_root_identities: Some("{}"),
            mount_identity: Some("test-mount"),
        })
        .unwrap();
    let profile_id = format!("P-{root}");
    state
        .state_store
        .bind_thread_workspace(
            root,
            &RuntimeWorkspaceBinding {
                workspace_id: workspace_id.clone(),
                view_identity: "test-mount".to_owned(),
                borrower_launch_owner: launch_claim.owner.clone(),
            },
        )
        .unwrap();
    state
        .state_store
        .create_credential_profile(NewCredentialProfile {
            profile_id: &profile_id,
            owner_principal: owner,
            home_id: &format!("home-{root}"),
        })
        .unwrap();
    let worker_instance_id = format!("worker-{root}");
    state
        .state_store
        .admit_dedicated_session(NewDedicatedSession {
            placement_thread_id: root,
            chain_root_id: root,
            owner_principal: owner,
            admitted_capsule_hash: &capsule_hash,
            workspace_id: &workspace_id,
            candidate_required: false,
            candidate_disposition: DedicatedCandidateDisposition::OwnerDecision,
            credential_profile_id: &profile_id,
            credential_generation: 1,
            credential_lock_owner: &worker_instance_id,
        })
        .unwrap();
    let now = lillux::time::timestamp_millis() as i64;
    state
        .state_store
        .attach_worker_process(&WorkerProcessRecord {
            worker_instance_id: worker_instance_id.clone(),
            boot_identity_hash: "c".repeat(64),
            session_capsule_hash: capsule_hash.clone(),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: ExecutionProcessIdentity {
                schema_version: PROCESS_IDENTITY_SCHEMA_VERSION,
                process_scope: None,
                boot_id: format!("test-boot-{root}"),
                target_pid: 101,
                target_start_time_ticks: 10,
                group_leader_pid: 101,
                group_leader_start_time_ticks: 10,
            },
            control_channel_identity: format!("fd:{root}"),
            state: WorkerProcessState::Attached,
            daemon_generation_id: ryeos_app::runtime_db::daemon_generation_id().to_owned(),
            placement_thread_id: root.to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: now,
            updated_at_ms: now,
        })
        .unwrap();
    state
        .state_store
        .complete_worker_binding(&worker_instance_id, root, 1)
        .unwrap();
    state
        .state_store
        .bind_dedicated_remote_thread(root, &worker_instance_id, 1, "upstream-thread")
        .unwrap();

    let command_payload = json!({"route_id":"test.route","payload":{"value":1}});
    let request_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind":"route",
        "payload":command_payload,
    }))
    .unwrap();
    let command = state
        .state_store
        .reserve_dedicated_session_command(NewDedicatedSessionCommand {
            placement_thread_id: root,
            idempotency_key: &format!("settled-{root}"),
            worker_boot_epoch: 1,
            command_kind: "route",
            request_digest: &request_digest,
            payload: &command_payload,
        })
        .unwrap();
    let turn_id = format!("turn-{root}");
    let result = json!({
        "events":[],
        "session_observations":[{
            "kind":"state",
            "expected":"idle",
            "next":"turn_running",
            "turn_id":turn_id,
        }],
        "value":"retained",
    });
    let mut start = turn_start_fact(root, command.command_sequence, &request_digest, 1, &turn_id);
    let progress = early_progress.then(|| {
        let batch = json!({"events":[],"session_observations":result["session_observations"]});
        let progress = command_fact(
            root,
            "hosted_worker_command_progress",
            command.command_sequence,
            &request_digest,
            1,
            json!({"schema":1,"origin":"daemon_observed_io",
                "response_digest":ryeos_state::objects::canonical_value_digest(&batch).unwrap(),
                "canonical_batch":batch}),
        );
        start.payload["source"]["kind"] = json!("command_progress");
        start.payload["source"]["batch_operation_id"] = progress.payload["operation_id"].clone();
        progress
    });
    state
        .state_store
        .append_events(
            root,
            root,
            &[
                Some(command_fact(
                    root,
                    "hosted_command.committed",
                    command.command_sequence,
                    &request_digest,
                    1,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "command_kind":"route",
                        "route_id":"test.route",
                        "idempotency_key":format!("settled-{root}"),
                        "canonical_command":command_payload,
                        "admitted_session_capsule_hash":capsule_hash,
                        "protocol_profile_hash":protocol_profile_hash,
                        "protocol_schema_hashes":protocol_schema_hashes,
                    }),
                )),
                progress,
                Some(start),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
        )
        .unwrap();
    state
        .state_store
        .mark_dedicated_command_contacted(root, command.command_sequence, 1)
        .unwrap();
    PendingTurnFixture {
        worker_instance_id,
        command_payload,
        request_digest,
        sequence: command.command_sequence,
        turn_id,
        result,
    }
}

fn append_final_turn_batch(
    state: &ryeos_app::state::AppState,
    root: &str,
    sequence: u64,
    request_digest: &str,
    result: &Value,
) {
    state.state_store.append_events(root, root, &[command_fact(
        root, "hosted_worker_command_observation_batch", sequence, request_digest, 1,
        json!({"schema":1,"origin":"daemon_observed_io",
            "response_digest":ryeos_state::objects::canonical_value_digest(result).unwrap(),
            "canonical_batch":{"events":result["events"],"session_observations":result["session_observations"]}}),
    )]).unwrap();
}

fn project_turn_start(state: &ryeos_app::state::AppState, root: &str, turn_id: &str) {
    state
        .state_store
        .observe_dedicated_session_state(root, 1, "idle", "turn_running", None, Some(turn_id))
        .unwrap();
}

fn complete_turn(state: &ryeos_app::state::AppState, root: &str, turn_id: &str) {
    let mut terminal_batch = json!({
        "first_sequence":1,
        "count":1,
        "previous_digest":null,
        "events":[{"event_type":"turn.completed","payload":{"turn_id":turn_id}}],
        "session_observations":[{
            "kind":"state",
            "expected":"turn_running",
            "next":"idle",
            "completed_turn_id":turn_id,
        }],
    });
    terminal_batch["batch_digest"] =
        Value::String(ryeos_state::objects::canonical_value_digest(&terminal_batch).unwrap());
    ryeos_app::dedicated_session_service::ingest_observation_batch(state, root, 1, terminal_batch)
        .unwrap();
}

fn settle_pending_turn(
    state: &ryeos_app::state::AppState,
    root: &str,
    pending: &PendingTurnFixture,
) {
    append_final_turn_batch(
        state,
        root,
        pending.sequence,
        &pending.request_digest,
        &pending.result,
    );
    let response_digest = ryeos_state::objects::canonical_value_digest(&pending.result).unwrap();
    state
        .state_store
        .append_events(
            root,
            root,
            &[command_fact(
                root,
                "hosted_command.settled",
                pending.sequence,
                &pending.request_digest,
                1,
                json!({"schema":1,"origin":"daemon_observed_io","response_digest":response_digest,"succeeded":true}),
            )],
        )
        .unwrap();
    state
        .state_store
        .settle_dedicated_command(root, pending.sequence, 1, true, &pending.result)
        .unwrap();
    project_turn_start(state, root, &pending.turn_id);
}

fn seed_workload_child(
    state: &ryeos_app::state::AppState,
    root: &str,
    worker_instance_id: &str,
    upstream_session_id: &str,
    turn_id: &str,
    call_id: &str,
    child_thread_id: &str,
    child_owner: &str,
) {
    seed_workspace_child(
        state,
        root,
        worker_instance_id,
        ryeos_runtime::workload_client::WorkloadInvocationSource::StructuredSession {
            upstream_session_id: upstream_session_id.to_owned(),
            operation_id: turn_id.to_owned(),
            call_id: call_id.to_owned(),
        },
        child_thread_id,
        child_thread_id,
        child_owner,
    );
}

fn seed_workspace_child(
    state: &ryeos_app::state::AppState,
    root: &str,
    worker_instance_id: &str,
    invocation: ryeos_runtime::workload_client::WorkloadInvocationSource,
    child_thread_id: &str,
    child_chain_root_id: &str,
    child_owner: &str,
) {
    let session = state.state_store.dedicated_session(root).unwrap().unwrap();
    let worker = state
        .state_store
        .worker_process(worker_instance_id)
        .unwrap()
        .unwrap();
    assert_eq!(worker.placement_thread_id, root);
    assert_eq!(worker.boot_epoch, 1);
    assert_eq!(
        worker.daemon_generation_id,
        ryeos_app::runtime_db::daemon_generation_id()
    );
    assert_eq!(worker.session_capsule_hash, session.admitted_capsule_hash);
    assert_eq!(worker.state, WorkerProcessState::Live);
    assert_eq!(worker.cleanup_state, "owned");
    let grant_digest = "d".repeat(64);
    let operation_id = invocation.runtime_operation_id(&grant_digest).unwrap();
    let project_authority_digest = "e".repeat(64);
    state
        .state_store
        .reserve_runtime_action_intent_with_workspace(
            &operation_id,
            root,
            RuntimeActionMode::Inline,
            &"f".repeat(64),
            child_thread_id,
            None,
            &NewRuntimeWorkspaceOperation {
                workspace_id: &session.workspace_id,
                access: ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
                worker_instance_id,
                worker_boot_epoch: 1,
                worker_boot_identity_hash: &worker.boot_identity_hash,
                project_authority_digest: &project_authority_digest,
                workload_client_grant_digest: &grant_digest,
                invocation,
            },
        )
        .unwrap();
    state
        .state_store
        .transition_runtime_workspace_operation(
            &operation_id,
            &[RuntimeWorkspaceOperationPhase::Reserved],
            RuntimeWorkspaceOperationPhase::Quiescing,
        )
        .unwrap();
    state
        .state_store
        .bind_runtime_workspace_input_generation(
            &operation_id,
            &ryeos_state::objects::WorkspaceGenerationPair {
                snapshot_hash: "a".repeat(64),
                output_capture_hash: None,
            },
        )
        .unwrap();
    state
        .state_store
        .transition_runtime_workspace_operation(
            &operation_id,
            &[RuntimeWorkspaceOperationPhase::Quiesced],
            RuntimeWorkspaceOperationPhase::ChildRunning,
        )
        .unwrap();
    let mut child = root_thread(child_thread_id, child_owner);
    child.chain_root_id = child_chain_root_id.to_owned();
    if child_chain_root_id != child_thread_id {
        child.upstream_thread_id = Some(root.to_owned());
        child.captured_history_policy = None;
    }
    state.state_store.create_thread_for_test(&child).unwrap();
    state
        .state_store
        .mark_thread_running(child_thread_id, None)
        .unwrap();
    state
        .state_store
        .finalize_thread(
            child_thread_id,
            &FinalizeThreadRecord {
                status: "completed".to_owned(),
                outcome_code: None,
                result_json: Some(json!({"child_thread_id":child_thread_id,"ok":true})),
                error_json: None,
                artifacts: vec![],
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();
    assert!(
        state
            .state_store
            .settle_runtime_workspace_operation(&operation_id)
            .unwrap()
    );
}

fn seed_nonworkload_child(state: &ryeos_app::state::AppState, root: &str, child_thread_id: &str) {
    state
        .state_store
        .reserve_runtime_action_intent(
            &ryeos_state::objects::canonical_value_digest(&json!({
                "kind":"non-workload-fixture",
                "child_thread_id":child_thread_id,
            }))
            .unwrap(),
            root,
            RuntimeActionMode::Inline,
            &"4".repeat(64),
            child_thread_id,
            None,
        )
        .unwrap();
    state
        .state_store
        .create_thread_for_test(&root_thread(child_thread_id, "fp:test-operator"))
        .unwrap();
    state
        .state_store
        .mark_thread_running(child_thread_id, None)
        .unwrap();
    state
        .state_store
        .finalize_thread(
            child_thread_id,
            &FinalizeThreadRecord {
                status: "completed".to_owned(),
                outcome_code: None,
                result_json: Some(json!({"child_thread_id":child_thread_id,"ok":true})),
                error_json: None,
                artifacts: vec![],
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();
}

#[test]
fn command_observation_projects_only_children_of_its_exact_turn() {
    let (tmp, state) = test_state::build_test_state();
    let root = "T-exact-turn-children";
    let pending = seed_pending_turn_fixture(&state, root, false);
    settle_pending_turn(&state, root, &pending);
    for (call_id, child) in [("call-one", "T-child-one"), ("call-two", "T-child-two")] {
        seed_workload_child(
            &state,
            root,
            &pending.worker_instance_id,
            "upstream-thread",
            &pending.turn_id,
            call_id,
            child,
            "fp:test-operator",
        );
    }
    seed_workspace_child(
        &state,
        root,
        &pending.worker_instance_id,
        ryeos_runtime::workload_client::WorkloadInvocationSource::Cli {
            external_request_id: "cli-other".to_owned(),
        },
        "T-child-cli",
        "T-child-cli",
        "fp:test-operator",
    );
    seed_nonworkload_child(&state, root, "T-child-nonworkload");

    // Model a retained callback from another upstream session using the
    // production reservation path, then restore the current session
    // coordinate. This is deliberately a projection-corruption fixture: the
    // normal admission path would refuse such a callback while this turn is
    // current, but observation must still exclude it if retained state ever
    // contains it.
    let projection = rusqlite::Connection::open(&state.config.db_path).unwrap();
    projection
        .execute(
            "UPDATE dedicated_session SET remote_thread_id='other-upstream', current_turn_id='other-turn' WHERE placement_thread_id=?1",
            [root],
        )
        .unwrap();
    seed_workload_child(
        &state,
        root,
        &pending.worker_instance_id,
        "other-upstream",
        "other-turn",
        "call-other-session",
        "T-child-other-session",
        "fp:test-operator",
    );
    projection
        .execute(
            "UPDATE dedicated_session SET remote_thread_id='upstream-thread', current_turn_id=?2 WHERE placement_thread_id=?1",
            rusqlite::params![root, pending.turn_id],
        )
        .unwrap();
    drop(projection);

    let running_session = state.state_store.dedicated_session(root).unwrap().unwrap();
    let running_command = state
        .state_store
        .dedicated_session_command(root, pending.sequence)
        .unwrap()
        .unwrap();
    let running_history = serde_json::to_value(
        state
            .state_store
            .get_authoritative_root_thread_snapshot(root)
            .unwrap(),
    )
    .unwrap();
    let running =
        ryeos_app::dedicated_session_service::command_observation(&state, root, pending.sequence)
            .unwrap();
    assert_eq!(running["operation"]["state"], "running");
    assert!(running.get("completion_fence").is_none());
    assert!(running.get("child_executions").is_none());
    assert_eq!(
        state.state_store.dedicated_session(root).unwrap().unwrap(),
        running_session
    );
    assert_eq!(
        state
            .state_store
            .dedicated_session_command(root, pending.sequence)
            .unwrap()
            .unwrap(),
        running_command
    );
    assert_eq!(
        serde_json::to_value(
            state
                .state_store
                .get_authoritative_root_thread_snapshot(root)
                .unwrap()
        )
        .unwrap(),
        running_history
    );
    complete_turn(&state, root, &pending.turn_id);

    let before_session = state.state_store.dedicated_session(root).unwrap().unwrap();
    let before_command = state
        .state_store
        .dedicated_session_command(root, pending.sequence)
        .unwrap()
        .unwrap();
    let before_history = serde_json::to_value(
        state
            .state_store
            .get_authoritative_root_thread_snapshot(root)
            .unwrap(),
    )
    .unwrap();
    let before_intents = state.state_store.runtime_action_intents().unwrap().len();
    let first =
        ryeos_app::dedicated_session_service::command_observation(&state, root, pending.sequence)
            .unwrap();
    let children = first["child_executions"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0]["invocation"]["call_id"], "call-one");
    assert_eq!(children[1]["invocation"]["call_id"], "call-two");
    assert_eq!(
        state.state_store.dedicated_session(root).unwrap().unwrap(),
        before_session
    );
    assert_eq!(
        state
            .state_store
            .dedicated_session_command(root, pending.sequence)
            .unwrap()
            .unwrap(),
        before_command
    );
    assert_eq!(
        serde_json::to_value(
            state
                .state_store
                .get_authoritative_root_thread_snapshot(root)
                .unwrap()
        )
        .unwrap(),
        before_history
    );
    assert_eq!(
        state.state_store.runtime_action_intents().unwrap().len(),
        before_intents
    );

    state
        .state_store
        .observe_dedicated_session_state(root, 1, "idle", "turn_running", None, Some("later-turn"))
        .unwrap();
    seed_workload_child(
        &state,
        root,
        &pending.worker_instance_id,
        first["child_executions"][0]["invocation"]["upstream_session_id"]
            .as_str()
            .unwrap(),
        "later-turn",
        "later-call",
        "T-child-later",
        "fp:test-operator",
    );
    let replay =
        ryeos_app::dedicated_session_service::command_observation(&state, root, pending.sequence)
            .unwrap();
    assert_eq!(replay["child_executions"], first["child_executions"]);
    assert_eq!(
        state.state_store.runtime_action_intents().unwrap().len(),
        before_intents + 1
    );

    drop(state);
    let reopened = test_state::reopen_test_state(&tmp);
    let reconstructed = ryeos_app::dedicated_session_service::command_observation(
        &reopened,
        root,
        pending.sequence,
    )
    .unwrap();
    assert_eq!(reconstructed["child_executions"], first["child_executions"]);
    assert_eq!(reconstructed["completion_fence"], first["completion_fence"]);
}

#[test]
fn command_observation_refuses_a_child_with_contradictory_owner() {
    let (_tmp, state) = test_state::build_test_state();
    let root = "T-contradictory-child-owner";
    let pending = seed_pending_turn_fixture(&state, root, false);
    settle_pending_turn(&state, root, &pending);
    let upstream = state
        .state_store
        .dedicated_session(root)
        .unwrap()
        .unwrap()
        .remote_thread_id
        .unwrap();
    seed_workload_child(
        &state,
        root,
        &pending.worker_instance_id,
        &upstream,
        &pending.turn_id,
        "call-wrong-owner",
        "T-child-wrong-owner",
        "fp:other-owner",
    );
    complete_turn(&state, root, &pending.turn_id);
    let error =
        ryeos_app::dedicated_session_service::command_observation(&state, root, pending.sequence)
            .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("contradicts its placement ownership"),
        "{error:#}"
    );
}

#[test]
fn command_observation_refuses_a_child_with_contradictory_root() {
    let (_tmp, state) = test_state::build_test_state();
    let root = "T-contradictory-child-root";
    let pending = seed_pending_turn_fixture(&state, root, false);
    settle_pending_turn(&state, root, &pending);
    seed_workspace_child(
        &state,
        root,
        &pending.worker_instance_id,
        ryeos_runtime::workload_client::WorkloadInvocationSource::StructuredSession {
            upstream_session_id: "upstream-thread".to_owned(),
            operation_id: pending.turn_id.clone(),
            call_id: "call-wrong-root".to_owned(),
        },
        "T-child-wrong-root",
        root,
        "fp:test-operator",
    );
    complete_turn(&state, root, &pending.turn_id);
    let error =
        ryeos_app::dedicated_session_service::command_observation(&state, root, pending.sequence)
            .unwrap_err();
    assert!(
        error.to_string().contains("has no authoritative snapshot"),
        "{error:#}"
    );
}

#[tokio::test]
async fn terminal_predecessor_command_history_is_preserved_without_replay() {
    let (_tmp, state) = test_state::build_test_state();
    let root = "T-terminal-predecessor";
    let pending = seed_pending_turn_fixture_with_schema(
        &state,
        root,
        false,
        json!(ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION - 1),
    );
    // Neither a live placement nor an unproved cleanup is historical authority.
    assert!(ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).is_err());
    state
        .state_store
        .settle_worker_process(&pending.worker_instance_id, root, 1, "unproved", "fixture")
        .unwrap();
    assert!(ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).is_err());
    state
        .state_store
        .settle_worker_process(&pending.worker_instance_id, root, 1, "reaped", "fixture")
        .unwrap();
    // Use the normal terminal transition, then restart fencing is deliberately
    // not sufficient: this still-attached terminal must not take the opaque path.
    state
        .state_store
        .terminalize_dedicated_session(root, &pending.worker_instance_id, 1, "fixture")
        .unwrap();
    finalize_fixture_thread(&state, root);
    assert!(ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).is_err());

    // A separate placement exercises the exact installed failure: its prior
    // worker was fenced and detached before the session/root became terminal.
    let (_tmp, state) = test_state::build_test_state();
    let pending = seed_pending_turn_fixture_with_schema(
        &state,
        root,
        false,
        json!(ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION - 1),
    );
    retire_fixture_session(&state, root, &pending);
    let session_before =
        serde_json::to_value(state.state_store.dedicated_session(root).unwrap()).unwrap();
    let command_before = serde_json::to_value(
        state
            .state_store
            .dedicated_session_command(root, pending.sequence)
            .unwrap(),
    )
    .unwrap();
    let history_before = serde_json::to_value(
        state
            .state_store
            .get_authoritative_root_thread_snapshot(root)
            .unwrap(),
    )
    .unwrap();
    for _ in 0..2 {
        ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).unwrap();
        ryeos_app::dedicated_session_service::reconcile_observation_outboxes(&state).unwrap();
        assert_eq!(
            serde_json::to_value(state.state_store.dedicated_session(root).unwrap()).unwrap(),
            session_before
        );
        assert_eq!(
            serde_json::to_value(
                state
                    .state_store
                    .dedicated_session_command(root, pending.sequence)
                    .unwrap()
            )
            .unwrap(),
            command_before
        );
        assert_eq!(
            serde_json::to_value(
                state
                    .state_store
                    .get_authoritative_root_thread_snapshot(root)
                    .unwrap()
            )
            .unwrap(),
            history_before
        );
        assert!(
            ryeos_app::dedicated_session_service::command_observation(
                &state,
                root,
                pending.sequence
            )
            .is_err()
        );
    }
    // Model an orphaned unproved boot not present in the detached slot. The
    // existing indexed cleanup owner, not slot absence, must refuse retention.
    let projection = rusqlite::Connection::open(&state.config.db_path).unwrap();
    projection
        .execute(
            "UPDATE worker_process SET cleanup_state='unproved' WHERE worker_instance_id=?1",
            [&pending.worker_instance_id],
        )
        .unwrap();
    assert!(ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).is_err());
    projection
        .execute(
            "UPDATE worker_process SET cleanup_state='reaped' WHERE worker_instance_id=?1",
            [&pending.worker_instance_id],
        )
        .unwrap();
    ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).unwrap();

    // A stale/corrupt session row may not select another old capsule to evade
    // exact immutable command association checks.
    let original_capsule = state
        .state_store
        .dedicated_session(root)
        .unwrap()
        .unwrap()
        .admitted_capsule_hash;
    let (other_capsule, _, _) = store_structured_session_capsule_with_schema(
        &state,
        json!(ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION - 2),
    );
    projection
        .execute(
            "UPDATE dedicated_session SET admitted_capsule_hash=?1 WHERE placement_thread_id=?2",
            [&other_capsule, root],
        )
        .unwrap();
    assert!(ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).is_err());
    projection
        .execute(
            "UPDATE dedicated_session SET admitted_capsule_hash=?1 WHERE placement_thread_id=?2",
            [&original_capsule, root],
        )
        .unwrap();
    drop(projection);

    // The old row must not stop current unrelated commands from being repaired.
    let current = seed_pending_turn_fixture(&state, "T-current-alongside-history", false);
    append_final_turn_batch(
        &state,
        "T-current-alongside-history",
        current.sequence,
        &current.request_digest,
        &current.result,
    );
    ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).unwrap();
    assert_eq!(
        state
            .state_store
            .dedicated_session_command("T-current-alongside-history", current.sequence)
            .unwrap()
            .unwrap()
            .state,
        "completed"
    );
}

fn finalize_fixture_thread(state: &ryeos_app::state::AppState, root: &str) {
    state
        .state_store
        .finalize_thread(
            root,
            &FinalizeThreadRecord {
                status: "failed".to_owned(),
                outcome_code: None,
                result_json: None,
                error_json: Some(json!({"fixture":"retired"})),
                artifacts: vec![],
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();
}

fn retire_fixture_session(
    state: &ryeos_app::state::AppState,
    root: &str,
    pending: &PendingTurnFixture,
) {
    state
        .state_store
        .fence_abandoned_worker_process(&pending.worker_instance_id, root, 1, "reaped")
        .unwrap();
    state
        .state_store
        .terminalize_unattached_dedicated_session(root, "fixture")
        .unwrap();
    finalize_fixture_thread(state, root);
}

#[tokio::test]
async fn terminal_history_does_not_hide_malformed_or_future_session_capsules() {
    for schema in [
        Value::Null,
        json!(0),
        json!(-1),
        json!("10"),
        json!(1.5),
        json!(ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION + 1),
    ] {
        let (_tmp, state) = test_state::build_test_state();
        let root = "T-invalid-terminal-capsule";
        let pending = seed_pending_turn_fixture_with_schema(&state, root, false, schema);
        retire_fixture_session(&state, root, &pending);
        assert!(ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).is_err());
    }
}

#[tokio::test]
async fn command_progress_recovery_preserves_the_exact_crash_frontier() {
    for (label, projected, completed, final_batch) in [
        ("before-projection", false, false, false),
        ("before-ack", true, false, false),
        ("before-settlement", true, false, true),
        ("completion-before-settlement", true, true, true),
    ] {
        let (_tmp, state) = test_state::build_test_state();
        let root = format!("T-progress-{label}");
        let pending = seed_pending_turn_fixture(&state, &root, true);
        if projected {
            project_turn_start(&state, &root, &pending.turn_id);
        }
        if completed {
            complete_turn(&state, &root, &pending.turn_id);
        }
        if final_batch {
            append_final_turn_batch(
                &state,
                &root,
                pending.sequence,
                &pending.request_digest,
                &pending.result,
            );
        }
        for _ in 0..2 {
            ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).unwrap();
            let command = state
                .state_store
                .dedicated_session_command(&root, pending.sequence)
                .unwrap()
                .unwrap();
            let session = state.state_store.dedicated_session(&root).unwrap().unwrap();
            assert_eq!(
                command.state,
                if final_batch {
                    "completed"
                } else {
                    "outcome_unknown"
                },
                "{label}"
            );
            if completed {
                assert_eq!(session.state, "idle", "{label}");
                assert!(session.current_turn_id.is_none());
                let observed = ryeos_app::dedicated_session_service::command_observation(
                    &state,
                    &root,
                    pending.sequence,
                )
                .unwrap();
                assert_eq!(observed["completion_fence"]["turn_id"], pending.turn_id);
            } else {
                assert_eq!(
                    session.current_turn_id.as_deref(),
                    Some(pending.turn_id.as_str()),
                    "{label}"
                );
                assert_eq!(
                    session.state,
                    if final_batch {
                        "turn_running"
                    } else {
                        "outcome_unknown"
                    },
                    "{label}"
                );
            }
            if final_batch {
                assert_eq!(
                    command.result.as_ref().unwrap()["response_digest"],
                    ryeos_state::objects::canonical_value_digest(&pending.result).unwrap()
                );
            }
        }
        let replay = state
            .state_store
            .replay_events(&root, Some(&root), None, 128, 1024 * 1024)
            .unwrap();
        let starts = replay
            .events
            .iter()
            .filter(|event| event.event_type == "hosted_session.turn_started")
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 1, "{label}");
        assert_eq!(starts[0].payload["source"]["kind"], "command_progress");
    }
}

#[tokio::test]
async fn command_progress_recovery_refuses_a_conflicting_final_start() {
    let (_tmp, state) = test_state::build_test_state();
    let root = "T-progress-conflicting-final";
    let pending = seed_pending_turn_fixture(&state, root, true);
    project_turn_start(&state, root, &pending.turn_id);
    let mut bad_result = pending.result.clone();
    bad_result["session_observations"][0]["turn_id"] = json!("different-turn");
    append_final_turn_batch(
        &state,
        root,
        pending.sequence,
        &pending.request_digest,
        &bad_result,
    );
    assert!(ryeos_app::dedicated_session_service::reconcile_command_outboxes(&state).is_err());
    let command = state
        .state_store
        .dedicated_session_command(root, pending.sequence)
        .unwrap()
        .unwrap();
    assert_eq!(command.state, "dispatched");
    let session = state.state_store.dedicated_session(root).unwrap().unwrap();
    assert_eq!(
        session.current_turn_id.as_deref(),
        Some(pending.turn_id.as_str())
    );
}

#[tokio::test]
async fn completed_command_retains_its_early_progress_start_authority() {
    let (_tmp, state) = test_state::build_test_state();
    let root = "T-progress-completed-fence";
    let fixture = seed_completed_turn_fixture_with_progress(&state, root, true);
    let observed = ryeos_app::dedicated_session_service::command_observation(
        &state,
        root,
        fixture.fence.command_sequence,
    )
    .unwrap();
    assert_eq!(observed["operation"]["state"], "completed");
    assert_eq!(
        observed["completion_fence"],
        serde_json::to_value(&fixture.fence).unwrap()
    );
    let replay = state
        .state_store
        .replay_events(root, Some(root), None, 128, 1024 * 1024)
        .unwrap();
    let starts = replay
        .events
        .iter()
        .filter(|event| event.event_type == "hosted_session.turn_started")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].payload["source"]["kind"], "command_progress");
}

#[tokio::test]
async fn completed_termination_requires_the_exact_immutable_turn_fence_and_frontier() {
    let (_tmp, state) = test_state::build_test_state();
    let root = "T-completed-fence";
    let fixture = seed_completed_turn_fixture(&state, root);

    ryeos_app::dedicated_session_service::terminate_session(&state, root, "completed", None)
        .await
        .expect_err("mutable idle state cannot replace an exact completed-turn fence");

    let mut mutations = Vec::new();
    let mut changed = fixture.fence.clone();
    changed.placement_thread_id = "T-other-placement".to_owned();
    mutations.push(changed);
    let mut changed = fixture.fence.clone();
    changed.admitted_capsule_hash = "0".repeat(64);
    mutations.push(changed);
    let mut changed = fixture.fence.clone();
    changed.worker_boot_epoch = 2;
    mutations.push(changed);
    let mut changed = fixture.fence.clone();
    changed.command_sequence += 1;
    mutations.push(changed);
    let mut changed = fixture.fence.clone();
    changed.request_digest = "0".repeat(64);
    mutations.push(changed);
    let mut changed = fixture.fence.clone();
    changed.turn_id = "turn-other".to_owned();
    mutations.push(changed);
    let mut changed = fixture.fence.clone();
    changed.completion_operation_id = "0".repeat(64);
    mutations.push(changed);
    for changed in mutations {
        ryeos_app::dedicated_session_service::terminate_session(
            &state,
            root,
            "completed",
            Some(&changed),
        )
        .await
        .expect_err("mutated completion fence must fail closed");
    }
    ryeos_app::dedicated_session_service::terminate_session(
        &state,
        root,
        "cancelled",
        Some(&fixture.fence),
    )
    .await
    .expect_err("cancelled termination cannot claim completed-turn authority");

    let completed = ryeos_app::dedicated_session_service::terminate_session(
        &state,
        root,
        "completed",
        Some(&fixture.fence),
    )
    .await
    .unwrap();
    assert_eq!(completed["state"], "terminal");
    let retry = ryeos_app::dedicated_session_service::terminate_session(
        &state,
        root,
        "completed",
        Some(&fixture.fence),
    )
    .await
    .unwrap();
    assert_eq!(retry["idempotent"], true);

    let frontier_root = "T-completed-fence-frontier";
    let frontier = seed_completed_turn_fixture(&state, frontier_root);
    let later_payload = json!({"route_id":"test.later","payload":{}});
    let later_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind":"route",
        "payload":later_payload,
    }))
    .unwrap();
    let later = state
        .state_store
        .reserve_dedicated_session_command(NewDedicatedSessionCommand {
            placement_thread_id: frontier_root,
            idempotency_key: "later-route",
            worker_boot_epoch: 1,
            command_kind: "route",
            request_digest: &later_digest,
            payload: &later_payload,
        })
        .unwrap();
    state
        .state_store
        .mark_dedicated_command_contacted(frontier_root, later.command_sequence, 1)
        .unwrap();
    state
        .state_store
        .settle_dedicated_command(
            frontier_root,
            later.command_sequence,
            1,
            true,
            &json!({"value":"later route settled"}),
        )
        .unwrap();
    assert_eq!(
        state
            .state_store
            .dedicated_session(frontier_root)
            .unwrap()
            .unwrap()
            .state,
        "idle"
    );
    let error = ryeos_app::dedicated_session_service::terminate_session(
        &state,
        frontier_root,
        "completed",
        Some(&frontier.fence),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("completed termination command frontier has advanced"),
        "{error:#}"
    );

    let recovered_root = "T-completed-fence-recovered";
    let recovered = seed_completed_turn_fixture(&state, recovered_root);
    state
        .state_store
        .fence_abandoned_worker_process(&recovered.worker_instance_id, recovered_root, 1, "reaped")
        .unwrap();
    let recovered_worker = format!("worker-{recovered_root}-epoch-2");
    state
        .state_store
        .acquire_credential_profile(
            &format!("P-{recovered_root}"),
            "fp:test-operator",
            &recovered_worker,
        )
        .unwrap();
    assert_eq!(
        state
            .state_store
            .prepare_dedicated_session_recovery(
                recovered_root,
                1,
                &recovered_worker,
                &format!("W-{recovered_root}")
            )
            .unwrap(),
        2
    );
    let now = lillux::time::timestamp_millis() as i64;
    state
        .state_store
        .attach_worker_process(&WorkerProcessRecord {
            worker_instance_id: recovered_worker.clone(),
            boot_identity_hash: "d".repeat(64),
            session_capsule_hash: recovered.fence.admitted_capsule_hash.clone(),
            boot_epoch: 2,
            lifecycle_generation: 2,
            process_identity: ExecutionProcessIdentity {
                schema_version: PROCESS_IDENTITY_SCHEMA_VERSION,
                process_scope: None,
                boot_id: "test-boot-recovered-2".to_owned(),
                target_pid: 102,
                target_start_time_ticks: 20,
                group_leader_pid: 102,
                group_leader_start_time_ticks: 20,
            },
            control_channel_identity: "fd:recovered-2".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-test-2".to_owned(),
            placement_thread_id: recovered_root.to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: now,
            updated_at_ms: now,
        })
        .unwrap();
    state
        .state_store
        .complete_worker_binding(&recovered_worker, recovered_root, 2)
        .unwrap();
    let reattach_payload = json!({"upstream_session_id":"upstream-thread"});
    let reattach = state
        .state_store
        .reserve_dedicated_session_command(NewDedicatedSessionCommand {
            placement_thread_id: recovered_root,
            idempotency_key: "reattach-recovered-2",
            worker_boot_epoch: 2,
            command_kind: "reattach",
            request_digest: &"e".repeat(64),
            payload: &reattach_payload,
        })
        .unwrap();
    state
        .state_store
        .mark_dedicated_command_contacted(recovered_root, reattach.command_sequence, 2)
        .unwrap();
    state
        .state_store
        .settle_recovered_dedicated_command(
            recovered_root,
            reattach.command_sequence,
            2,
            &json!({"redacted":true}),
        )
        .unwrap();
    state
        .state_store
        .settle_dedicated_remote_recovery_status(recovered_root, 2, "upstream-thread", "safe_idle")
        .unwrap();
    assert_eq!(
        ryeos_app::dedicated_session_service::command_observation(
            &state,
            recovered_root,
            recovered.fence.command_sequence,
        )
        .unwrap()["completion_fence"]["worker_boot_epoch"],
        1
    );
    let historical = ryeos_app::dedicated_session_service::terminate_session(
        &state,
        recovered_root,
        "completed",
        Some(&recovered.fence),
    )
    .await
    .unwrap();
    assert_eq!(historical["state"], "terminal");

    assert_eq!(fixture.command_payload["route_id"], "test.route");
    assert!(lillux::valid_hash(&fixture.request_digest));
}

#[tokio::test]
async fn terminal_root_replays_only_exact_authoritatively_settled_command() {
    let (_tmp, state) = test_state::build_test_state();
    let root = "T-terminal-hosted-replay";
    let owner = "fp:test-operator";
    let (capsule_hash, protocol_profile_hash, protocol_schema_hashes) =
        store_structured_session_capsule(&state);
    let launch_claim = state
        .state_store
        .reserve_fresh_thread_launch_active(root, "claim-terminal-hosted-replay", "daemon-test")
        .unwrap()
        .unwrap();
    state
        .state_store
        .create_thread_for_test(&root_thread(root, owner))
        .unwrap();
    state.state_store.mark_thread_running(root, None).unwrap();
    state
        .state_store
        .reserve_execution_workspace(
            "W-terminal-hosted-replay",
            &"f".repeat(64),
            "/tmp/W-terminal-hosted-replay",
        )
        .unwrap();
    state
        .state_store
        .transition_execution_workspace(
            "W-terminal-hosted-replay",
            &[WorkspaceState::Reserved],
            WorkspaceState::Constructing,
            None,
        )
        .unwrap();
    state
        .state_store
        .claim_execution_workspace_construction(
            "W-terminal-hosted-replay",
            root,
            &launch_claim.claimed_by,
        )
        .unwrap();
    state
        .state_store
        .prepare_execution_workspace_backend(
            "W-terminal-hosted-replay",
            root,
            &launch_claim.claimed_by,
            "test-backend",
            "1",
        )
        .unwrap();
    state
        .state_store
        .bind_execution_workspace(WorkspaceBinding {
            workspace_id: "W-terminal-hosted-replay",
            workspace_output_partition_identity: None,
            base_output_capture_hash: None,
            thread_id: root,
            launch_owner: Some(&launch_claim.claimed_by),
            backend_id: Some("test-backend"),
            backend_version: Some("1"),
            pinned_root_identities: Some("{}"),
            mount_identity: Some("test-mount"),
        })
        .unwrap();
    state
        .state_store
        .bind_thread_workspace(
            root,
            &RuntimeWorkspaceBinding {
                workspace_id: "W-terminal-hosted-replay".to_owned(),
                view_identity: "test-mount".to_owned(),
                borrower_launch_owner: launch_claim.owner.clone(),
            },
        )
        .unwrap();
    state
        .state_store
        .create_credential_profile(NewCredentialProfile {
            profile_id: "P-terminal-hosted-replay",
            owner_principal: owner,
            home_id: "home-terminal-hosted-replay",
        })
        .unwrap();
    state
        .state_store
        .admit_dedicated_session(NewDedicatedSession {
            placement_thread_id: root,
            chain_root_id: root,
            owner_principal: owner,
            admitted_capsule_hash: &capsule_hash,
            workspace_id: "W-terminal-hosted-replay",
            candidate_required: false,
            candidate_disposition: DedicatedCandidateDisposition::OwnerDecision,
            credential_profile_id: "P-terminal-hosted-replay",
            credential_generation: 1,
            credential_lock_owner: "worker-terminal-hosted-replay",
        })
        .unwrap();
    let now = lillux::time::timestamp_millis() as i64;
    state
        .state_store
        .attach_worker_process(&WorkerProcessRecord {
            worker_instance_id: "worker-terminal-hosted-replay".to_owned(),
            boot_identity_hash: "c".repeat(64),
            session_capsule_hash: capsule_hash.clone(),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: ExecutionProcessIdentity {
                schema_version: PROCESS_IDENTITY_SCHEMA_VERSION,
                process_scope: None,
                boot_id: "test-boot".to_owned(),
                target_pid: 101,
                target_start_time_ticks: 10,
                group_leader_pid: 101,
                group_leader_start_time_ticks: 10,
            },
            control_channel_identity: "fd:test".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-test".to_owned(),
            placement_thread_id: root.to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: now,
            updated_at_ms: now,
        })
        .unwrap();
    state
        .state_store
        .complete_worker_binding("worker-terminal-hosted-replay", root, 1)
        .unwrap();
    state
        .state_store
        .bind_dedicated_remote_thread(
            root,
            "worker-terminal-hosted-replay",
            1,
            "upstream-terminal-hosted-replay",
        )
        .unwrap();
    let command_payload = json!({"route_id":"test.route","payload":{"value":1}});
    let request_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind":"route",
        "payload":command_payload,
    }))
    .unwrap();
    let settled = state
        .state_store
        .reserve_dedicated_session_command(NewDedicatedSessionCommand {
            placement_thread_id: root,
            idempotency_key: "settled-key",
            worker_boot_epoch: 1,
            command_kind: "route",
            request_digest: &request_digest,
            payload: &command_payload,
        })
        .unwrap();
    let turn_id = "turn-terminal-hosted-replay";
    let result = json!({
        "events":[],
        "session_observations":[{
            "kind":"state",
            "expected":"idle",
            "next":"turn_running",
            "turn_id":turn_id,
        }],
        "value":"retained",
    });
    let response_digest = ryeos_state::objects::canonical_value_digest(&result).unwrap();
    state
        .state_store
        .append_events(
            root,
            root,
            &[
                command_fact(
                    root,
                    "hosted_command.committed",
                    settled.command_sequence,
                    &request_digest,
                    1,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "command_kind":"route",
                        "route_id":"test.route",
                        "idempotency_key":"settled-key",
                        "canonical_command":command_payload,
                        "admitted_session_capsule_hash":capsule_hash,
                        "protocol_profile_hash":protocol_profile_hash.clone(),
                        "protocol_schema_hashes":protocol_schema_hashes.clone(),
                    }),
                ),
                command_fact(
                    root,
                    "hosted_worker_command_observation_batch",
                    settled.command_sequence,
                    &request_digest,
                    1,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "response_digest":response_digest,
                        "canonical_batch":{
                            "events":result["events"],
                            "session_observations":result["session_observations"],
                        },
                    }),
                ),
                turn_start_fact(root, settled.command_sequence, &request_digest, 1, turn_id),
                command_fact(
                    root,
                    "hosted_command.settled",
                    settled.command_sequence,
                    &request_digest,
                    1,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "response_digest":response_digest,
                        "succeeded":true,
                    }),
                ),
            ],
        )
        .unwrap();
    state
        .state_store
        .mark_dedicated_command_contacted(root, settled.command_sequence, 1)
        .unwrap();
    state
        .state_store
        .settle_dedicated_command(root, settled.command_sequence, 1, true, &result)
        .unwrap();
    state
        .state_store
        .observe_dedicated_session_state(root, 1, "idle", "turn_running", None, Some(turn_id))
        .unwrap();
    let running_observation = ryeos_app::dedicated_session_service::command_observation(
        &state,
        root,
        settled.command_sequence,
    )
    .unwrap();
    assert_eq!(running_observation["operation"]["id"], turn_id);
    assert_eq!(running_observation["operation"]["state"], "running");
    assert!(running_observation.get("completion_fence").is_none());

    let mut terminal_batch = json!({
        "first_sequence":1,
        "count":1,
        "previous_digest":null,
        "events":[{
            "event_type":"turn.completed",
            "payload":{"turn_id":turn_id,"status":"completed"},
        }],
        "session_observations":[{
            "kind":"state",
            "expected":"turn_running",
            "next":"idle",
            "completed_turn_id":turn_id,
        }],
    });
    let terminal_batch_digest =
        ryeos_state::objects::canonical_value_digest(&terminal_batch).unwrap();
    terminal_batch["batch_digest"] = Value::String(terminal_batch_digest.clone());
    ryeos_app::dedicated_session_service::ingest_observation_batch(&state, root, 1, terminal_batch)
        .unwrap();
    let observation = ryeos_app::dedicated_session_service::command_observation(
        &state,
        root,
        settled.command_sequence,
    )
    .unwrap();
    assert_eq!(observation["chain_root_id"], root);
    assert_eq!(observation["placement_thread_id"], root);
    assert_eq!(observation["admitted_capsule_hash"], capsule_hash);
    assert_eq!(observation["command_state"], "completed");
    assert_eq!(observation["idempotency_key"], "settled-key");
    assert_eq!(observation["route_id"], "test.route");
    assert_eq!(observation["request_digest"], request_digest);
    assert_eq!(observation["response_digest"], response_digest);
    assert_eq!(observation["worker_boot_epoch"], 1);
    assert_eq!(observation["operation"]["id"], turn_id);
    assert_eq!(observation["operation"]["state"], "completed");
    assert_eq!(
        observation["completion_fence"]["command_sequence"],
        settled.command_sequence
    );
    assert_eq!(observation["completion_fence"]["placement_thread_id"], root);
    assert_eq!(
        observation["completion_fence"]["admitted_capsule_hash"],
        capsule_hash
    );
    assert_eq!(observation["completion_fence"]["worker_boot_epoch"], 1);
    assert_eq!(
        observation["completion_fence"]["request_digest"],
        request_digest
    );
    assert_eq!(observation["completion_fence"]["turn_id"], turn_id);
    assert!(
        observation["completion_fence"]["completion_operation_id"]
            .as_str()
            .is_some_and(lillux::valid_hash)
    );

    // A completion that is not admissible from the current state must not
    // publish orphan root testimony. Starting and completing the same turn ID
    // later produces exactly one ordered accepted completion, not a reusable
    // predecessor fact.
    let orphan_turn = "turn-orphan-before-start";
    let mut rejected_completion = json!({
        "first_sequence":2,
        "count":1,
        "previous_digest":terminal_batch_digest,
        "events":[{"event_type":"turn.completed","payload":{"turn_id":orphan_turn}}],
        "session_observations":[{
            "kind":"state",
            "expected":"turn_running",
            "next":"idle",
            "completed_turn_id":orphan_turn,
        }],
    });
    let rejected_digest =
        ryeos_state::objects::canonical_value_digest(&rejected_completion).unwrap();
    rejected_completion["batch_digest"] = Value::String(rejected_digest);
    assert!(
        ryeos_app::dedicated_session_service::ingest_observation_batch(
            &state,
            root,
            1,
            rejected_completion,
        )
        .is_err()
    );
    let before_accepted = state
        .state_store
        .replay_events(root, Some(root), None, 256, 1024 * 1024)
        .unwrap()
        .events;
    assert!(!before_accepted.iter().any(|event| {
        event.event_type == "hosted_session.turn_completed"
            && event.payload["turn_id"] == orphan_turn
    }));

    let mut accepted_start = json!({
        "first_sequence":2,
        "count":1,
        "previous_digest":terminal_batch_digest,
        "events":[{"event_type":"turn.started","payload":{"turn_id":orphan_turn}}],
        "session_observations":[{
            "kind":"state",
            "expected":"idle",
            "next":"turn_running",
            "turn_id":orphan_turn,
        }],
    });
    let accepted_start_digest =
        ryeos_state::objects::canonical_value_digest(&accepted_start).unwrap();
    accepted_start["batch_digest"] = Value::String(accepted_start_digest.clone());
    ryeos_app::dedicated_session_service::ingest_observation_batch(&state, root, 1, accepted_start)
        .unwrap();
    let mut accepted_completion = json!({
        "first_sequence":3,
        "count":1,
        "previous_digest":accepted_start_digest,
        "events":[{"event_type":"turn.completed","payload":{"turn_id":orphan_turn}}],
        "session_observations":[{
            "kind":"state",
            "expected":"turn_running",
            "next":"idle",
            "completed_turn_id":orphan_turn,
        }],
    });
    let accepted_completion_digest =
        ryeos_state::objects::canonical_value_digest(&accepted_completion).unwrap();
    accepted_completion["batch_digest"] = Value::String(accepted_completion_digest);
    ryeos_app::dedicated_session_service::ingest_observation_batch(
        &state,
        root,
        1,
        accepted_completion,
    )
    .unwrap();
    let accepted_events = state
        .state_store
        .replay_events(root, Some(root), None, 256, 1024 * 1024)
        .unwrap()
        .events;
    let accepted_start_seq = accepted_events
        .iter()
        .find(|event| {
            event.event_type == "hosted_session.turn_started"
                && event.payload["turn_id"] == orphan_turn
        })
        .unwrap()
        .chain_seq;
    let accepted_completion = accepted_events
        .iter()
        .filter(|event| {
            event.event_type == "hosted_session.turn_completed"
                && event.payload["turn_id"] == orphan_turn
        })
        .collect::<Vec<_>>();
    assert_eq!(accepted_completion.len(), 1);
    assert!(accepted_start_seq < accepted_completion[0].chain_seq);

    // Model the projection-first crash gap: a settled SQLite row without the
    // matching root fact must never become replay authority.
    let unproved_payload = json!({"route_id":"test.unproved","payload":{}});
    let unproved_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind":"route",
        "payload":unproved_payload,
    }))
    .unwrap();
    let unproved = state
        .state_store
        .reserve_dedicated_session_command(NewDedicatedSessionCommand {
            placement_thread_id: root,
            idempotency_key: "unproved-settled-key",
            worker_boot_epoch: 1,
            command_kind: "route",
            request_digest: &unproved_digest,
            payload: &unproved_payload,
        })
        .unwrap();
    state
        .state_store
        .mark_dedicated_command_contacted(root, unproved.command_sequence, 1)
        .unwrap();
    state
        .state_store
        .settle_dedicated_command(
            root,
            unproved.command_sequence,
            1,
            true,
            &json!({"unproved":true}),
        )
        .unwrap();

    // A failure fact cannot authorize a contradictory completed projection.
    let contradictory_payload = json!({"route_id":"test.contradictory","payload":{}});
    let contradictory_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind":"route",
        "payload":contradictory_payload,
    }))
    .unwrap();
    let contradictory = state
        .state_store
        .reserve_dedicated_session_command(NewDedicatedSessionCommand {
            placement_thread_id: root,
            idempotency_key: "contradictory-state-key",
            worker_boot_epoch: 1,
            command_kind: "route",
            request_digest: &contradictory_digest,
            payload: &contradictory_payload,
        })
        .unwrap();
    state
        .state_store
        .append_events(
            root,
            root,
            &[
                command_fact(
                    root,
                    "hosted_command.committed",
                    contradictory.command_sequence,
                    &contradictory_digest,
                    1,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "command_kind":"route",
                        "route_id":"test.contradictory",
                        "idempotency_key":"contradictory-state-key",
                        "canonical_command":contradictory_payload,
                        "admitted_session_capsule_hash":capsule_hash,
                        "protocol_profile_hash":protocol_profile_hash.clone(),
                        "protocol_schema_hashes":protocol_schema_hashes.clone(),
                    }),
                ),
                command_fact(
                    root,
                    "hosted_command.failed_uncontacted",
                    contradictory.command_sequence,
                    &contradictory_digest,
                    1,
                    json!({
                        "schema":1,
                        "origin":"daemon_verified_process",
                        "retryable_uncontacted":true,
                    }),
                ),
            ],
        )
        .unwrap();
    state
        .state_store
        .mark_dedicated_command_contacted(root, contradictory.command_sequence, 1)
        .unwrap();
    state
        .state_store
        .settle_dedicated_command(
            root,
            contradictory.command_sequence,
            1,
            true,
            &json!({
                "error":"worker epoch ended before contact",
                "retryable_uncontacted":true,
            }),
        )
        .unwrap();

    // A syntactically valid but different protocol-profile digest cannot
    // replace the profile identity sealed by the session capsule.
    let profile_mismatch_payload = json!({"route_id":"test.profile-mismatch","payload":{}});
    let profile_mismatch_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind":"route",
        "payload":profile_mismatch_payload,
    }))
    .unwrap();
    let profile_mismatch = state
        .state_store
        .reserve_dedicated_session_command(NewDedicatedSessionCommand {
            placement_thread_id: root,
            idempotency_key: "profile-mismatch-key",
            worker_boot_epoch: 1,
            command_kind: "route",
            request_digest: &profile_mismatch_digest,
            payload: &profile_mismatch_payload,
        })
        .unwrap();
    state
        .state_store
        .append_events(
            root,
            root,
            &[command_fact(
                root,
                "hosted_command.committed",
                profile_mismatch.command_sequence,
                &profile_mismatch_digest,
                1,
                json!({
                    "schema":1,
                    "origin":"daemon_observed_io",
                    "command_kind":"route",
                    "route_id":"test.profile-mismatch",
                    "idempotency_key":"profile-mismatch-key",
                    "canonical_command":profile_mismatch_payload,
                    "admitted_session_capsule_hash":capsule_hash,
                    "protocol_profile_hash":"0".repeat(64),
                    "protocol_schema_hashes":protocol_schema_hashes.clone(),
                }),
            )],
        )
        .unwrap();
    state
        .state_store
        .mark_dedicated_command_contacted(root, profile_mismatch.command_sequence, 1)
        .unwrap();
    state
        .state_store
        .settle_dedicated_command(
            root,
            profile_mismatch.command_sequence,
            1,
            true,
            &json!({"mismatch":true}),
        )
        .unwrap();
    let error = ryeos_app::dedicated_session_service::command_observation(
        &state,
        root,
        profile_mismatch.command_sequence,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("exact command contract"));

    let unsettled_payload = json!({"route_id":"test.pending","payload":{}});
    let unsettled_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind":"route",
        "payload":unsettled_payload,
    }))
    .unwrap();
    let unsettled = state
        .state_store
        .reserve_dedicated_session_command(NewDedicatedSessionCommand {
            placement_thread_id: root,
            idempotency_key: "unsettled-key",
            worker_boot_epoch: 1,
            command_kind: "route",
            request_digest: &unsettled_digest,
            payload: &unsettled_payload,
        })
        .unwrap();
    state
        .state_store
        .append_events(
            root,
            root,
            &[command_fact(
                root,
                "hosted_command.committed",
                unsettled.command_sequence,
                &unsettled_digest,
                1,
                json!({
                    "schema":1,
                    "origin":"daemon_observed_io",
                    "command_kind":"route",
                    "route_id":"test.pending",
                    "idempotency_key":"unsettled-key",
                    "canonical_command":unsettled_payload,
                    "admitted_session_capsule_hash":capsule_hash,
                    "protocol_profile_hash":protocol_profile_hash,
                    "protocol_schema_hashes":protocol_schema_hashes,
                }),
            )],
        )
        .unwrap();
    let error = ryeos_app::dedicated_session_service::command_observation(
        &state,
        root,
        unsettled.command_sequence,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("command is not authoritatively settled"));
    state
        .state_store
        .finalize_thread(
            root,
            &FinalizeThreadRecord {
                status: "completed".to_owned(),
                outcome_code: None,
                result_json: Some(json!({"ok":true})),
                error_json: None,
                artifacts: vec![],
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();

    let terminal_observation = ryeos_app::dedicated_session_service::command_observation(
        &state,
        root,
        settled.command_sequence,
    )
    .unwrap();
    assert_eq!(terminal_observation["operation"]["state"], "completed");

    let before_events = state
        .state_store
        .replay_events(root, Some(root), None, 128, 1024 * 1024)
        .unwrap()
        .events
        .len();
    let before_processes = state.state_store.live_worker_processes().unwrap();
    let before_outbox = state
        .state_store
        .dedicated_command_outbox_records()
        .unwrap();
    let replay = ryeos_app::dedicated_session_service::execute_command(
        &state,
        root,
        "settled-key",
        "route",
        command_payload.clone(),
    )
    .await
    .unwrap();
    assert_eq!(replay["state"], "completed");
    assert_eq!(replay["result"], result);
    assert_eq!(
        state
            .state_store
            .replay_events(root, Some(root), None, 128, 1024 * 1024)
            .unwrap()
            .events
            .len(),
        before_events
    );
    assert_eq!(
        state.state_store.live_worker_processes().unwrap(),
        before_processes
    );
    assert_eq!(
        state
            .state_store
            .dedicated_command_outbox_records()
            .unwrap(),
        before_outbox
    );

    let changed = ryeos_app::dedicated_session_service::execute_command(
        &state,
        root,
        "settled-key",
        "route",
        json!({"route_id":"test.route","payload":{"value":2}}),
    )
    .await
    .unwrap_err();
    assert!(changed.to_string().contains("different authority"));
    let absent = ryeos_app::dedicated_session_service::execute_command(
        &state,
        root,
        "absent-key",
        "route",
        json!({"route_id":"test.absent","payload":{}}),
    )
    .await
    .unwrap_err();
    assert!(
        absent.to_string().contains("not durably appendable"),
        "{absent:#}"
    );
    let unproved = ryeos_app::dedicated_session_service::execute_command(
        &state,
        root,
        "unproved-settled-key",
        "route",
        unproved_payload,
    )
    .await
    .unwrap_err();
    assert!(
        unproved.to_string().contains("not durably appendable"),
        "{unproved:#}"
    );
    let contradictory = ryeos_app::dedicated_session_service::execute_command(
        &state,
        root,
        "contradictory-state-key",
        "route",
        contradictory_payload,
    )
    .await
    .unwrap_err();
    assert!(
        contradictory
            .to_string()
            .contains("projection is not failed"),
        "{contradictory:#}"
    );
    let unsettled = ryeos_app::dedicated_session_service::execute_command(
        &state,
        root,
        "unsettled-key",
        "route",
        unsettled_payload,
    )
    .await
    .unwrap_err();
    assert!(
        unsettled.to_string().contains("not durably appendable"),
        "{unsettled:#}"
    );
}
