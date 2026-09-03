mod test_state;

use ryeos_app::process::{ExecutionProcessIdentity, PROCESS_IDENTITY_SCHEMA_VERSION};
use ryeos_app::runtime_db::{
    NewCredentialProfile, NewDedicatedSession, WorkerProcessRecord, WorkerProcessState,
    WorkspaceBinding, WorkspaceState,
};
use ryeos_app::state_store::{
    FinalizeThreadRecord, NewDedicatedSessionCommand, NewEventRecord, NewThreadRecord,
};
use serde_json::{Value, json};

fn store_structured_session_capsule(state: &ryeos_app::state::AppState) -> (String, String, Value) {
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
            wire_version: 1,
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
        structured_session_profile: Some(AdmittedStructuredSessionProfile {
            profile_hash: profile_hash.clone(),
            contract,
            schema_hashes: schema_hashes.clone(),
            baseline_source: "baseline.toml".to_owned(),
            baseline_destination: "config.toml".to_owned(),
        }),
        executable_search: Vec::new(),
        runtime_ref: "runtime:fixture/session".to_owned(),
        executor_ref: "native:fixture".to_owned(),
    };
    let value = capsule.to_value().unwrap();
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

fn seed_completed_turn_fixture(
    state: &ryeos_app::state::AppState,
    root: &str,
) -> CompletedTurnFixture {
    let owner = "fp:test-operator";
    let (capsule_hash, protocol_profile_hash, protocol_schema_hashes) =
        store_structured_session_capsule(state);
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
                boot_id: format!("test-boot-{root}"),
                target_pid: 101,
                target_start_time_ticks: 10,
                group_leader_pid: 101,
                group_leader_start_time_ticks: 10,
            },
            control_channel_identity: format!("fd:{root}"),
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
        .complete_worker_binding(&worker_instance_id, root, 1)
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
                ),
                command_fact(
                    root,
                    "hosted_worker_command_observation_batch",
                    command.command_sequence,
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
                turn_start_fact(root, command.command_sequence, &request_digest, 1, &turn_id),
                command_fact(
                    root,
                    "hosted_command.settled",
                    command.command_sequence,
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
        .mark_dedicated_command_contacted(root, command.command_sequence, 1)
        .unwrap();
    state
        .state_store
        .settle_dedicated_command(root, command.command_sequence, 1, true, &result)
        .unwrap();
    state
        .state_store
        .observe_dedicated_session_state(root, 1, "idle", "turn_running", None, Some(&turn_id))
        .unwrap();
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
    let observation = ryeos_app::dedicated_session_service::command_observation(
        state,
        root,
        command.command_sequence,
    )
    .unwrap();
    CompletedTurnFixture {
        fence: serde_json::from_value(observation["completion_fence"].clone()).unwrap(),
        worker_instance_id,
        command_payload,
        request_digest,
    }
}

#[tokio::test]
async fn completed_termination_requires_the_exact_immutable_turn_fence_and_frontier() {
    let (_tmp, state) = test_state::build_test_state();
    let root = "T-completed-fence";
    let fixture = seed_completed_turn_fixture(&state, root);

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
            .prepare_dedicated_session_recovery(recovered_root, 1, &recovered_worker)
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
    let reattach_payload = json!({"upstream_session_id":"upstream-recovered"});
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
