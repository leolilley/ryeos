//! Workspace binding, worker handoff and credential-authority tests.
use super::*;

#[test]
fn workspace_binding_reaped_parent_cannot_hide_unsettled_descendant_contact() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-reaped-parent";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    let identity = attach_workspace_test_process(&store, &binding);
    let nested = workspace_child_fixture(&store, child, "T-live-descendant", &root_binding);
    store
        .bind_thread_workspace("T-live-descendant", &nested)
        .unwrap();
    assert!(
        !store
            .settle_reaped_thread_workspace_owned(child, &binding, &identity)
            .unwrap()
    );
    assert_eq!(
        store
            .get_thread(child)
            .unwrap()
            .unwrap()
            .runtime
            .process_identity,
        Some(identity.clone())
    );
    finish_workspace_test_thread(&store, "T-live-descendant");
    assert!(
        store
            .settle_thread_workspace_owned("T-live-descendant", &nested)
            .unwrap()
    );
    assert!(
        store
            .settle_reaped_thread_workspace_owned(child, &binding, &identity)
            .unwrap()
    );
}

#[test]
fn workspace_binding_replays_exactly_and_refuses_conflicts_and_stale_owners() {
    let (store, root_binding) = workspace_binding_fixture();
    // Contact can use the existing Active root view; it does not repeat
    // the Ready-only root birth mutation.
    store
        .authorize_thread_workspace_contact("T-workspace-root", &root_binding)
        .unwrap();
    let mut wrong_root = root_binding.clone();
    wrong_root.view_identity = "other-created-view".to_owned();
    assert!(
        store
            .authorize_thread_workspace_contact("T-workspace-root", &wrong_root)
            .is_err()
    );
    let child = "T-binding-child";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    store.bind_thread_workspace(child, &binding).unwrap();
    let mut conflicting = binding.clone();
    conflicting.view_identity = "other-created-view".to_owned();
    assert!(store.bind_thread_workspace(child, &conflicting).is_err());
    let claim = store.get_launch_claim(child).unwrap().unwrap();
    store
        .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
        .unwrap();
    store
        .claim_thread_launch_active(child, "replacement-claim", "daemon:replacement")
        .unwrap()
        .unwrap();
    assert!(store.bind_thread_workspace(child, &binding).is_err());
    assert!(
        store
            .authorize_thread_workspace_contact(child, &binding)
            .is_err()
    );
    assert_eq!(
        store.thread_workspace_binding(child).unwrap(),
        Some(binding)
    );
}

#[test]
fn workspace_binding_freeze_fences_admission_attachment_and_late_release() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-freeze-child";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    let identity = crate::process::ExecutionProcessIdentity {
        schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
        process_scope: None,
        boot_id: "test-boot".to_owned(),
        target_pid: 12345,
        target_start_time_ticks: 10,
        group_leader_pid: 12345,
        group_leader_start_time_ticks: 10,
        resource_selections: Vec::new(),
        resource_settlement_authority: None,
        resource_operations: Vec::new(),
        resource_allocation_limit: None,
        resource_occupancy_start: None,
        resource_occupancy_limit: None,
        resource_cleanup_allowance_ms: None,
    };
    let owner = store.get_launch_claim(child).unwrap().unwrap().claimed_by;
    store
        .attach_new_thread_process(
            child,
            identity.target_pid,
            identity.group_leader_pid,
            &identity,
            &crate::launch_metadata::RuntimeLaunchMetadata::default(),
            Some(&owner),
        )
        .unwrap();
    store
        .authorize_attached_process_release(child, &identity, Some(&owner))
        .unwrap();
    store
        .transition_execution_workspace(
            &binding.workspace_id,
            &[runtime_db::WorkspaceState::Active],
            runtime_db::WorkspaceState::Freezing,
            None,
        )
        .unwrap();
    assert!(store.bind_thread_workspace(child, &binding).is_err());
    assert!(
        store
            .authorize_thread_workspace_contact(child, &binding)
            .is_err()
    );
    assert!(
        store
            .authorize_attached_process_release(child, &identity, Some(&owner))
            .is_err()
    );
    assert!(
        store
            .attach_thread_process(
                child,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                Some(&owner)
            )
            .is_err()
    );
    assert_eq!(
        store.thread_workspace_binding(child).unwrap(),
        Some(binding)
    );
}

#[test]
fn dedicated_workspace_contact_requires_exact_active_root_binding() {
    let (store, binding) = workspace_binding_fixture();
    {
        let g = store.lock().unwrap();
        store
            .authorize_dedicated_workspace_contact_locked(
                &g,
                "T-workspace-root",
                &binding.workspace_id,
            )
            .unwrap();
        assert!(
            store
                .authorize_dedicated_workspace_contact_locked(
                    &g,
                    "T-workspace-root",
                    "obsolete-workspace"
                )
                .is_err()
        );
    }
    let child = "T-not-dedicated-root";
    let child_binding = workspace_child_fixture(&store, "T-workspace-root", child, &binding);
    store.bind_thread_workspace(child, &child_binding).unwrap();
    assert!(
        store
            .authorize_dedicated_workspace_contact_locked(
                &store.lock().unwrap(),
                child,
                &binding.workspace_id
            )
            .is_err()
    );
    let claim = store.get_launch_claim("T-workspace-root").unwrap().unwrap();
    store
        .release_active_thread_launch_claim("T-workspace-root", &claim.claim_id, &claim.claimed_by)
        .unwrap();
    assert!(
        store
            .authorize_dedicated_workspace_contact_locked(
                &store.lock().unwrap(),
                "T-workspace-root",
                &binding.workspace_id
            )
            .is_err()
    );
}

#[test]
fn workspace_capture_refuses_pending_and_identity_unknown_worker_start() {
    for (cleanup_proved, recovered_boot, contacted_command) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, false),
        (true, true, true),
    ] {
        let (store, binding) = workspace_binding_fixture();
        let root = "T-workspace-root";
        let workspace = store
            .execution_workspace(&binding.workspace_id)
            .unwrap()
            .unwrap();
        assert!(
            store
                .workspace_worker_capture_record(&workspace)
                .unwrap()
                .is_none()
        );
        store
            .create_credential_profile(NewCredentialProfile {
                profile_id: "P-capture",
                owner_principal: "fp:operator",
                home_id: "home-capture",
            })
            .unwrap();
        store
            .acquire_credential_profile("P-capture", "fp:operator", "worker-capture")
            .unwrap();
        store
            .admit_dedicated_session(NewDedicatedSession {
                placement_thread_id: root,
                chain_root_id: root,
                owner_principal: "fp:operator",
                admitted_capsule_hash: &"a".repeat(64),
                workspace_id: &binding.workspace_id,
                candidate_required: false,
                candidate_disposition: runtime_db::DedicatedCandidateDisposition::OwnerDecision,
                credential_profile_id: "P-capture",
                credential_generation: 1,
                credential_lock_owner: "worker-capture",
            })
            .unwrap();
        assert!(store.workspace_worker_capture_record(&workspace).is_err());
        let (worker_id, boot_epoch) = if recovered_boot {
            // An exact old retirement permits recovery before, but never
            // after, a new admitted attempt crosses the durable boundary.
            let identity = attach_workspace_test_process(&store, &binding);
            store
                .transition_execution_workspace_owned(
                    &binding.workspace_id,
                    root,
                    &store.get_launch_claim(root).unwrap().unwrap().claimed_by,
                    &[runtime_db::WorkspaceState::Active],
                    runtime_db::WorkspaceState::Active,
                    Some(&serde_json::to_string(&identity).unwrap()),
                )
                .unwrap();
            store
                .attach_worker_process(&WorkerProcessRecord {
                    worker_instance_id: "worker-capture".to_owned(),
                    boot_identity_hash: "b".repeat(64),
                    session_capsule_hash: "a".repeat(64),
                    boot_epoch: 1,
                    lifecycle_generation: 1,
                    process_identity: identity,
                    control_channel_identity: "fixture-channel".to_owned(),
                    state: runtime_db::WorkerProcessState::Attached,
                    daemon_generation_id: "daemon:workspace-test".to_owned(),
                    placement_thread_id: root.to_owned(),
                    cleanup_state: "owned".to_owned(),
                    created_at_ms: 1,
                    updated_at_ms: 1,
                })
                .unwrap();
            let contacted = if contacted_command {
                store
                    .complete_worker_binding("worker-capture", root, 1)
                    .unwrap();
                let command = store
                    .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                        placement_thread_id: root,
                        idempotency_key: "capture-contacted-command",
                        worker_boot_epoch: 1,
                        command_kind: "route",
                        request_digest: &"c".repeat(64),
                        payload: &serde_json::json!({"route": "fixture.read"}),
                    })
                    .unwrap();
                store
                    .mark_dedicated_command_contacted(root, command.command_sequence, 1)
                    .unwrap();
                Some(command.command_sequence)
            } else {
                None
            };
            store
                .fence_abandoned_worker_process("worker-capture", root, 1, "reaped")
                .unwrap();
            let retired = store
                .execution_workspace(&binding.workspace_id)
                .unwrap()
                .unwrap();
            assert!(
                store
                    .workspace_worker_capture_record(&retired)
                    .unwrap()
                    .is_none()
            );
            if let Some(sequence) = contacted {
                let session = store.dedicated_session(root).unwrap().unwrap();
                assert_eq!(session.state, "outcome_unknown");
                assert_eq!(session.send_boundary, "outcome_unknown");
                assert!(session.worker_instance_id.is_none());
                assert!(session.worker_boot_epoch.is_none());
                let command = store
                    .dedicated_session_command(root, sequence)
                    .unwrap()
                    .unwrap();
                assert_eq!(command.state, "outcome_unknown");
                assert_eq!(command.worker_boot_epoch, 1);
                assert!(command.result.is_none());
                assert!(
                    store
                        .mark_dedicated_command_contacted(root, sequence, 1)
                        .is_err()
                );
            }
            store
                .acquire_credential_profile("P-capture", "fp:operator", "worker-next")
                .unwrap();
            if contacted.is_some() {
                // Proven process retirement permits capture, not automatic
                // recovery of an unknown command. The uncontacted cases
                // below separately prove that a valid newer pending boot
                // cannot hide behind this historical cleanup testimony.
                assert!(
                    store
                        .prepare_dedicated_session_recovery(
                            root,
                            1,
                            "worker-next",
                            &binding.workspace_id
                        )
                        .is_err()
                );
                let session = store.dedicated_session(root).unwrap().unwrap();
                assert_eq!(session.state, "outcome_unknown");
                assert_eq!(session.send_boundary, "outcome_unknown");
                assert!(session.worker_instance_id.is_none());
                assert!(session.worker_boot_epoch.is_none());
                assert!(
                    store
                        .workspace_worker_capture_record(&retired)
                        .unwrap()
                        .is_none()
                );
                continue;
            }
            assert_eq!(
                store
                    .prepare_dedicated_session_recovery(
                        root,
                        1,
                        "worker-next",
                        &binding.workspace_id
                    )
                    .unwrap(),
                2
            );
            assert!(store.workspace_worker_capture_record(&retired).is_err());
            assert!(
                store
                    .fail_dedicated_session_start(root, "worker-capture", 1, "stale cleanup", true)
                    .is_err()
            );
            assert!(
                store
                    .fail_dedicated_session_start(root, "worker-next", 1, "wrong epoch", true)
                    .is_err()
            );
            assert_eq!(
                store
                    .reconcile_unattached_credential_profile_locks()
                    .unwrap(),
                (1, 0)
            );
            assert!(store.workspace_worker_capture_record(&retired).is_err());
            assert_eq!(
                store
                    .credential_profile("P-capture")
                    .unwrap()
                    .unwrap()
                    .lock_owner
                    .as_deref(),
                Some("worker-next")
            );
            ("worker-next", 2)
        } else {
            ("worker-capture", 1)
        };
        store
            .fail_dedicated_session_start(
                root,
                worker_id,
                boot_epoch,
                "fixture start failure",
                cleanup_proved,
            )
            .unwrap();
        let current = store
            .execution_workspace(&binding.workspace_id)
            .unwrap()
            .unwrap();
        let observed = store.workspace_worker_capture_record(&current);
        if cleanup_proved {
            assert!(observed.unwrap().is_none());
        } else {
            assert!(
                observed.is_err(),
                "missing identity must not hide outcome-unknown contact"
            );
        }
    }
}

#[test]
fn workspace_binding_unknown_contact_survives_terminalization_and_claim_removal() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-unknown-child";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    assert!(
        store
            .get_thread(child)
            .unwrap()
            .unwrap()
            .runtime
            .process_identity
            .is_none()
    );
    assert!(
        !store
            .settle_thread_workspace_owned(child, &binding)
            .unwrap()
    );
    finish_workspace_test_thread(&store, child);
    assert_eq!(
        store.thread_workspace_binding(child).unwrap(),
        Some(binding.clone())
    );
    let claim = store.get_launch_claim(child).unwrap().unwrap();
    store
        .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
        .unwrap();
    assert!(
        store
            .settle_thread_workspace_owned(child, &binding)
            .is_err()
    );
    let members = store
        .workspace_members_after(&binding.workspace_id, &binding.view_identity, None, 10)
        .unwrap();
    assert!(
        members
            .iter()
            .any(|member| member.thread_id == child && member.binding == binding)
    );
}

#[test]
fn workspace_binding_live_owner_must_explicitly_settle_after_lifecycle_completion() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-settled-child";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    finish_workspace_test_thread(&store, child);
    assert!(store.thread_workspace_binding(child).unwrap().is_some());
    assert!(
        store
            .settle_thread_workspace_owned(child, &binding)
            .unwrap()
    );
    assert!(
        store
            .settle_thread_workspace_owned(child, &binding)
            .unwrap()
    );
    assert!(store.thread_workspace_binding(child).unwrap().is_none());
}

#[test]
fn workspace_binding_immutable_input_is_not_membership_or_mutable_descendant_authority() {
    let (store, root_binding) = workspace_binding_fixture();
    let input = "T-independent-input";
    workspace_child_fixture(&store, "T-workspace-root", input, &root_binding);
    // The launch owner selects a separate immutable input and never binds
    // the subject's mutable view. Its lineage alone cannot add membership.
    assert!(store.thread_workspace_binding(input).unwrap().is_none());
    StateStore::authorize_thread_workspace_contact_locked(&store.lock().unwrap(), input).unwrap();
    let nested = workspace_child_fixture(&store, input, "T-input-nested", &root_binding);
    assert!(
        store
            .bind_thread_workspace("T-input-nested", &nested)
            .is_err()
    );
    assert_eq!(
        store
            .workspace_members_after(
                &root_binding.workspace_id,
                &root_binding.view_identity,
                None,
                10
            )
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn workspace_binding_immutable_capture_blocks_even_previously_bound_writers() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-capture-child";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    reserve_workspace_test_barrier(
        &store,
        ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
        "T-operation-child",
    );
    assert!(store.bind_thread_workspace(child, &binding).is_err());
    assert!(
        StateStore::authorize_thread_workspace_contact_locked(&store.lock().unwrap(), child)
            .is_err()
    );
}

#[test]
fn workspace_binding_exclusive_operation_admits_only_its_same_view_subtree() {
    use runtime_db::RuntimeWorkspaceOperationPhase as Phase;
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-exclusive-child";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    let op = reserve_workspace_test_barrier(
        &store,
        ryeos_engine::kind_registry::WorkspaceAccess::SharedExclusive,
        child,
    );
    assert!(store.bind_thread_workspace(child, &binding).is_err());
    store
        .transition_runtime_workspace_operation(&op, &[Phase::Reserved], Phase::Quiescing)
        .unwrap();
    store
        .transition_runtime_workspace_operation(&op, &[Phase::Quiescing], Phase::Quiesced)
        .unwrap();
    store.bind_thread_workspace(child, &binding).unwrap();
    let nested = workspace_child_fixture(&store, child, "T-exclusive-nested", &root_binding);
    store
        .bind_thread_workspace("T-exclusive-nested", &nested)
        .unwrap();
    let sibling = workspace_child_fixture(
        &store,
        "T-workspace-root",
        "T-exclusive-sibling",
        &root_binding,
    );
    assert!(
        store
            .bind_thread_workspace("T-exclusive-sibling", &sibling)
            .is_err()
    );
    store
        .transition_runtime_workspace_operation(&op, &[Phase::Quiesced], Phase::ChildRunning)
        .unwrap();
    finish_workspace_test_thread(&store, child);
    assert!(
        !store
            .settle_thread_workspace_owned(child, &binding)
            .unwrap()
    );
    let intent = store.get_runtime_action_intent(&op).unwrap().unwrap();
    assert!(
        !StateStore::runtime_workspace_subtree_is_settled_locked(&store.lock().unwrap(), &intent)
            .unwrap()
    );
    finish_workspace_test_thread(&store, "T-exclusive-nested");
    assert!(
        store
            .settle_thread_workspace_owned("T-exclusive-nested", &nested)
            .unwrap()
    );
    assert!(
        store
            .settle_thread_workspace_owned(child, &binding)
            .unwrap()
    );
    assert!(
        StateStore::runtime_workspace_subtree_is_settled_locked(&store.lock().unwrap(), &intent)
            .unwrap()
    );
    store
        .transition_runtime_workspace_operation(&op, &[Phase::ChildRunning], Phase::Settling)
        .unwrap();
    assert!(
        StateStore::authorize_thread_workspace_contact_locked(
            &store.lock().unwrap(),
            "T-workspace-root"
        )
        .is_err()
    );
}

#[test]
fn workspace_binding_member_pages_are_bounded_and_exact() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-page-child";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    let first = store
        .workspace_members_after(&binding.workspace_id, &binding.view_identity, None, 1)
        .unwrap();
    let next = store
        .workspace_members_after(
            &binding.workspace_id,
            &binding.view_identity,
            Some(&first[0].thread_id),
            1,
        )
        .unwrap();
    assert_ne!(first[0].thread_id, next[0].thread_id);
    assert!(
        store
            .workspace_members_after(&binding.workspace_id, "another-view", None, 1)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .workspace_members_after(&binding.workspace_id, &binding.view_identity, None, 0)
            .is_err()
    );
    assert!(
        store
            .workspace_members_after(
                &binding.workspace_id,
                &binding.view_identity,
                None,
                runtime_db::WORKSPACE_MEMBER_PAGE_SIZE + 1
            )
            .is_err()
    );
}

#[test]
fn handoff_reservation_fences_the_exact_project_head_until_adoption() {
    let tmp = tempdir().expect("tempdir");
    let identity =
        ryeos_node_runtime::identity::NodeIdentity::create(&tmp.path().join("node-key.pem"))
            .expect("test node identity");
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let store = TestStateStore::open(tmp.path(), signer.clone(), Arc::new(head_trust))
        .expect("state store");
    let authority = store.pinned_state_authority().expect("state authority");
    let guard = authority.acquire_shared_guard().expect("CAS guard");
    let principal_key = "1".repeat(64);
    let project_hash = "2".repeat(64);
    let initial_head = "3".repeat(64);
    let next_head = "4".repeat(64);
    store
        .write_project_head_ref(
            &principal_key,
            &project_hash,
            &initial_head,
            signer.as_ref(),
            &guard,
        )
        .expect("initial project HEAD");

    store
        .create_credential_profile(NewCredentialProfile {
            profile_id: "P-project-fence",
            owner_principal: "fp:operator",
            home_id: "home-project-fence",
        })
        .expect("credential profile");
    store
        .acquire_credential_profile(
            "P-project-fence",
            "fp:operator",
            "credential-enrollment:project-fence",
        )
        .expect("credential lock");
    let login_epoch = store
        .begin_credential_enrollment(
            "P-project-fence",
            "credential-enrollment:project-fence",
            "credential-login:project-fence",
            lillux::time::timestamp_millis() + 60_000,
        )
        .expect("begin credential enrollment");
    let credential_generation = store
        .complete_credential_enrollment(
            "P-project-fence",
            "credential-enrollment:project-fence",
            "credential-login:project-fence",
            login_epoch,
            &json!({"account":"project-fence"}),
        )
        .expect("complete credential enrollment");
    store
        .release_credential_profile("P-project-fence", "credential-enrollment:project-fence")
        .expect("release enrollment lock");
    store
        .reserve_credential_profile_generation(NewCredentialProfileReservation {
            reservation_id: "reservation-project-fence",
            operation_id: "operation-project-fence",
            successor_thread_id: "T-project-fence-target",
            profile_id: "P-project-fence",
            owner_principal: "fp:operator",
            credential_generation,
            subject_contract_digest: &"5".repeat(64),
            subject_digest: &"6".repeat(64),
            checkpoint_manifest_hash: &"7".repeat(64),
            upstream_session_id: "upstream-project-fence",
            project_principal_key: &principal_key,
            project_hash: &project_hash,
            target_project_head_hash: &initial_head,
        })
        .expect("handoff reservation");

    let error = store
        .advance_project_head_ref(
            &principal_key,
            &project_hash,
            &next_head,
            &initial_head,
            signer.as_ref(),
            &guard,
        )
        .expect_err("active handoff fence must reject project mutation");
    assert!(format!("{error:#}").contains("fenced by worker handoff reservation"));
    assert_eq!(
        store
            .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))
            .unwrap()
            .as_deref(),
        Some(initial_head.as_str())
    );

    let compact_called = std::sync::atomic::AtomicBool::new(false);
    let error = store
        .with_project_head_compaction_authority(|| {
            compact_called.store(true, Ordering::SeqCst);
            Ok(())
        })
        .expect_err("online project compaction must share the handoff fence");
    assert!(format!("{error:#}").contains("active worker handoff"));
    assert!(!compact_called.load(Ordering::SeqCst));

    store
        .release_project_head_fence("reservation-project-fence")
        .expect("authoritative adoption releases project fence");
    store
        .with_project_head_compaction_authority(|| {
            compact_called.store(true, Ordering::SeqCst);
            Ok(())
        })
        .expect("released fence permits project compaction");
    assert!(compact_called.load(Ordering::SeqCst));
    store
        .advance_project_head_ref(
            &principal_key,
            &project_hash,
            &next_head,
            &initial_head,
            signer.as_ref(),
            &guard,
        )
        .expect("released fence permits exact project CAS");
}

#[test]
fn target_terminal_outcomes_share_one_irreversible_branch_head() {
    let adoption_store = test_store();
    let adoption_operation = handoff_test_operation(
        crate::worker_handoff::WorkerHandoffJobRole::Target,
        "adoption-wins",
        "T-source-adoption-wins",
    );
    let adoption_job_id = seed_projected_target_adoption_job(&adoption_store, &adoption_operation);
    let (request, response) = target_adoption_coordinates(&adoption_operation);
    adoption_store
        .publish_worker_handoff_adoption_receipt(
            &adoption_job_id,
            &adoption_operation,
            &request,
            &response,
        )
        .unwrap();
    assert!(
        adoption_store
            .publish_worker_handoff_abort_fence(&adoption_operation, &"e".repeat(64))
            .unwrap_err()
            .to_string()
            .contains("competing target branch")
    );
    assert_eq!(
        adoption_store
            .worker_handoff_adoption_receipt(&adoption_operation.operation_id)
            .unwrap()
            .unwrap()
            .response,
        response
    );

    // One Store is live per process: each branch closes its Store before
    // the next opens.
    adoption_store.close().unwrap();
    let abort_store = test_store();
    let abort_operation = handoff_test_operation(
        crate::worker_handoff::WorkerHandoffJobRole::Target,
        "abort-wins",
        "T-source-abort-wins",
    );
    let abort_job_id = seed_projected_target_adoption_job(&abort_store, &abort_operation);
    let (request, response) = target_adoption_coordinates(&abort_operation);
    abort_store
        .publish_worker_handoff_abort_fence(&abort_operation, &"e".repeat(64))
        .unwrap();
    assert!(
        abort_store
            .publish_worker_handoff_adoption_receipt(
                &abort_job_id,
                &abort_operation,
                &request,
                &response,
            )
            .is_err()
    );
    assert!(
        abort_store
            .worker_handoff_abort_fence(&abort_operation.operation_id)
            .unwrap()
            .is_some()
    );

    abort_store.close().unwrap();
    let completion_store = test_store();
    let completion_operation = handoff_test_operation(
        crate::worker_handoff::WorkerHandoffJobRole::Target,
        "completion-wins",
        "T-source-completion-wins",
    );
    let completion_job_id =
        seed_projected_target_adoption_job(&completion_store, &completion_operation);
    let (completion_request, adoption_response) =
        target_adoption_coordinates(&completion_operation);
    let completion = crate::worker_handoff::WorkerPlacementCompletionResponse {
        operation_id: completion_operation.operation_id.clone(),
        chain_root_id: completion_operation.chain_root_id.clone(),
        placement_thread_id: completion_operation.successor_placement_thread_id.clone(),
        target_chain_head_hash: "f".repeat(64),
        terminal_status: "completed".to_owned(),
        credential_disposition: "completed_session_released".to_owned(),
    };
    let completion_hash = completion_store
        .publish_worker_handoff_terminal_completion(
            &completion_operation,
            &completion_request,
            &completion,
        )
        .unwrap();
    assert_eq!(
        completion_store
            .publish_worker_handoff_terminal_completion(
                &completion_operation,
                &completion_request,
                &completion,
            )
            .unwrap(),
        completion_hash
    );
    assert_eq!(
        completion_store
            .worker_handoff_terminal_completion(&completion_operation.operation_id)
            .unwrap()
            .unwrap()
            .completion,
        completion
    );
    assert!(
        completion_store
            .publish_worker_handoff_adoption_receipt(
                &completion_job_id,
                &completion_operation,
                &completion_request,
                &adoption_response,
            )
            .is_err()
    );
    assert!(
        completion_store
            .publish_worker_handoff_abort_fence(&completion_operation, &"e".repeat(64))
            .is_err()
    );
    assert!(
        completion_store
            .publish_worker_handoff_terminal_failure(
                &completion_operation,
                &completion_request,
                &crate::worker_handoff::WorkerPlacementFailureResponse {
                    operation_id: completion_operation.operation_id.clone(),
                    chain_root_id: completion_operation.chain_root_id.clone(),
                    placement_thread_id: completion_operation.successor_placement_thread_id.clone(),
                    target_chain_head_hash: "d".repeat(64),
                    terminal_status: "failed".to_owned(),
                    credential_disposition: "consumed_session_terminal".to_owned(),
                },
            )
            .is_err()
    );
}

#[test]
fn rejected_verified_sync_payload_cannot_poison_a_same_process_retry() {
    let store = test_store();
    let job_id = "job:verified-sync-retry";
    store
        .with_state_db(|db| {
            db.create_sync_job(&ryeos_state::NewSyncJob {
                job_id: job_id.to_owned(),
                operation_type: "verified_sync_retry".to_owned(),
                operation: serde_json::json!({
                    "schema": 1,
                    "operation_type": "verified_sync_retry",
                }),
                peer: Some("remote".to_owned()),
                roots: Vec::new(),
                heads: Vec::new(),
                max_attempts: 1,
            })
        })
        .unwrap();
    let value = serde_json::json!({
        "schema": "ryeos.test.verified_sync_retry.v1",
        "coordinate": "exact",
    });
    let bytes = lillux::canonical_json(&value).unwrap().into_bytes();
    let hash = lillux::sha256_hex(&bytes);
    let payload = ryeos_state::sync::ExportPayload {
        chain_root_id: "T-verified-sync-retry".to_owned(),
        chain_head_hash: hash.clone(),
        entries: vec![ryeos_state::sync::SyncEntry {
            hash: hash.clone(),
            is_blob: false,
            data: bytes,
        }],
        total_bytes: lillux::canonical_json(&value).unwrap().len(),
    };
    let before = store
        .with_state_db(|db| db.get_sync_job(job_id))
        .unwrap()
        .unwrap();

    let rejected = store.stage_verified_sync_payload_for_existing_job(
        &payload,
        &ryeos_state::sync::ImportAttribution::default(),
        job_id,
        "remote_proof",
        std::slice::from_ref(&hash),
        Some(serde_json::json!({"proof":"rejected"})),
        |_| anyhow::bail!("synthetic semantic proof failure"),
    );
    assert!(
        rejected
            .unwrap_err()
            .to_string()
            .contains("synthetic semantic proof failure")
    );
    assert_eq!(
        store
            .with_state_db(|db| db.get_sync_job(job_id))
            .unwrap()
            .unwrap(),
        before
    );

    store
        .stage_verified_sync_payload_for_existing_job(
            &payload,
            &ryeos_state::sync::ImportAttribution::default(),
            job_id,
            "remote_proof",
            std::slice::from_ref(&hash),
            Some(serde_json::json!({"proof":"accepted"})),
            |cas| {
                anyhow::ensure!(cas.get_object(&hash)?.as_ref() == Some(&value));
                Ok(())
            },
        )
        .unwrap();
    let accepted = store
        .with_state_db(|db| db.get_sync_job(job_id))
        .unwrap()
        .unwrap();
    assert_eq!(accepted.roots, vec![hash]);
    assert_eq!(
        accepted.result,
        Some(serde_json::json!({"proof":"accepted"}))
    );
}

#[test]
fn concurrent_target_branch_publishers_cannot_overwrite_each_other() {
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let operation = handoff_test_operation(
        crate::worker_handoff::WorkerHandoffJobRole::Target,
        "concurrent-branch",
        "T-source-concurrent-branch",
    );
    let job_id = seed_projected_target_adoption_job(&store, &operation);
    let (request, response) = target_adoption_coordinates(&operation);
    let ready = Arc::new(std::sync::Barrier::new(3));
    let (adoption, abort) = std::thread::scope(|scope| {
        let adoption_store = Arc::clone(&store);
        let adoption_ready = Arc::clone(&ready);
        let adoption_operation = operation.clone();
        let adoption_job_id = job_id.clone();
        let adoption = scope.spawn(move || {
            adoption_ready.wait();
            adoption_store.publish_worker_handoff_adoption_receipt(
                &adoption_job_id,
                &adoption_operation,
                &request,
                &response,
            )
        });
        let abort_store = Arc::clone(&store);
        let abort_ready = Arc::clone(&ready);
        let abort_operation = operation.clone();
        let abort = scope.spawn(move || {
            abort_ready.wait();
            abort_store.publish_worker_handoff_abort_fence(&abort_operation, &"e".repeat(64))
        });
        ready.wait();
        (adoption.join().unwrap(), abort.join().unwrap())
    });
    assert_ne!(
        adoption.is_ok(),
        abort.is_ok(),
        "exactly one competing target branch must win"
    );
    let head = store
        .with_state_db(|db| {
            db.read_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                &operation.operation_id,
            )
        })
        .unwrap()
        .expect("winning target branch head");
    if adoption.is_ok() {
        assert_eq!(
            store
                .worker_handoff_adoption_receipt(&operation.operation_id)
                .unwrap()
                .unwrap()
                .response
                .operation_id,
            operation.operation_id
        );
    } else {
        assert_eq!(
            store
                .worker_handoff_abort_fence(&operation.operation_id)
                .unwrap()
                .unwrap()
                .target_operation
                .operation_id,
            operation.operation_id
        );
    }
    assert!(!head.target_hash.is_empty());
}

#[test]
fn contradictory_projected_attachment_cannot_publish_a_branch_head() {
    let store = test_store();
    let operation = handoff_test_operation(
        crate::worker_handoff::WorkerHandoffJobRole::Target,
        "contradictory-adoption",
        "T-source-contradictory-adoption",
    );
    let job_id = seed_projected_target_adoption_job(&store, &operation);
    let (mut request, mut response) = target_adoption_coordinates(&operation);
    request.writer_grant_hash = "f".repeat(64);
    response.target_chain_head_hash = request.target_chain_head_hash.clone();

    assert!(
        store
            .publish_worker_handoff_adoption_receipt(&job_id, &operation, &request, &response,)
            .unwrap_err()
            .to_string()
            .contains("projected attachment authority")
    );
    assert!(
        store
            .with_state_db(|db| {
                db.read_generic_head_ref(
                    crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                    &operation.operation_id,
                )
            })
            .unwrap()
            .is_none()
    );
}

#[test]
fn target_abort_claim_is_durable_for_existing_prepared_job_and_hash_exact() {
    let store = test_store();
    let operation = handoff_test_operation(
        crate::worker_handoff::WorkerHandoffJobRole::Target,
        "abort-claim",
        "T-source-abort-claim",
    );
    let progress = target_handoff_progress(
        &operation.operation_id,
        crate::worker_handoff::WorkerHandoffPhase::TargetPrepared,
    );
    let job_id = format!("worker-handoff-target:{}", operation.operation_id);
    let source_head = operation.source_chain_head_hash.clone();
    store
        .with_state_db(|db| {
            db.create_sync_job_with_initial_progress(
                &ryeos_state::NewSyncJob {
                    job_id: job_id.clone(),
                    operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
                        .to_owned(),
                    operation: operation.to_value()?,
                    peer: Some("source".to_owned()),
                    roots: vec![source_head.clone()],
                    heads: vec![source_head.clone()],
                    max_attempts: crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
                },
                ryeos_state::SyncJobState::Running,
                progress.phase.as_str(),
                Some(&progress.to_value()?),
            )
        })
        .unwrap();
    let request = ryeos_state::NewSyncJob {
        job_id: job_id.clone(),
        operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
        operation: operation.to_value().unwrap(),
        peer: Some("source".to_owned()),
        roots: Vec::new(),
        heads: Vec::new(),
        max_attempts: crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
    };
    let abort_head = "e".repeat(64);
    let claimed = store
        .claim_worker_handoff_target_abort(&request, &abort_head)
        .unwrap();
    assert_eq!(
        claimed.phase,
        crate::worker_handoff::WORKER_HANDOFF_TARGET_ABORT_CLAIM_PHASE
    );
    assert_eq!(claimed.heads, vec![abort_head.clone()]);
    assert_eq!(claimed.roots, vec![source_head]);
    assert_eq!(
        crate::worker_handoff::WorkerSessionHandoffProgress::from_value(
            claimed.result.clone().unwrap()
        )
        .unwrap(),
        progress
    );
    assert!(
        store
            .claim_worker_handoff_target_abort(&request, &"f".repeat(64))
            .is_err()
    );
    assert_eq!(
        store
            .with_state_db(|db| db.get_sync_job(&job_id))
            .unwrap()
            .unwrap(),
        claimed
    );
}

#[test]
fn duplicate_preparation_cannot_regress_post_cut_job_or_roots() {
    for phase in [
        crate::worker_handoff::WorkerHandoffPhase::SourceCommitted,
        crate::worker_handoff::WorkerHandoffPhase::TargetAdopted,
    ] {
        let operation = handoff_test_operation(
            crate::worker_handoff::WorkerHandoffJobRole::Target,
            phase.as_str(),
            "T-source-post-cut",
        );
        let current = target_handoff_progress(&operation.operation_id, phase);
        let requested = target_handoff_progress(
            &operation.operation_id,
            crate::worker_handoff::WorkerHandoffPhase::TargetPrepared,
        );
        let job = ryeos_state::SyncJobRecord {
            job_id: format!("worker-handoff-target:{}", operation.operation_id),
            operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
            operation: operation.to_value().unwrap(),
            peer: Some("source".to_owned()),
            state: ryeos_state::SyncJobState::Running,
            phase: phase.as_str().to_owned(),
            roots: vec![
                operation.transfer_manifest_hash.clone(),
                "a".repeat(64),
                "b".repeat(64),
                "c".repeat(64),
                "d".repeat(64),
            ],
            heads: vec!["d".repeat(64)],
            uploaded_hashes: vec!["d".repeat(64)],
            fetched_hashes: Vec::new(),
            attempt_count: 1,
            max_attempts: crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
            last_error: None,
            result: Some(current.to_value().unwrap()),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            finished_at: None,
        };
        let before = job.clone();
        assert!(
            validate_target_preparation_job_transition(
                &job,
                &operation.transfer_manifest_hash,
                &requested,
            )
            .is_err()
        );
        assert_eq!(job, before);
    }
}

#[test]
fn active_source_handoff_query_fails_closed_on_duplicate_placement_owner() {
    let store = test_store();
    for label in ["source-owner-a", "source-owner-b"] {
        let operation = handoff_test_operation(
            crate::worker_handoff::WorkerHandoffJobRole::Source,
            label,
            "T-one-source-placement",
        );
        let progress = crate::worker_handoff::WorkerSessionHandoffProgress::source_exported(
            operation.operation_id.clone(),
        )
        .unwrap();
        store
            .with_state_db(|db| {
                db.create_sync_job_with_initial_progress(
                    &ryeos_state::NewSyncJob {
                        job_id: format!("worker-handoff-source:{}", operation.operation_id),
                        operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
                            .to_owned(),
                        operation: operation.to_value()?,
                        peer: Some("target".to_owned()),
                        roots: vec![operation.transfer_manifest_hash.clone()],
                        heads: vec![operation.source_chain_head_hash.clone()],
                        max_attempts: crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
                    },
                    ryeos_state::SyncJobState::Running,
                    progress.phase.as_str(),
                    Some(&progress.to_value()?),
                )
            })
            .unwrap();
        if label == "source-owner-a" {
            assert!(
                store
                    .active_source_worker_handoff_for_placement("T-one-source-placement")
                    .unwrap()
                    .is_some()
            );
        }
    }
    assert!(
        store
            .active_source_worker_handoff_for_placement("T-one-source-placement")
            .is_err()
    );
}

#[test]
fn credential_authority_reconciles_a_runtime_commit_gap_on_reopen() {
    let tmp = tempdir().expect("tempdir").keep();
    let identity = ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
        .expect("test node identity");
    let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let trust = Arc::new(trust);
    let open = || {
        TestStateStore::open(&tmp, Arc::clone(&signer), Arc::clone(&trust)).expect("state store")
    };

    let store = open();
    store
        .create_credential_profile(NewCredentialProfile {
            profile_id: "profile-gap",
            owner_principal: "fp:operator",
            home_id: "credential-gap",
        })
        .unwrap();
    store
        .acquire_credential_profile("profile-gap", "fp:operator", "login-gap")
        .unwrap();
    {
        // Deliberately stop after RuntimeDb's durable transition, before
        // StateStore folds the higher revision into OperationalDb.
        let guard = store.lock().unwrap();
        guard
            .runtime_db
            .begin_credential_enrollment(
                "profile-gap",
                "login-gap",
                "ceremony-gap",
                lillux::time::timestamp_millis() as i64 + 60_000,
            )
            .unwrap();
    }
    drop(store);

    let reopened = open();
    let profile = reopened.credential_profile("profile-gap").unwrap().unwrap();
    assert_eq!(profile.state, "enrolling");
    assert_eq!(profile.authority_revision, 2);
    let stable = reopened
        .with_state_db(|db| db.operational_credential_profiles())
        .unwrap();
    assert_eq!(stable.len(), 1);
    assert_eq!(stable[0].authority_revision, 2);
    assert_eq!(stable[0].state, "enrolling");
}

#[test]
fn recreated_credential_profile_persists_its_advanced_generation() {
    let tmp = tempdir().expect("tempdir").keep();
    let identity = ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
        .expect("test node identity");
    let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let trust = Arc::new(trust);
    let open = || {
        TestStateStore::open(&tmp, Arc::clone(&signer), Arc::clone(&trust)).expect("state store")
    };

    let store = open();
    let profile = NewCredentialProfile {
        profile_id: "profile-recreated",
        owner_principal: "fp:operator",
        home_id: "credential-recreated",
    };
    store.create_credential_profile(profile.clone()).unwrap();
    let revoked_generation = store
        .revoke_credential_profile("profile-recreated", "fp:operator", 1)
        .unwrap();
    store
        .finish_credential_profile_revocation(
            "profile-recreated",
            "fp:operator",
            revoked_generation,
        )
        .unwrap();
    let deleted_generation = store
        .begin_credential_profile_deletion("profile-recreated", "fp:operator", revoked_generation)
        .unwrap();
    store
        .finish_credential_profile_deletion("profile-recreated", "fp:operator", deleted_generation)
        .unwrap();
    store.create_credential_profile(profile).unwrap();
    drop(store);

    let reopened = open();
    let recreated = reopened
        .credential_profile("profile-recreated")
        .unwrap()
        .expect("recreated profile after reopen");
    assert_eq!(recreated.credential_generation, deleted_generation + 1);
    assert_eq!(recreated.state, "unauthenticated");
    let stable = reopened
        .with_state_db(|db| db.operational_credential_profiles())
        .unwrap();
    assert_eq!(stable.len(), 1);
    assert_eq!(stable[0].credential_generation, deleted_generation + 1);
    assert_eq!(stable[0].state, "unauthenticated");
}

#[test]
fn terminal_credential_cancellation_folds_stable_authority_and_reopens() {
    let tmp = tempdir().expect("tempdir").keep();
    let identity = ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
        .expect("test node identity");
    let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let trust = Arc::new(trust);
    let open = || {
        TestStateStore::open(&tmp, Arc::clone(&signer), Arc::clone(&trust)).expect("state store")
    };

    let store = open();
    {
        let guard = store.lock().unwrap();
        guard
            .runtime_db
            .reserve_workspace("W-terminal-profile", &"a".repeat(64), "/workspace")
            .unwrap();
        guard
            .runtime_db
            .transition_workspace(
                "W-terminal-profile",
                &[runtime_db::WorkspaceState::Reserved],
                runtime_db::WorkspaceState::Constructing,
                None,
            )
            .unwrap();
        guard
            .runtime_db
            .claim_workspace_construction(
                "W-terminal-profile",
                "T-terminal-profile",
                "launch-owner",
            )
            .unwrap();
        guard
            .runtime_db
            .bind_workspace(runtime_db::WorkspaceBinding {
                workspace_id: "W-terminal-profile",
                thread_id: "T-terminal-profile",
                launch_owner: Some("launch-owner"),
                backend_id: Some("native"),
                backend_version: Some("fixture"),
                pinned_root_identities: Some("fixture-roots"),
                mount_identity: Some("fixture-view"),
                workspace_output_partition_identity: None,
                base_output_capture_hash: None,
            })
            .unwrap();
    }
    store
        .create_credential_profile(NewCredentialProfile {
            profile_id: "P-terminal-profile",
            owner_principal: "fp:operator",
            home_id: "credential-terminal-profile",
        })
        .unwrap();
    store
        .acquire_credential_profile(
            "P-terminal-profile",
            "fp:operator",
            "worker-terminal-profile",
        )
        .unwrap();
    store
        .admit_dedicated_session(NewDedicatedSession {
            placement_thread_id: "T-terminal-profile",
            chain_root_id: "T-terminal-profile",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"b".repeat(64),
            workspace_id: "W-terminal-profile",
            candidate_required: false,
            candidate_disposition: runtime_db::DedicatedCandidateDisposition::OwnerDecision,
            credential_profile_id: "P-terminal-profile",
            credential_generation: 1,
            credential_lock_owner: "worker-terminal-profile",
        })
        .unwrap();
    {
        let guard = store.lock().unwrap();
        guard
            .runtime_db
            .attach_worker_process(&WorkerProcessRecord {
                worker_instance_id: "worker-terminal-profile".to_owned(),
                boot_identity_hash: "c".repeat(64),
                session_capsule_hash: "b".repeat(64),
                boot_epoch: 1,
                lifecycle_generation: 1,
                process_identity: crate::process::ExecutionProcessIdentity {
                    schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                    boot_id: "fixture-boot".to_owned(),
                    target_pid: 101,
                    target_start_time_ticks: 201,
                    group_leader_pid: 101,
                    group_leader_start_time_ticks: 201,
                    process_scope: None,
                    resource_selections: Vec::new(),
                    resource_settlement_authority: None,
                    resource_operations: Vec::new(),
                    resource_allocation_limit: None,
                    resource_occupancy_start: None,
                    resource_occupancy_limit: None,
                    resource_cleanup_allowance_ms: None,
                },
                control_channel_identity: "fixture-control".to_owned(),
                state: runtime_db::WorkerProcessState::Attached,
                daemon_generation_id: "daemon:fixture".to_owned(),
                placement_thread_id: "T-terminal-profile".to_owned(),
                cleanup_state: "owned".to_owned(),
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .unwrap();
    }
    store
        .complete_worker_binding("worker-terminal-profile", "T-terminal-profile", 1)
        .unwrap();
    store
        .begin_credential_enrollment(
            "P-terminal-profile",
            "worker-terminal-profile",
            "login-terminal-profile",
            lillux::time::timestamp_millis() as i64 + 60_000,
        )
        .unwrap();
    store
        .settle_worker_process(
            "worker-terminal-profile",
            "T-terminal-profile",
            1,
            "reaped",
            "cancelled",
        )
        .unwrap();
    store
        .terminalize_dedicated_session(
            "T-terminal-profile",
            "worker-terminal-profile",
            1,
            "cancelled",
        )
        .unwrap();
    let stable = store
        .with_state_db(|db| db.operational_credential_profiles())
        .unwrap();
    let stable = stable
        .iter()
        .find(|profile| profile.profile_id == "P-terminal-profile")
        .unwrap();
    assert_eq!(stable.state, "unauthenticated");
    assert_eq!(stable.authority_revision, 3);
    drop(store);

    let reopened = open();
    let runtime = reopened
        .credential_profile("P-terminal-profile")
        .unwrap()
        .unwrap();
    assert_eq!(runtime.state, "unauthenticated");
    assert_eq!(runtime.authority_revision, 3);
    let stable = reopened
        .with_state_db(|db| db.operational_credential_profiles())
        .unwrap();
    let stable = stable
        .iter()
        .find(|profile| profile.profile_id == "P-terminal-profile")
        .unwrap();
    assert_eq!(stable.state, "unauthenticated");
    assert_eq!(stable.authority_revision, 3);
}

#[test]
fn replay_object_verification_runs_outside_the_state_mutex() {
    let store = test_store();
    let namespace = ryeos_state::ReplayIndexNamespace::new("dispatch.effect").unwrap();
    let indexed = ryeos_state::ReplayIndexRecord {
        cache_key: "a".repeat(64),
        answer_digest: "b".repeat(64),
        record_hash: "c".repeat(64),
    };
    store
        .with_state_db(|db| {
            db.publish_replay_record(&namespace, &indexed, |_| {
                ryeos_state::ReplayRecordVerification::Verified
            })
        })
        .expect("publish replay row");

    let outcome = store
        .lookup_replay_record(&namespace, &indexed.cache_key, |_| {
            assert!(
                store.inner.try_lock().is_ok(),
                "CAS verification must not hold the node-wide state mutex"
            );
            ryeos_state::ReplayRecordVerification::Verified
        })
        .expect("lookup replay row");
    assert_eq!(outcome, ryeos_state::ReplayLookupOutcome::Present(indexed));
}

#[test]
fn predecessor_released_binding_head_requires_explicit_epoch_reset() {
    let tmp = tempdir().unwrap();
    let runtime_state_dir = tmp.path().join(".ai/state");
    let identity =
        ryeos_node_runtime::identity::NodeIdentity::create(&tmp.path().join("node-key.pem"))
            .expect("test node identity");
    let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let trust = Arc::new(trust);
    let state =
        ryeos_state::test_support::TestState::open(&runtime_state_dir, Arc::clone(&trust)).unwrap();
    let authority = state.pinned_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let active = ryeos_state::objects::ExternalContentBinding::active(
        "a".repeat(64),
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
        ryeos_state::objects::ExternalContentConsumerAuthority::installed_bundle(
            "worker:tests/old".to_owned(),
            "b".repeat(64),
        )
        .unwrap(),
        identity.fingerprint().to_owned(),
        "c".repeat(64),
        "b".repeat(64),
    )
    .unwrap();
    let released = ryeos_state::objects::ExternalContentBinding::released_from(
        &active,
        "c".repeat(64),
        "b".repeat(64),
    )
    .unwrap();
    let binding_hash = authority
        .cas_store()
        .unwrap()
        .store_object(&released.to_value().unwrap())
        .unwrap();
    state
        .write_generic_head_ref(
            ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
            &released.binding_subject_id,
            &binding_hash,
            signer.as_ref(),
            &guard,
        )
        .unwrap();
    drop(guard);
    drop(authority);
    drop(state);

    let refusal = TestStateStore::open_refusal(tmp.path(), signer, trust);
    assert!(
        refusal.contains("ryeos node reset external-content-bindings"),
        "{refusal}"
    );
}

#[test]
fn daemon_bundle_event_append_serializes_concurrent_writers_at_current_head() {
    const WRITERS: usize = 12;
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let start = Arc::new(std::sync::Barrier::new(WRITERS));
    let writers = (0..WRITERS)
        .map(|index| {
            let store = Arc::clone(&store);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                store
                    .append_daemon_bundle_event_at_current_head(
                        ryeos_state::BundleEventAppendRequest {
                            effective_bundle_id: "ryeos-node".to_string(),
                            bundle_id: None,
                            event_kind: "admission".to_string(),
                            chain_id: "project-test".to_string(),
                            event_type: "admission_recorded".to_string(),
                            schema_version: 1,
                            payload: json!({"writer": index}),
                            expected_chain_head_hash: None,
                            idempotency_key: None,
                            correlation_id: None,
                            causation_id: None,
                            attribution: ryeos_state::objects::BundleEventAttribution::default(),
                            attachments: vec![],
                        },
                    )
                    .expect("daemon-owned append")
            })
        })
        .collect::<Vec<_>>();

    for writer in writers {
        writer.join().expect("writer thread");
    }

    let page = store
        .read_bundle_event_chain_page(
            "ryeos-node",
            "admission",
            "project-test",
            None,
            WRITERS,
            ryeos_state::objects::MAX_BUNDLE_EVENT_SERIALIZED_BYTES,
        )
        .expect("read daemon-owned event chain");
    assert_eq!(page.records.len(), WRITERS);
    let mut sequences = page
        .records
        .iter()
        .map(|record| record.event.chain_seq)
        .collect::<Vec<_>>();
    sequences.sort_unstable();
    assert_eq!(sequences, (1..=WRITERS as u64).collect::<Vec<_>>());
}

#[test]
fn event_append_readback_proves_exact_batch_from_both_predecessor_anchors() {
    let predecessor_chain_hash = "a".repeat(64);
    let predecessor_thread_hash = "b".repeat(64);
    let first_hash = "c".repeat(64);
    let second_hash = "d".repeat(64);
    let predecessor = ryeos_state::AuthoritativeThreadAppendAnchorReadback {
        chain_last_event_hash: Some(predecessor_chain_hash.clone()),
        chain_last_seq: 40,
        thread_last_event_hash: Some(predecessor_thread_hash.clone()),
        thread_last_seq: 7,
    };
    let first = positioned_test_event(
        "T-root",
        41,
        8,
        Some(predecessor_chain_hash),
        Some(predecessor_thread_hash),
    );
    let second = positioned_test_event(
        "T-root",
        42,
        9,
        Some(first_hash.clone()),
        Some(first_hash.clone()),
    );
    let readback = ryeos_state::AuthoritativeThreadAppendHistoryReadback {
        anchor: ryeos_state::AuthoritativeThreadAppendAnchorReadback {
            chain_last_event_hash: Some(second_hash.clone()),
            chain_last_seq: 42,
            thread_last_event_hash: Some(second_hash),
            thread_last_seq: 9,
        },
        events: vec![
            (first_hash, first.clone()),
            ("d".repeat(64), second.clone()),
        ],
    };

    assert!(matches!(
        classify_event_append_readback(
            Some(&readback),
            &predecessor,
            &[first, second]
        )
        .unwrap(),
        EventAppendCommitReadback::Exact(events) if events.len() == 2
    ));
}

#[test]
fn event_append_readback_only_retries_a_proven_unchanged_head() {
    let predecessor = ryeos_state::AuthoritativeThreadAppendAnchorReadback {
        chain_last_event_hash: Some("a".repeat(64)),
        chain_last_seq: 10,
        thread_last_event_hash: Some("b".repeat(64)),
        thread_last_seq: 4,
    };
    let unchanged = ryeos_state::AuthoritativeThreadAppendHistoryReadback {
        anchor: predecessor.clone(),
        events: Vec::new(),
    };
    let expected = positioned_test_event("T-root", 0, 1, None, None);
    assert!(matches!(
        classify_event_append_readback(
            Some(&unchanged),
            &predecessor,
            std::slice::from_ref(&expected)
        )
        .unwrap(),
        EventAppendCommitReadback::ProvenAbsent
    ));

    let advanced = ryeos_state::AuthoritativeThreadAppendHistoryReadback {
        anchor: ryeos_state::AuthoritativeThreadAppendAnchorReadback {
            chain_last_event_hash: Some("c".repeat(64)),
            chain_last_seq: 11,
            thread_last_event_hash: Some("d".repeat(64)),
            thread_last_seq: 5,
        },
        events: vec![(
            "d".repeat(64),
            ryeos_state::objects::ThreadEvent {
                payload: json!({"different": true}),
                ..expected
            },
        )],
    };
    assert!(matches!(
        classify_event_append_readback(
            Some(&advanced),
            &predecessor,
            &[positioned_test_event("T-root", 0, 1, None, None)]
        )
        .unwrap(),
        EventAppendCommitReadback::Ambiguous(_)
    ));
}

#[test]
fn atomic_continuation_positions_reject_injected_source_event() {
    let predecessor_chain_hash = "a".repeat(64);
    let predecessor_source_hash = "b".repeat(64);
    let successor_hash = "c".repeat(64);
    let predecessor = ryeos_state::AuthoritativeContinuationPredecessorReadback {
        updated_at: "2026-07-24T00:00:00.000Z".to_string(),
        last_event_hash: Some(predecessor_chain_hash.clone()),
        last_chain_seq: 10,
        source_was_present: true,
        source_last_event_hash: Some(predecessor_source_hash.clone()),
        source_last_thread_seq: 7,
        successor_was_absent: true,
    };
    let successor_events = vec![(
        successor_hash.clone(),
        positioned_test_event("T-successor", 11, 1, Some(predecessor_chain_hash), None),
    )];
    let exact_source = positioned_test_event(
        "T-source",
        12,
        8,
        Some(successor_hash),
        Some(predecessor_source_hash),
    );
    assert!(has_exact_atomic_continuation_positions(
        &predecessor,
        &successor_events,
        &exact_source,
    ));

    let injected_hash = "d".repeat(64);
    let source_after_injected_event = positioned_test_event(
        "T-source",
        13,
        9,
        Some(injected_hash.clone()),
        Some(injected_hash),
    );
    assert!(!has_exact_atomic_continuation_positions(
        &predecessor,
        &successor_events,
        &source_after_injected_event,
    ));
}

#[test]
fn exact_root_event_proof_rejects_extra_or_divergent_events() {
    let first_hash = "e".repeat(64);
    let second_hash = "f".repeat(64);
    let third_hash = "1".repeat(64);
    let first = positioned_test_event("T-root", 1, 1, None, None);
    let second = positioned_test_event(
        "T-root",
        2,
        2,
        Some(first_hash.clone()),
        Some(first_hash.clone()),
    );
    let expected = vec![first.clone(), second.clone()];
    let mut with_extra = vec![(first_hash.clone(), first), (second_hash.clone(), second)];
    with_extra.push((
        third_hash,
        positioned_test_event("T-root", 3, 3, Some(second_hash.clone()), Some(second_hash)),
    ));
    assert!(has_exact_root_event_positions(&with_extra));
    assert!(!same_successor_event_batch(&with_extra, &expected));

    let mut divergent = with_extra[..2].to_vec();
    divergent[1].1.prev_thread_event_hash = Some("2".repeat(64));
    assert!(!has_exact_root_event_positions(&divergent));

    let expected_snapshot = build_snapshot(&thread_record("T-root", "T-root"));
    let mut divergent_snapshot = expected_snapshot.clone();
    divergent_snapshot.result = Some(json!({"unexpected": true}));
    assert!(!same_exact_snapshot(
        &divergent_snapshot,
        &expected_snapshot,
    ));
}

#[tokio::test]
async fn unbound_launch_cancel_commits_then_aborts_registered_task() {
    let store = test_store();
    let launch_id = store
        .reserve_launch_planning("T-planning", "fp:owner")
        .expect("reserve planning");
    assert!(
        store
            .get_thread("T-planning")
            .expect("read absent authoritative row")
            .is_none()
    );
    let task = tokio::spawn(std::future::pending::<()>());
    store
        .register_launch_task_abort("T-planning", task.abort_handle())
        .expect("register abort signal");

    assert_eq!(
        store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("cancel planning"),
        Some(LaunchCancellationResolution::Cancelled)
    );
    assert!(task.await.expect_err("task must be aborted").is_cancelled());
    store
        .settle_launch_planning_task_exit("T-planning")
        .expect("cancelled task exit must preserve cancelled outcome");
    assert!(
        store
            .get_thread("T-planning")
            .expect("read absent authoritative row after cancel")
            .is_none()
    );
    assert_eq!(
        store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("read terminal planning outcome"),
        Some(LaunchCancellationResolution::Terminal {
            state: "cancelled".to_string(),
            outcome_code: Some("cancelled_by_requester".to_string()),
        })
    );
}

#[test]
fn caller_retained_launch_status_is_exact_owner_bound_and_terminal_on_admission_exit() {
    let store = test_store();
    let launch_id = "L-0123456789abcdef0123456789abcdef";
    assert!(is_canonical_launch_id(launch_id));
    assert!(!is_canonical_launch_id("L-not-a-canonical-coordinate"));
    store
        .reserve_launch_planning_with_id(launch_id, "T-reserved", "fp:owner")
        .expect("reserve caller coordinate");

    assert_eq!(
        store
            .launch_planning_status(launch_id, "fp:owner")
            .expect("read owner status"),
        Some(LaunchPlanningStatus {
            launch_id: launch_id.to_string(),
            thread_id: None,
            status: "planning".to_string(),
            outcome_code: None,
        })
    );
    assert_eq!(
        store
            .launch_planning_status(launch_id, "fp:other")
            .expect("foreign read is indistinguishable from absence"),
        None
    );

    store
        .settle_launch_planning_admission_exit("T-reserved")
        .expect("settle pre-handoff refusal");
    assert_eq!(
        store
            .launch_planning_status(launch_id, "fp:owner")
            .expect("read terminal owner status"),
        Some(LaunchPlanningStatus {
            launch_id: launch_id.to_string(),
            thread_id: None,
            status: "failed".to_string(),
            outcome_code: Some("launch_admission_failed".to_string()),
        })
    );
}

#[test]
fn owner_status_repairs_committed_root_and_refuses_owner_disagreement() {
    let store = test_store();
    let thread = thread_record("T-status-repair", "T-status-repair");
    store
        .create_root_thread_with_events(&thread, Vec::new())
        .expect("publish authoritative root before auxiliary planning row");
    let launch_id = "L-11111111111111111111111111111111";
    store
        .reserve_launch_planning_with_id(launch_id, &thread.thread_id, "fp:test")
        .expect("reserve interrupted auxiliary planning row");
    assert_eq!(
        store
            .launch_planning_status(launch_id, "fp:test")
            .expect("status repairs exact committed root"),
        Some(LaunchPlanningStatus {
            launch_id: launch_id.to_string(),
            thread_id: Some(thread.thread_id.clone()),
            status: "bound".to_string(),
            outcome_code: Some("thread_bound".to_string()),
        })
    );

    let mismatched_thread = thread_record("T-status-mismatch", "T-status-mismatch");
    store
        .create_root_thread_with_events(&mismatched_thread, Vec::new())
        .expect("publish second authoritative root");
    let mismatched_id = "L-22222222222222222222222222222222";
    store
        .reserve_launch_planning_with_id(mismatched_id, &mismatched_thread.thread_id, "fp:other")
        .expect("reserve deliberately mismatched planning owner");
    let error = store
        .launch_planning_status(mismatched_id, "fp:other")
        .expect_err("authoritative owner disagreement must fail closed");
    assert!(error.to_string().contains("owner disagrees"), "{error:#}");
}

#[tokio::test]
async fn cancel_before_task_registration_aborts_at_registration() {
    let store = test_store();
    let launch_id = store
        .reserve_launch_planning("T-late-registration", "fp:owner")
        .expect("reserve planning");
    store
        .cancel_launch_planning(&launch_id, "fp:owner")
        .expect("cancel planning");
    let task = tokio::spawn(std::future::pending::<()>());
    store
        .register_launch_task_abort("T-late-registration", task.abort_handle())
        .expect("late registration");
    assert!(task.await.expect_err("task must be aborted").is_cancelled());
}

#[tokio::test]
async fn launch_abort_registry_capacity_refusal_aborts_and_can_be_settled() {
    let store = test_store();
    store
        .reserve_launch_planning("T-capacity-one", "fp:owner")
        .expect("reserve first planning");
    let refused_launch_id = store
        .reserve_launch_planning("T-capacity-two", "fp:owner")
        .expect("reserve second planning");
    let first = tokio::spawn(std::future::pending::<()>());
    store
        .register_launch_task_abort_bounded("T-capacity-one", first.abort_handle(), 1)
        .expect("register first signal");
    let refused = tokio::spawn(std::future::pending::<()>());
    assert!(matches!(
        store.register_launch_task_abort_bounded("T-capacity-two", refused.abort_handle(), 1,),
        Err(LaunchTaskAbortRegistrationError::CapacityExceeded)
    ));
    assert!(
        refused
            .await
            .expect_err("capacity-refused task must be aborted")
            .is_cancelled()
    );

    store
        .settle_launch_planning_task_exit("T-capacity-two")
        .expect("settle refused planning");
    assert_eq!(
        store
            .cancel_launch_planning(&refused_launch_id, "fp:owner")
            .expect("read settled planning"),
        Some(LaunchCancellationResolution::Terminal {
            state: "failed".to_string(),
            outcome_code: Some("thread_creation_failed".to_string()),
        })
    );
    first.abort();
    assert!(
        first
            .await
            .expect_err("first task must be aborted during cleanup")
            .is_cancelled()
    );
}

#[test]
fn planning_reservation_maps_only_typed_capacity_and_preserves_internal_errors() {
    let capacity = map_launch_planning_reservation_error(
        anyhow::Error::new(LaunchPlanningCapacityExceeded).context("reserve durable planning row"),
    );
    assert!(matches!(
        capacity,
        LaunchPlanningReservationError::CapacityExceeded(_)
    ));

    let internal =
        map_launch_planning_reservation_error(anyhow::anyhow!("runtime database unavailable"));
    assert!(matches!(
        internal,
        LaunchPlanningReservationError::Internal(_)
    ));
}

#[test]
fn foreign_launch_id_is_indistinguishable_from_nonexistent_and_does_not_cancel() {
    let store = test_store();
    let launch_id = store
        .reserve_launch_planning("T-owned-planning", "fp:owner")
        .expect("reserve planning");
    let foreign = store
        .cancel_launch_planning(&launch_id, "fp:other")
        .expect("hide non-owner");
    let nonexistent = store
        .cancel_launch_planning("L-does-not-exist", "fp:other")
        .expect("hide nonexistent");
    assert_eq!(foreign, nonexistent);
    assert_eq!(foreign, None);
    store
        .ensure_launch_planning_active("T-owned-planning")
        .expect("non-owner must not mutate planning");
}

#[tokio::test]
async fn row_commit_before_handoff_routes_cancel_to_thread_without_aborting_launch_task() {
    let store = test_store();
    let launch_id = store
        .reserve_launch_planning("T-row-before-handoff", "fp:owner")
        .expect("reserve planning");
    let task = tokio::spawn(std::future::pending::<()>());
    store
        .register_launch_task_abort("T-row-before-handoff", task.abort_handle())
        .expect("register launch task");
    let mut root = thread_record("T-row-before-handoff", "T-row-before-handoff");
    root.requested_by = Some("fp:owner".to_string());
    store
        .create_thread_for_test(&root)
        .expect("commit and bind authoritative root");

    assert_eq!(
        store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("resolve post-row cancel"),
        Some(LaunchCancellationResolution::Bound {
            thread_id: "T-row-before-handoff".to_string(),
        })
    );
    assert!(
        !task.is_finished(),
        "binding must transfer cancellation to the durable thread rather than aborting its launch task"
    );
    task.abort();
    assert!(
        task.await
            .expect_err("test cleanup must abort launch task")
            .is_cancelled()
    );
    store
        .settle_launch_planning_task_exit("T-row-before-handoff")
        .expect("post-bind task exit must preserve thread binding");
    assert_eq!(
        store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("read binding after task exit"),
        Some(LaunchCancellationResolution::Bound {
            thread_id: "T-row-before-handoff".to_string(),
        })
    );
}

#[test]
fn cancel_after_handoff_resolves_to_canonical_durable_thread_stop() {
    let store = test_store();
    let launch_id = store
        .reserve_launch_planning("T-after-handoff", "fp:owner")
        .expect("reserve planning");
    let mut root = thread_record("T-after-handoff", "T-after-handoff");
    root.requested_by = Some("fp:owner".to_string());
    store
        .create_thread_for_test(&root)
        .expect("commit and bind authoritative root");

    let foreign = store
        .cancel_launch_planning(&launch_id, "fp:other")
        .expect("hide foreign handed-off launch");
    let nonexistent = store
        .cancel_launch_planning("L-missing-after-handoff", "fp:other")
        .expect("hide nonexistent launch");
    assert_eq!(foreign, nonexistent);
    assert_eq!(foreign, None);

    let resolution = store
        .cancel_launch_planning(&launch_id, "fp:owner")
        .expect("resolve handed-off launch")
        .expect("owned launch");
    let LaunchCancellationResolution::Bound { thread_id } = resolution else {
        panic!("handed-off launch must resolve to its durable thread");
    };
    let runtime = store
        .request_thread_stop(&thread_id, StopIntent::Cancel)
        .expect("request canonical durable stop");
    assert_eq!(runtime.stop_intent, Some(StopIntent::Cancel));
    assert_eq!(thread_id, "T-after-handoff");
}

#[test]
fn pre_bind_augmentation_or_launch_task_exit_settles_failed() {
    let store = test_store();
    let launch_id = store
        .reserve_launch_planning("T-pre-bind-task-exit", "fp:owner")
        .expect("reserve planning");

    store
        .settle_launch_planning_task_exit("T-pre-bind-task-exit")
        .expect("settle pre-bind task exit");

    assert!(
        store
            .get_thread("T-pre-bind-task-exit")
            .expect("read absent authoritative row")
            .is_none()
    );
    assert_eq!(
        store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("read settled launch outcome"),
        Some(LaunchCancellationResolution::Terminal {
            state: "failed".to_string(),
            outcome_code: Some("thread_creation_failed".to_string()),
        })
    );
}

#[test]
fn root_publication_binds_planning_before_return() {
    let store = test_store();
    let thread_id = "T-root-direct-bind";
    let launch_id = store
        .reserve_launch_planning(thread_id, "fp:owner")
        .expect("reserve root planning");
    let mut root = thread_record(thread_id, thread_id);
    root.requested_by = Some("fp:owner".to_string());

    let publication = store
        .create_root_thread_with_events_and_launch_metadata(&root, Vec::new(), None)
        .expect("publish root and bind planning");
    assert_eq!(publication.successor.thread_id, thread_id);
    assert_eq!(publication.successor.chain_root_id, thread_id);
    assert!(publication.successor.upstream_thread_id.is_none());
    assert_eq!(publication.successor.status, ThreadStatus::Created.as_str());
    assert!(publication.successor.runtime.pid.is_none());
    assert!(publication.successor.runtime.launch_metadata.is_none());

    let g = store.lock().expect("inspect root publication");
    let planning = g
        .runtime_db
        .launch_planning_by_thread(thread_id)
        .expect("read planning directly")
        .expect("root planning record");
    assert_eq!(planning.launch_id, launch_id);
    assert_eq!(planning.state, "bound");
    assert_eq!(planning.bound_thread_id.as_deref(), Some(thread_id));
    assert!(
        g.state_db
            .get_thread(thread_id)
            .expect("read authoritative root directly")
            .is_some()
    );
}

#[test]
fn launch_cancel_and_authoritative_root_bind_have_one_atomic_winner() {
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let launch_id = store
        .reserve_launch_planning("T-bind-race", "fp:owner")
        .expect("reserve planning");
    let barrier = Arc::new(std::sync::Barrier::new(3));

    let creator_store = store.clone();
    let creator_barrier = barrier.clone();
    let creator = std::thread::spawn(move || {
        creator_barrier.wait();
        let mut root = thread_record("T-bind-race", "T-bind-race");
        root.requested_by = Some("fp:owner".to_string());
        creator_store.create_thread_for_test(&root)
    });

    let canceller_store = store.clone();
    let canceller_barrier = barrier.clone();
    let canceller = std::thread::spawn(move || {
        canceller_barrier.wait();
        canceller_store.cancel_launch_planning(&launch_id, "fp:owner")
    });

    barrier.wait();
    let created = creator.join().expect("creator thread");
    let cancelled = canceller
        .join()
        .expect("canceller thread")
        .expect("cancel resolution")
        .expect("owned planning record");
    let outcomes = (created, cancelled);
    match outcomes {
        (Ok(_), LaunchCancellationResolution::Bound { thread_id }) => {
            assert_eq!(thread_id, "T-bind-race")
        }
        (Err(error), LaunchCancellationResolution::Cancelled) => {
            assert!(
                error
                    .chain()
                    .any(|cause| cause.is::<LaunchPlanningInactive>()),
                "cancel-won publication must retain typed planning-inactive authority: {error:#}"
            );
        }
        (created, cancelled) => {
            panic!(
                "invalid cancel-vs-bind race outcome: created={created:?}, cancelled={cancelled:?}"
            )
        }
    }
}

#[test]
fn continuation_publication_binds_planning_before_return() {
    let store = test_store();
    let source_thread_id = "T-continuation-direct-bind-source";
    let successor_thread_id = "T-continuation-direct-bind-successor";
    let mut source = thread_record(source_thread_id, source_thread_id);
    source.requested_by = Some("fp:owner".to_string());
    store
        .create_thread_for_test(&source)
        .expect("create continuation source");
    let launch_id = store
        .reserve_launch_planning(successor_thread_id, "fp:owner")
        .expect("reserve continuation planning");
    let mut successor = thread_record(successor_thread_id, source_thread_id);
    successor.requested_by = Some("fp:owner".to_string());

    let publication = store
        .create_continuation_admitted(
            &successor,
            source_thread_id,
            source_thread_id,
            Some("chained_resume"),
            Vec::new(),
            None,
        )
        .expect("publish continuation and bind planning");
    assert_eq!(publication.successor.thread_id, successor_thread_id);
    assert_eq!(publication.successor.chain_root_id, source_thread_id);
    assert_eq!(
        publication.successor.upstream_thread_id.as_deref(),
        Some(source_thread_id)
    );
    assert_eq!(publication.successor.status, ThreadStatus::Created.as_str());
    assert!(publication.successor.runtime.pid.is_none());
    assert!(publication.successor.runtime.launch_metadata.is_none());
    assert_eq!(
        store
            .current_chain_placement_thread_id(source_thread_id)
            .expect("resolve authoritative placement"),
        Some(successor_thread_id.to_string())
    );
    assert_eq!(
        store
            .current_chain_placement_thread_id("T-absent-chain")
            .expect("resolve absent chain"),
        None
    );

    let g = store.lock().expect("inspect continuation publication");
    let planning = g
        .runtime_db
        .launch_planning_by_thread(successor_thread_id)
        .expect("read planning directly")
        .expect("continuation planning record");
    assert_eq!(planning.launch_id, launch_id);
    assert_eq!(planning.state, "bound");
    assert_eq!(
        planning.bound_thread_id.as_deref(),
        Some(successor_thread_id)
    );
    assert!(
        g.state_db
            .get_thread(successor_thread_id)
            .expect("read authoritative successor directly")
            .is_some()
    );
    assert_eq!(
        queries::continuation_successor(g.state_db.projection(), source_thread_id)
            .expect("read continuation edge directly")
            .as_deref(),
        Some(successor_thread_id)
    );
}

#[test]
fn cancelled_planning_refuses_continuation_publication_with_typed_inactivity() {
    let store = test_store();
    let source_thread_id = "T-continuation-cancelled-source";
    let successor_thread_id = "T-continuation-cancelled-successor";
    let mut source = thread_record(source_thread_id, source_thread_id);
    source.requested_by = Some("fp:owner".to_string());
    store
        .create_thread_for_test(&source)
        .expect("create continuation source");
    let launch_id = store
        .reserve_launch_planning(successor_thread_id, "fp:owner")
        .expect("reserve continuation planning");
    assert_eq!(
        store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("cancel continuation planning"),
        Some(LaunchCancellationResolution::Cancelled)
    );

    let mut successor = thread_record(successor_thread_id, source_thread_id);
    successor.requested_by = Some("fp:owner".to_string());
    let error = store
        .create_continuation_for_test(
            &successor,
            source_thread_id,
            source_thread_id,
            Some("chained_resume"),
        )
        .expect_err("cancelled planning must fence continuation publication");
    assert!(
        error
            .chain()
            .any(|cause| cause.is::<LaunchPlanningInactive>()),
        "continuation publication must retain typed planning inactivity: {error:#}"
    );
    assert!(
        store
            .get_thread(successor_thread_id)
            .expect("read absent continuation successor")
            .is_none()
    );
    let source = store
        .get_thread(source_thread_id)
        .expect("read unchanged continuation source")
        .expect("continuation source");
    assert_eq!(source.status, ThreadStatus::Created.as_str());
    assert!(source.successor_thread_id.is_none());
}
