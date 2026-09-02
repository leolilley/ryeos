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

#[tokio::test]
async fn terminal_root_replays_only_exact_authoritatively_settled_command() {
    let (_tmp, state) = test_state::build_test_state();
    let root = "T-terminal-hosted-replay";
    let owner = "fp:test-operator";
    let capsule_hash = "b".repeat(64);
    let launch_claim = state
        .state_store
        .reserve_fresh_thread_launch_active(root, "claim-terminal-hosted-replay", "daemon-test")
        .unwrap()
        .unwrap();
    state
        .state_store
        .create_thread_for_test(&root_thread(root, owner))
        .unwrap();
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
    let result = json!({"events":[],"session_observations":[],"value":"retained"});
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
                        "protocol_profile_hash":"d".repeat(64),
                        "protocol_schema_hashes":{"request":"e".repeat(64)},
                    }),
                ),
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
                        "protocol_profile_hash":"d".repeat(64),
                        "protocol_schema_hashes":{"request":"e".repeat(64)},
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

    let unsettled_payload = json!({"route_id":"test.pending","payload":{}});
    let unsettled_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind":"route",
        "payload":unsettled_payload,
    }))
    .unwrap();
    state
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
