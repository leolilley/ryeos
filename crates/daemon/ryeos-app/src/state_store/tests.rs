//! Unit tests of the state store: shared fixtures, with the cases in sibling modules.
use super::test_native::TestStateStore;
use super::*;
use ryeos_engine::contracts::{EffectivePrincipal, ExecutionHints, Principal, ProjectContext};
use std::io::Write as _;
use tempfile::tempdir;

mod birth_tests;
mod core_tests;
mod launch_tests;
mod workspace_handoff_tests;

pub(super) fn projected_event_fixture(
    durability: ryeos_state::objects::EventDurability,
) -> (ryeos_state::objects::ThreadEvent, queries::EventRow) {
    let event = ryeos_state::objects::ThreadEvent {
        schema: 1,
        kind: "thread_event".into(),
        chain_root_id: "T-projected".into(),
        chain_seq: 3,
        thread_id: "T-projected-child".into(),
        thread_seq: 2,
        event_type: "test_observation".into(),
        durability,
        ts: "2026-10-09T00:00:00Z".into(),
        prev_chain_event_hash: Some("a".repeat(64)),
        prev_thread_event_hash: Some("b".repeat(64)),
        payload: json!({"operation_id": "projection-control", "value": 7}),
    };
    let row = queries::EventRow {
        event_id: 3,
        event_hash: ryeos_state::objects::thread_event::hash_event(&event).unwrap(),
        chain_root_id: event.chain_root_id.clone(),
        chain_seq: 3,
        thread_id: event.thread_id.clone(),
        thread_seq: 2,
        event_type: event.event_type.clone(),
        durability: event.durability.to_string(),
        ts: event.ts.clone(),
        prev_chain_event_hash: event.prev_chain_event_hash.clone(),
        prev_thread_event_hash: event.prev_thread_event_hash.clone(),
        payload: serde_json::to_vec(&event.payload).unwrap(),
    };
    (event, row)
}

pub(super) fn append_indexed(
    store: &TestStateStore,
    thread_id: &str,
    count: usize,
) -> Vec<PersistedEventRecord> {
    let authored = (0..count)
        .map(|index| NewEventRecord {
            event_type: "test.history.event".into(),
            storage_class: "indexed".into(),
            payload: json!({"index": index}),
        })
        .collect::<Vec<_>>();
    store
        .append_events(thread_id, thread_id, &authored)
        .expect("append events")
}

pub(super) fn rooted_bundle_proof_fixture(
    key: &lillux::crypto::SigningKey,
    fingerprint: &str,
) -> ryeos_state::objects::AdmittedLaunchCapsule {
    use ryeos_state::objects::{
        AdmittedDirectCommandClosure as Command, AdmittedExecutionClosure as Closure,
        AdmittedLaunchArtifactIdentity as Artifact, DirectExecutableIdentity as Executable,
        DirectRootSourceIdentity, DirectRuntimeIdentity, DirectRuntimeSourceSpace,
        ExecutionLaunchDriver, ExecutionLifecycleAuthority, ExecutionOwnershipAuthority,
        ExecutionProjectAuthority, ExecutionRecoveryAuthority, RetainedItemSpaceV1 as Space,
        RetainedTrustClassV1 as Trust,
    };

    let triple = "x86_64-unknown-linux-gnu";
    let item_ref = format!("bin/{triple}/runner");
    let item_source = json!({
        "kind": "item_source",
        "item_ref": item_ref,
        "content_blob_hash": "f".repeat(64),
        "integrity": format!("sha256:{}", "f".repeat(64)),
        "signature_info": null,
        "mode": 0o755,
    });
    let item_source_bytes = lillux::canonical_json(&item_source).unwrap();
    let item_source_hash = lillux::sha256_hex(item_source_bytes.as_bytes());
    let manifest = json!({
        "kind": "source_manifest",
        "item_source_hashes": { item_ref.clone(): item_source_hash.clone() },
    });
    let manifest_hash = lillux::sha256_hex(lillux::canonical_json(&manifest).unwrap().as_bytes());
    let manifest_ref_body = format!("ryeos:bundle-executor-manifest\n{manifest_hash}\n");
    let signed_manifest_ref = lillux::signature::sign_content(&manifest_ref_body, key, "#", None);
    let signed_sidecar = lillux::signature::sign_content(&item_source_bytes, key, "#", None);
    let proof = ryeos_state::objects::BundleExecutorMemberSourceProofV1 {
        requested_binary_ref: "bin:runner".to_owned(),
        request_origin: ryeos_state::objects::BundleExecutorRequestOriginV1::WrapperBundle {
            wrapper_bundle_name: "fixture".to_owned(),
        },
        selected_bundle: ryeos_state::objects::SelectedBundleIdentityV1 {
            name: "fixture".to_owned(),
            body_digest: "a".repeat(64),
            signer_fingerprint: fingerprint.to_owned(),
        },
        runtime_selection: ryeos_state::objects::RetainedRuntimeBinarySelectionV1 {
            definition_owner: ryeos_state::objects::RetainedRuntimeDefinitionOwnerV1 {
                requested_id: "runtime:fixture".to_owned(),
                canonical_ref: "runtime:fixture".to_owned(),
                source_space: Space::Bundle,
                trust_class: Trust::TrustedBundle,
                signer_fingerprint: Some(fingerprint.to_owned()),
                content_hash: "b".repeat(64),
                raw_content_digest: "c".repeat(64),
            },
            definition_owner_chain_index: 1,
            authored_command: "bin:runner".to_owned(),
            expanded_command: "bin:runner".to_owned(),
        },
        selected_item_ref: item_ref,
        target_triple: triple.to_owned(),
        item_source_hash,
        expected_mode: 0o755,
        signed_executor_manifest_ref: signed_manifest_ref.into_bytes(),
        executor_manifest_object: manifest,
        item_source_object: item_source,
        signed_item_source_sidecar: signed_sidecar.into_bytes(),
    };
    let exact_program = json!({});
    let exact_program_hash =
        lillux::sha256_hex(lillux::canonical_json(&exact_program).unwrap().as_bytes());
    let sealed_invocation = json!({});
    let plan = json!({});
    let execution_plan_hash = lillux::sha256_hex(lillux::canonical_json(&plan).unwrap().as_bytes());
    let protocol_document = String::new();
    let protocol_hash = lillux::sha256_hex(protocol_document.as_bytes());

    ryeos_state::objects::AdmittedLaunchCapsule {
        schema: ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
        kind: "admitted_launch_capsule".to_owned(),
        exact_program,
        exact_program_hash,
        sealed_invocation,
        project_authority: ExecutionProjectAuthority::PROJECTLESS,
        lifecycle_authority: ExecutionLifecycleAuthority {
            ownership: ExecutionOwnershipAuthority::DaemonOwned,
            recovery: ExecutionRecoveryAuthority::RestartRecoverable,
        },
        launch_driver: ExecutionLaunchDriver::DirectItemExecutor,
        artifact_identity: Artifact::DirectItemExecutor {
            executor_ref: "tool:test/executor".to_owned(),
            root_subject_source_content_digest: "d".repeat(64),
            root_subject_signer_fingerprint: Some(fingerprint.to_owned()),
            root_subject_source_identity: DirectRootSourceIdentity::Project,
            protocol_ref: "protocol:test".to_owned(),
            protocol_content_hash: protocol_hash,
            protocol_signer_fingerprint: fingerprint.to_owned(),
            execution_plan_hash,
            executable_identity: Executable::BundleExecutor {
                content_hash: "f".repeat(64),
                executor_manifest_hash: manifest_hash,
                executor_manifest_signer_fingerprint: fingerprint.to_owned(),
                member_source_proof: proof,
            },
            runtime_identity: DirectRuntimeIdentity {
                runtime_ref: "runtime:fixture".to_owned(),
                runtime_source_space: DirectRuntimeSourceSpace::Bundle,
                runtime_content_hash: "e".repeat(64),
                runtime_signer_fingerprint: fingerprint.to_owned(),
                runtime_bundle_manifest_hash: Some("1".repeat(64)),
                runtime_bundle_signer_fingerprint: Some(fingerprint.to_owned()),
            },
        },
        execution_closure: Closure::DirectItemExecutor {
            execution_plan: plan,
            protocol_descriptor_document: protocol_document,
            command: Command::NodePolicy,
            admitted_project_root: None,
        },
        execution_realization_hash: "2".repeat(64),
        source_binding_hash: None,
        accounting_scope: None,
        effective_caps: Vec::new(),
        parent_delegation_caps: None,
        runtime_ref: "runtime:fixture".to_owned(),
        executor_ref: "tool:test/executor".to_owned(),
    }
}

pub(super) fn bundle_admission_trust_for(
    keys: &[&lillux::crypto::SigningKey],
) -> BundleAdmissionTrust {
    BundleAdmissionTrust::bound(ryeos_engine::trust::TrustStore::from_signers(
        keys.iter()
            .map(|key| ryeos_engine::trust::TrustedSigner {
                fingerprint: lillux::crypto::fingerprint(&key.verifying_key()),
                verifying_key: key.verifying_key(),
                label: None,
            })
            .collect(),
    ))
}

pub(super) fn remote_follow_delivery_request()
-> crate::federated_follow::RemoteFollowTerminalDeliveryRequest {
    crate::federated_follow::RemoteFollowTerminalDeliveryRequest {
        operation_id: "1".repeat(64),
        reservation_attestation_hash: "2".repeat(64),
        terminal_attestation_hash: "3".repeat(64),
        child_chain_root_id: "T-child".to_owned(),
        target_chain_head_hash: "4".repeat(64),
        parent_site_id: "site:parent".to_owned(),
        target_site_id: "site:target".to_owned(),
    }
}

pub(super) fn remote_follow_delivery_job(
    state: ryeos_state::SyncJobState,
    phase: &str,
    result: Option<Value>,
) -> ryeos_state::SyncJobRecord {
    ryeos_state::SyncJobRecord {
        job_id: format!("remote-follow-terminal-target:{}", "1".repeat(64)),
        operation_type: crate::federated_follow::REMOTE_FOLLOW_DELIVERY_OPERATION.to_owned(),
        operation: json!({
            "operation_type":crate::federated_follow::REMOTE_FOLLOW_DELIVERY_OPERATION
        }),
        peer: Some("site:parent".to_owned()),
        state,
        phase: phase.to_owned(),
        roots: vec!["2".repeat(64), "3".repeat(64), "4".repeat(64)],
        heads: vec!["4".repeat(64)],
        uploaded_hashes: Vec::new(),
        fetched_hashes: Vec::new(),
        attempt_count: 0,
        max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
        last_error: None,
        result,
        created_at: "2026-08-29T00:00:00Z".to_owned(),
        updated_at: "2026-08-29T00:00:00Z".to_owned(),
        finished_at: None,
    }
}

pub(super) fn test_store() -> TestStateStore {
    let tmp = tempdir().expect("tempdir").keep();
    let identity = ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
        .expect("test node identity");
    let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    TestStateStore::open(&tmp, signer, Arc::new(head_trust)).expect("state store")
}

// Row-only fixtures; no selected issuer, native contact or settlement proof.
pub(super) fn selected_fence_fixture(
    phase: crate::selected_invocation_bookkeeping::SelectedFixturePhase,
    refused: bool,
) -> (TestStateStore, runtime_db::LaunchClaim) {
    let store = test_store();
    let thread = "T-selected-fence";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
        .unwrap()
        .unwrap();
    let borrower = store.fixture_selected_restored_metadata(thread).unwrap();
    {
        let _g = store.lock().unwrap();
        store
            .selected_invocations
            .lock()
            .unwrap()
            .fixture_retained_phase(borrower.invocation(), phase, refused);
    }
    (store, claim)
}

pub(super) fn fenced_kind_chain(store: &StateStore, thread: &str) {
    let mut record = thread_record(thread, thread);
    record.kind = "fenced_test_kind".to_string();
    record.captured_history_policy.as_mut().unwrap().retention =
        ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
    store.create_thread_for_test(&record).unwrap();
    finish_workspace_test_thread(store, thread);
}

pub(super) fn fenced_test_kinds() -> ryeos_state::FencedChainKinds {
    ryeos_state::FencedChainKinds::new(["fenced_test_kind".to_string()]).unwrap()
}

// Signed persistence fixture only: no RootAdmission, compiler token, or
// claim that the resolver/launcher admitted these storage inputs. Birth,
// substrate attestation, capsule verification and finalization are real.
pub(super) fn signed_direct_storage_root(store: &StateStore, thread_id: &str, external: bool) {
    use ryeos_state::objects::*;
    let cas = store.state_authority.cas_store().unwrap();
    let command_hash = cas.store_blob(b"storage-only executable bytes").unwrap();
    let project = ExecutionProjectAuthority::pinned(
        "project:direct-storage".into(),
        None,
        "b".repeat(64),
        PinnedProjectRealization::ReadOnly,
        EnvironmentAuthority::None,
        Vec::new(),
    )
    .unwrap();
    let mut metadata =
        external_fixture_launch_metadata(project.clone(), json!({"binding_records":{}}), json!({}));
    let root_history = metadata
        .sealed_root_request
        .as_ref()
        .unwrap()
        .captured_history_policy()
        .clone();
    let AdmittedLaunchArtifactIdentity::ManagedRuntime {
        runtime_ref,
        runtime_content_hash,
        runtime_signer_fingerprint,
        protocol_ref,
        protocol_content_hash,
        protocol_signer_fingerprint,
        executor_ref,
        ..
    } = metadata.admitted_artifact_identity.take().unwrap()
    else {
        unreachable!()
    };
    let AdmittedExecutionClosure::ManagedRuntime {
        protocol_descriptor_document,
        ..
    } = metadata.admitted_execution_closure.take().unwrap()
    else {
        unreachable!()
    };
    let runtime = DirectRuntimeIdentity {
        runtime_ref,
        runtime_content_hash,
        runtime_signer_fingerprint,
        runtime_source_space: DirectRuntimeSourceSpace::Project,
        runtime_bundle_manifest_hash: None,
        runtime_bundle_signer_fingerprint: None,
    };
    let command_path = admitted_direct_command_execution_path(
        &command_hash,
        std::path::Path::new("/storage-command"),
    )
    .unwrap();
    let plan: ryeos_engine::contracts::ExecutionPlan = serde_json::from_value(json!({
        "plan_id":"signed-storage-direct", "root_executor_id":executor_ref,
        "root_ref":"graph:test/storage-fixture", "item_kind":"graph",
        "nodes":[{"node_type":"dispatch_subprocess","id":"spawn","spec":{
            "cmd":command_path,"verified_command":if external {json!({"authority":"captured_content",
                "code":{"source_path":command_path,"content_hash":command_hash}})} else {Value::Null},
            "args":[],"cwd":"/workspace","env":{},"stdin":null,"timeout_secs":30
        },"executor_chain":["graph:test/storage-fixture",executor_ref]}],
        "entrypoint":"spawn", "capabilities":{"requires_model":false,
            "requires_subprocess":true,"requires_network":false,"custom":[]},
        "materialization_requirements":[],"network_authority_ceiling":"node_policy",
        "filesystem_authority_ceiling":"node_policy","target_requirement":null,
        "endpoint_requirement":if external {json!({"kind":"external",
            "binding_id":"storage-direct","stdout_max_bytes":4096,"stderr_max_bytes":4096})}
            else {json!({"kind":"local"})},
        "external_endpoint_binding":if external {json!({"binding_id":"storage-direct",
            "binding_digest":"c".repeat(64)})} else {Value::Null},
        "subprocess_limits":null, "resource_authority_ceiling":"node_policy","cache_key":"storage",
        "executor_chain":["graph:test/storage-fixture",executor_ref],
        "executor_authorities":[],"runtime_identity":runtime
    }))
    .unwrap();
    plan.validate_endpoint_for_sealing().unwrap();
    let execution_plan = serde_json::to_value(plan).unwrap();
    metadata.launch_driver = Some(ExecutionLaunchDriver::DirectItemExecutor);
    if !external {
        metadata
            .resume_context
            .as_mut()
            .unwrap()
            .lifecycle_authority = ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE;
    }
    metadata.admitted_artifact_identity =
        Some(AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executor_ref: executor_ref.clone(),
            root_subject_source_content_digest: root_history.item_content_hash.clone(),
            root_subject_signer_fingerprint: root_history.item_signer_fingerprint.clone(),
            root_subject_source_identity: DirectRootSourceIdentity::Project,
            protocol_ref,
            protocol_content_hash,
            protocol_signer_fingerprint,
            execution_plan_hash: lillux::sha256_hex(
                lillux::canonical_json(&execution_plan).unwrap().as_bytes(),
            ),
            executable_identity: if external {
                DirectExecutableIdentity::CapturedContent {
                    content_hash: command_hash.clone(),
                }
            } else {
                DirectExecutableIdentity::NodePolicy
            },
            runtime_identity: runtime.clone(),
        });
    metadata.admitted_execution_closure = Some(AdmittedExecutionClosure::DirectItemExecutor {
        execution_plan,
        protocol_descriptor_document,
        admitted_project_root: None,
        command: if external {
            AdmittedDirectCommandClosure::ContentAddressed {
                executable_blob_hash: command_hash,
                execution_path: command_path,
                host_origin: None,
            }
        } else {
            AdmittedDirectCommandClosure::NodePolicy
        },
    });
    let provisional = metadata.admitted_launch_capsule().unwrap().unwrap();
    let authority = provisional.launch_authority();
    let cas_root = store.cas_root().unwrap();
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(
        &cas_root.ancestors().nth(3).unwrap().join("node-key.pem"),
    )
    .unwrap();
    let substrate = crate::execution_identity_probe::boot_node_execution_identity(
        store,
        &identity,
        &crate::build_info::get(),
    )
    .unwrap();
    let realization = AdmittedExecutionRealization {
        schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
        kind: ADMITTED_EXECUTION_REALIZATION_KIND.into(),
        substrate_identity_hash: substrate.identity_hash.clone(),
        substrate_attestation_hash: substrate.attestation_hash.clone(),
        launch_authority_digest: authority.digest().unwrap(),
        effective_definition_digest: provisional.exact_program["effective_definition_digest"]
            .as_str()
            .unwrap()
            .into(),
        artifact_identity_digest: authority.artifact_identity_digest().unwrap(),
        execution_closure_digest: authority.execution_closure_digest().unwrap(),
        contract_ref: runtime.runtime_ref,
        contract_digest: runtime.runtime_content_hash,
        components: Vec::new(),
        properties: BTreeMap::new(),
    };
    metadata.execution_realization_hash =
        Some(cas.store_object(&realization.to_value().unwrap()).unwrap());
    let mut thread = thread_record(thread_id, thread_id);
    let resume = metadata.resume_context.as_ref().unwrap();
    thread.kind = resume.kind.clone();
    thread.item_ref = resume.item_ref.clone();
    thread.captured_history_policy = Some(root_history);
    thread.launch_mode = resume.launch_mode.clone();
    thread.executor_ref = executor_ref;
    thread.project_authority = project;
    thread.base_project_snapshot_hash = Some("b".repeat(64));
    store
        .create_root_thread_for_test_with_launch_metadata(&thread, &metadata)
        .unwrap();
    let retained = store.admitted_launch_capsule(thread_id).unwrap().unwrap();
    assert_eq!(
        crate::thread_lifecycle::capsule_requires_external_direct(&retained).unwrap(),
        external,
    );
}

pub(super) fn storage_direct_success() -> FinalizeThreadRecord {
    FinalizeThreadRecord {
        status: "completed".into(),
        outcome_code: Some("exit:0".into()),
        result_json: Some(json!({"answer":42})),
        error_json: None,
        artifacts: Vec::new(),
        final_cost: None,
        managed_envelope: None,
        result_project_snapshot_hash: None,
        result_workspace_output_capture_hash: None,
    }
}

pub(super) fn direct_owner_test_reservation(
    store: &StateStore,
) -> runtime_db::external_execution::ExternalAllocationReservation {
    use runtime_db::external_execution::{ExternalAllocationOwner, ExternalAllocationReservation};
    let thread = "T-direct-owner-proof";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let owner = store
        .claim_thread_launch_active(thread, "direct-proof-claim", "daemon:direct-proof")
        .unwrap()
        .unwrap()
        .owner;
    let binding=crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
    let now = lillux::time::timestamp_millis();
    ExternalAllocationReservation {
        schema: runtime_db::external_execution::EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
        placement_thread_id: thread.into(),
        admitted_capsule_hash: "a".repeat(64),
        owner: ExternalAllocationOwner::DirectThread {
            chain_root_id: thread.into(),
            launch_owner: owner,
            program: crate::thread_lifecycle::external_direct_program_test_fixture(),
        },
        base_snapshot_hash: "b".repeat(64),
        binding_hash: binding.digest().into(),
        capacity_owner: binding.capacity_owner().into(),
        channel_authority_generation: "3".repeat(64),
        channel_owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
            &lillux::crypto::SigningKey::from_bytes(&[19; 32]).verifying_key(),
        )
        .unwrap(),
        channel_bootstrap_capability_hash: "4".repeat(64),
        admitted_adapter_request_base64: None,
        request_digest: "e".repeat(64),
        max_active: 1,
        timeout_seconds: 60,
        contact_deadline_ms: now + 30_000,
        startup_started_at_ms: now,
        startup_deadline_ms: now + 60_000,
    }
}

pub(super) fn workspace_binding_fixture() -> (TestStateStore, runtime_db::RuntimeWorkspaceBinding) {
    let store = test_store();
    let root = "T-workspace-root";
    store
        .create_thread_for_test(&thread_record(root, root))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(root, "claim-root", "daemon:workspace-test")
        .unwrap()
        .unwrap();
    let binding = runtime_db::RuntimeWorkspaceBinding {
        workspace_id: "workspace-parent".to_owned(),
        view_identity: "exact-created-view-1".to_owned(),
        borrower_launch_owner: claim.owner,
    };
    {
        let g = store.lock().unwrap();
        g.runtime_db
            .reserve_workspace(&binding.workspace_id, &"a".repeat(64), "/workspace-fixture")
            .unwrap();
        g.runtime_db
            .transition_workspace(
                &binding.workspace_id,
                &[runtime_db::WorkspaceState::Reserved],
                runtime_db::WorkspaceState::Constructing,
                None,
            )
            .unwrap();
        g.runtime_db
            .claim_workspace_construction(&binding.workspace_id, root, &claim.claimed_by)
            .unwrap();
        g.runtime_db
            .bind_workspace(runtime_db::WorkspaceBinding {
                workspace_id: &binding.workspace_id,
                thread_id: root,
                launch_owner: Some(&claim.claimed_by),
                backend_id: Some("native"),
                backend_version: Some("fixture-build"),
                pinned_root_identities: Some("fixture-pinned-roots"),
                mount_identity: Some(&binding.view_identity),
                workspace_output_partition_identity: None,
                base_output_capture_hash: None,
            })
            .unwrap();
    }
    store.bind_thread_workspace(root, &binding).unwrap();
    // A replay while still Ready is exact, not another membership.
    store.bind_thread_workspace(root, &binding).unwrap();
    store
        .transition_execution_workspace(
            &binding.workspace_id,
            &[runtime_db::WorkspaceState::Ready],
            runtime_db::WorkspaceState::Active,
            None,
        )
        .unwrap();
    (store, binding)
}

pub(super) fn workspace_child_fixture(
    store: &StateStore,
    parent: &str,
    child: &str,
    root_binding: &runtime_db::RuntimeWorkspaceBinding,
) -> runtime_db::RuntimeWorkspaceBinding {
    // Deliberately separate chain roots: membership is not chain equality.
    store
        .create_thread_for_test(&thread_record(child, child))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(child, &format!("claim-{child}"), "daemon:workspace-test")
        .unwrap()
        .unwrap();
    assert!(
        store
            .record_child_link(parent, child, "dispatch")
            .unwrap()
            .is_none()
    );
    runtime_db::RuntimeWorkspaceBinding {
        borrower_launch_owner: claim.owner,
        ..root_binding.clone()
    }
}

pub(super) fn finish_workspace_test_thread(store: &StateStore, thread_id: &str) {
    store
        .finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: ThreadStatus::Completed.as_str().to_owned(),
                outcome_code: Some("completed".to_owned()),
                result_json: None,
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();
}

pub(super) fn attach_workspace_test_process(
    store: &StateStore,
    binding: &runtime_db::RuntimeWorkspaceBinding,
) -> crate::process::ExecutionProcessIdentity {
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
    let thread_id = &binding.borrower_launch_owner.thread_id;
    let owner = store
        .get_launch_claim(thread_id)
        .unwrap()
        .unwrap()
        .claimed_by;
    store
        .attach_new_thread_process(
            thread_id,
            identity.target_pid,
            identity.group_leader_pid,
            &identity,
            &crate::launch_metadata::RuntimeLaunchMetadata::default(),
            Some(&owner),
        )
        .unwrap();
    identity
}

pub(super) fn reserve_workspace_test_barrier(
    store: &StateStore,
    access: ryeos_engine::kind_registry::WorkspaceAccess,
    child: &str,
) -> String {
    let invocation = ryeos_runtime::workload_client::WorkloadInvocationSource::Cli {
        external_request_id: "fixture-request".to_owned(),
    };
    let operation_id = invocation.runtime_operation_id(&"c".repeat(64)).unwrap();
    let g = store.lock().unwrap();
    g.runtime_db
        .reserve_runtime_action_intent_with_workspace(
            &operation_id,
            "T-workspace-root",
            "T-workspace-root",
            runtime_db::RuntimeActionMode::Inline,
            &"e".repeat(64),
            child,
            None,
            Some(&runtime_db::NewRuntimeWorkspaceOperation {
                workspace_id: "workspace-parent",
                access,
                worker_instance_id: "test-worker",
                worker_boot_epoch: 1,
                worker_boot_identity_hash: &"a".repeat(64),
                project_authority_digest: &"b".repeat(64),
                workload_client_grant_digest: &"c".repeat(64),
                invocation,
            }),
        )
        .unwrap();
    operation_id
}

pub(super) fn handoff_test_operation(
    role: crate::worker_handoff::WorkerHandoffJobRole,
    operation_label: &str,
    placement_thread_id: &str,
) -> crate::worker_handoff::WorkerSessionHandoffJobOperation {
    let hash = |label: &str| lillux::sha256_hex(label.as_bytes());
    crate::worker_handoff::WorkerSessionHandoffJobOperation::new(
        role,
        hash(operation_label),
        hash(&format!("{operation_label}:preflight")),
        hash(&format!("{operation_label}:preflight-attestation")),
        "fp:handoff-owner".to_owned(),
        format!("T-chain-{operation_label}"),
        "site:origin".to_owned(),
        "site:source".to_owned(),
        "site:target".to_owned(),
        placement_thread_id.to_owned(),
        format!("T-successor-{operation_label}"),
        hash(&format!("{operation_label}:source-head")),
        hash(&format!("{operation_label}:source-event")),
        hash(&format!("{operation_label}:checkpoint")),
        hash(&format!("{operation_label}:transfer")),
        "source".to_owned(),
        "/source/project".to_owned(),
        "/target/project".to_owned(),
        hash(&format!("{operation_label}:route")),
        "credential:target".to_owned(),
        None,
    )
    .unwrap()
}

pub(super) fn target_handoff_progress(
    operation_id: &str,
    phase: crate::worker_handoff::WorkerHandoffPhase,
) -> crate::worker_handoff::WorkerSessionHandoffProgress {
    let mut progress =
        crate::worker_handoff::WorkerSessionHandoffProgress::planned(operation_id.to_owned())
            .unwrap();
    progress.phase = phase;
    if !matches!(
        phase,
        crate::worker_handoff::WorkerHandoffPhase::Planned
            | crate::worker_handoff::WorkerHandoffPhase::SourceExported
            | crate::worker_handoff::WorkerHandoffPhase::AbortAuthorized
    ) {
        progress.placement_attestation_hash = Some("a".repeat(64));
        progress.target_runtime_seed_hash = Some("b".repeat(64));
        progress.credential_reservation_id = Some("reservation:target".to_owned());
    }
    if phase.successor_is_only_authorized_writer() {
        progress.writer_grant_hash = Some("c".repeat(64));
        progress.target_chain_head_hash = Some("d".repeat(64));
    }
    progress.validate().unwrap();
    progress
}

pub(super) fn target_adoption_coordinates(
    operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
) -> (
    crate::worker_handoff::WorkerPlacementAdoptRequest,
    crate::worker_handoff::WorkerPlacementAdoptResponse,
) {
    let request = crate::worker_handoff::WorkerPlacementAdoptRequest {
        operation_id: operation.operation_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
        target_chain_head_hash: "d".repeat(64),
        placement_attestation_hash: "a".repeat(64),
        writer_grant_hash: "c".repeat(64),
    };
    let response = crate::worker_handoff::WorkerPlacementAdoptResponse {
        operation_id: operation.operation_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
        placement_thread_id: operation.successor_placement_thread_id.clone(),
        target_chain_head_hash: request.target_chain_head_hash.clone(),
        delivery: "attached".to_owned(),
    };
    (request, response)
}

pub(super) fn seed_projected_target_adoption_job(
    store: &StateStore,
    operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
) -> String {
    let progress = target_handoff_progress(
        &operation.operation_id,
        crate::worker_handoff::WorkerHandoffPhase::ProcessAttached,
    );
    let job_id = format!("worker-handoff-target:{}", operation.operation_id);
    store
        .with_state_db(|db| {
            db.create_sync_job_with_initial_progress(
                &ryeos_state::NewSyncJob {
                    job_id: job_id.clone(),
                    operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
                        .to_owned(),
                    operation: operation.to_value()?,
                    peer: Some("source".to_owned()),
                    roots: vec![
                        operation.transfer_manifest_hash.clone(),
                        "a".repeat(64),
                        "b".repeat(64),
                        "c".repeat(64),
                        "d".repeat(64),
                    ],
                    // The target sync head remains the source head until
                    // the permanent adoption receipt is folded terminally.
                    heads: vec![operation.source_chain_head_hash.clone()],
                    max_attempts: crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
                },
                ryeos_state::SyncJobState::Running,
                progress.phase.as_str(),
                Some(&progress.to_value()?),
            )
        })
        .unwrap();
    job_id
}

pub(super) fn positioned_test_event(
    thread_id: &str,
    chain_seq: u64,
    thread_seq: u64,
    prev_chain_event_hash: Option<String>,
    prev_thread_event_hash: Option<String>,
) -> ryeos_state::objects::ThreadEvent {
    ryeos_state::objects::ThreadEvent {
        schema: ryeos_state::objects::SCHEMA_VERSION,
        kind: "thread_event".to_string(),
        chain_root_id: "T-root".to_string(),
        chain_seq,
        thread_id: thread_id.to_string(),
        thread_seq,
        event_type: "test_event".to_string(),
        durability: ryeos_state::objects::EventDurability::Durable,
        ts: "2026-07-24T00:00:00.000Z".to_string(),
        prev_chain_event_hash,
        prev_thread_event_hash,
        payload: json!({}),
    }
}

pub(super) fn follow_waiter_for_admission() -> runtime_db::FollowWaiter {
    runtime_db::FollowWaiter {
        follow_key: "follow-key".to_string(),
        parent_thread_id: "T-parent".to_string(),
        parent_chain_root_id: "T-parent".to_string(),
        parent_successor_thread_id: Some("T-successor".to_string()),
        follow_node: "fanout".to_string(),
        graph_run_id: "run-1".to_string(),
        step_count: 1,
        frontier_id: None,
        fanout: true,
        expected_children: 2,
        child_project_authority: None,
        children: vec![runtime_db::FollowWaiterChild {
            item_index: 0,
            item_ref: "tool:test/one".to_string(),
            spec_hash: "spec".to_string(),
            child_thread_id: "T-child".to_string(),
            child_chain_root_id: "T-child".to_string(),
            sealed_root_request:
                crate::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture(),
            terminal_thread_id: None,
            terminal_status: None,
            terminal_envelope: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }],
        phase: runtime_db::follow_phase::RESERVED.to_string(),
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

pub(super) fn thread_record(thread_id: &str, chain_root_id: &str) -> NewThreadRecord {
    let captured_history_policy = (thread_id == chain_root_id).then(|| {
        let hash = "a".repeat(64);
        ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
            canonical_item_ref: "directive:test".to_string(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(
                ),
            },
        }
    });
    NewThreadRecord {
        thread_id: thread_id.to_string(),
        chain_root_id: chain_root_id.to_string(),
        kind: "directive".to_string(),
        item_ref: "directive:test".to_string(),
        executor_ref: "native:test".to_string(),
        launch_mode: "wait".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some("fp:test".to_string()),
        project_root: None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy,
    }
}

pub(super) fn launch_attempt_audit_events() -> Vec<NewEventRecord> {
    LAUNCH_ATTEMPT_AUDIT_TYPES
        .into_iter()
        .enumerate()
        .map(|(index, event_type)| NewEventRecord {
            event_type: event_type.as_str().to_string(),
            storage_class: event_type.storage_class().as_str().to_string(),
            payload: json!({"fixture": index}),
        })
        .collect()
}

pub(super) fn replayed_event_types(store: &StateStore, thread_id: &str) -> Vec<String> {
    store
        .replay_events(thread_id, Some(thread_id), None, 32, 1024 * 1024)
        .expect("replay thread events")
        .events
        .into_iter()
        .map(|event| event.event_type)
        .collect()
}

pub(super) fn running_in_process_test_root(store: &StateStore, thread_id: &str) {
    let metadata = in_process_launch_metadata();
    let owner = store
        .register_in_process_handler(thread_id)
        .expect("register hook evidence owner");
    store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            vec![NewEventRecord {
                event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            }],
            &metadata,
            &owner,
        )
        .expect("create running hook evidence root");
}

pub(super) fn hook_observation_identity(
    hook_id: &str,
) -> ryeos_runtime::callback::HookDispatchIdentity {
    ryeos_runtime::callback::HookDispatchIdentity {
        occurrence: ryeos_runtime::callback::HookDispatchOccurrence::new(
            "graph",
            "graph_step_completed",
            "graph:test/hook-evidence",
            "r".repeat(64),
            "d".repeat(64),
        )
        .with_text_coordinate("graph_run_id", "G-hook-evidence")
        .with_counter_coordinate("step", 3)
        .with_text_coordinate("node", "observe"),
        hook_id: hook_id.to_string(),
        layer: ryeos_runtime::hooks_loader::HookLayer::Infrastructure,
        result_mode: ryeos_runtime::hooks_loader::HookResultMode::Observation,
        context_contract: ryeos_engine::hooks::HookContextContract {
            schema: ryeos_engine::hooks::HOOK_CONTEXT_SCHEMA.to_string(),
            allowed_roots: std::collections::BTreeSet::from(["state".to_string()]),
        },
        context_hash: "c".repeat(64),
    }
}

pub(super) fn hook_test_dispatch_evidence()
-> ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
    ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
        source: ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed,
        effect_class: ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live,
        action_digest: "7".repeat(64),
        effect_identity: None,
        publication: ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable,
        record_hash: None,
        replayed_from: None,
        result_projection: ryeos_effect_contract::DispatchResultProjection::DispatchedSubject,
    }
}

pub(super) fn in_process_launch_metadata() -> crate::launch_metadata::RuntimeLaunchMetadata {
    crate::launch_metadata::RuntimeLaunchMetadata::default()
        .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        .with_in_process_lifecycle_authority(
            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
        )
}

pub(super) fn process_resource_reservation_fixture(
    coordinate: &str,
) -> (
    runtime_db::ProcessResourceReservationRecord,
    lillux::ProcessScopeRecovery,
) {
    let host =
        serde_json::to_value(lillux::ProcessHostLifetime::capture_current().unwrap()).unwrap();
    let planned = serde_json::json!({
        "version": 3,
        "control_timeout": {"secs": 1, "nanos": 0},
        "configuration": {"version": 3, "backend": {
            "implementation": "linux_cgroup_v2", "parent": "/fixture/delegation"
        }},
        "backend": {"implementation": "linux_cgroup_v2",
            "boot_id": host["backend"]["boot_id"],
            "cgroup_enclosure": host["backend"]["cgroup_enclosure"],
            "parent": {"filesystem": {"device": 1}, "inode": 2},
            "name": format!("resource-{coordinate}")}
    });
    let allocation: lillux::ProcessScopeAllocation =
        serde_json::from_value(planned.clone()).unwrap();
    let mut bound = planned;
    bound["version"] = 5.into();
    bound["backend"]["directory"] = serde_json::json!({"filesystem": {"device": 1}, "inode": 3});
    let recovery: lillux::ProcessScopeRecovery = serde_json::from_value(bound).unwrap();
    (
        runtime_db::ProcessResourceReservationRecord {
            owner_kind: "thread".to_owned(),
            owner_coordinate: coordinate.to_owned(),
            daemon_generation_id: runtime_db::daemon_generation_id().to_owned(),
            selections: vec![ryeos_engine::contracts::ExecutionResourceSelection {
                stable_id: format!("accelerator-{coordinate}"),
                class: "accelerator".to_owned(),
                matched_facts: BTreeMap::new(),
                observation_contract_digest: "1".repeat(64),
                device_binding_digest: "2".repeat(64),
                access: ryeos_engine::contracts::ExecutionResourceAccess::DeploymentVisible,
                enforcement:
                    ryeos_engine::contracts::ExecutionResourceEnforcement::DeploymentVisible,
                character_devices: Vec::new(),
            }],
            allocation_limit: 1,
            runtime_custody: None,
            launch_authority:
                crate::runtime_db::ProcessResourceLaunchAuthority::LocalProcessScope {
                    allocation,
                    recovery: None,
                },
        },
        recovery,
    )
}

/// Storage-only trusted coordinates; no admitted capsule, process or GPU.
#[cfg(target_os = "linux")]
pub(super) fn scratch_reservation_fixture(
    store: &StateStore,
    coordinate: &str,
) -> (
    Arc<crate::temp_dir_guard::TempDirGuard>,
    runtime_db::ProcessResourceReservationRecord,
) {
    use runtime_db::process_resource_custody::*;
    let runtime = store.state_authority.runtime_directory();
    let (_, original) = crate::temp_dir_guard::create_projectless_workspace(
        &runtime.path().join("cache"),
        coordinate,
    )
    .unwrap();
    std::fs::write(
        original.path().unwrap().join("sentinel"),
        b"retained original scratch",
    )
    .unwrap();
    let (parent, name) = original.owned_scratch_parent().unwrap();
    let (mut reservation, _) = process_resource_reservation_fixture(coordinate);
    reservation.owner_kind = "pooled_session".to_owned();
    reservation.launch_authority =
        runtime_db::ProcessResourceLaunchAuthority::TrustedProcessGroup {
            cleanup_contract_digest: "7".repeat(64),
            host_lifetime: lillux::ProcessHostLifetime::capture_current().unwrap(),
            phase: runtime_db::TrustedResourceLaunchPhase::Reserved,
        };
    reservation.runtime_custody = Some(ProcessResourceRuntimeCustody {
        version: ProcessResourceRuntimeCustody::VERSION,
        session_capsule_hash: "1".repeat(64),
        execution_realization_hash: "2".repeat(64),
        source_binding_hash: Some("3".repeat(64)),
        materializations: vec![ProcessMaterializationCustody {
            cache: ProcessMaterializationCache::ExternalContent,
            manifest_hash: "4".repeat(64),
        }],
        workspace: ProcessWorkspaceCustody::ResourceOwnedScratch {
            runtime_directory_identity: runtime.identity().unwrap(),
            parent_directory_identity: parent.identity().unwrap(),
            scratch_name: name.to_str().unwrap().to_owned(),
            workspace_identity: original.owned_scratch_root().unwrap().identity().unwrap(),
        },
        physical_state: ProcessCustodyPhysicalState::Retained,
    });
    (original, reservation)
}

pub(super) fn continuation_resume_context(
    project_context: ProjectContext,
) -> crate::launch_metadata::ResumeContext {
    let stable_project_identity = match &project_context {
        ProjectContext::LocalPath { path } => Some(
            crate::launch_metadata::StableProjectIdentity::from_path(path, "site:test").unwrap(),
        ),
        _ => None,
    };
    let local_overlay_root = Some(PathBuf::from("/work/project"));
    let original_snapshot_hash = Some("a".repeat(64));
    let project_root = PathBuf::from("/work/project");
    let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
        format!("local:{}", project_root.display()),
        Some(project_root),
        original_snapshot_hash.clone().unwrap(),
        ryeos_state::objects::PinnedProjectRealization::Cow {
            terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
        },
        ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
            project_authority_id: lillux::sha256_hex(
                b"live-project\0local:/work/project\0/work/project",
            ),
            source_identity: "dotenv:/work/project/.env".to_string(),
            include_operator_vault: true,
            name_authority: ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
        },
        Vec::new(),
    )
    .unwrap();
    crate::launch_metadata::ResumeContext {
        kind: "directive".to_string(),
        item_ref: "directive:test".to_string(),
        ref_bindings: std::collections::BTreeMap::new(),
        product_selections: Vec::new(),
        launch_mode: "wait".to_string(),
        parameters: json!({}),
        project_context,
        project_authority,
        lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
        stable_project_identity,
        local_overlay_root,
        original_snapshot_hash,
        original_pushed_head_ref: None,
        state_root: None,
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test".to_string(),
            scopes: vec!["execute".to_string()],
        }),
        execution_hints: ExecutionHints::default(),
        scheduled_fire: None,
        effective_caps: Vec::new(),
        parent_delegation_caps: None,
        executor_ref: Some("native:test".to_string()),
        runtime_ref: None,
    }
}

pub(super) fn in_process_child_birth_events() -> Vec<NewEventRecord> {
    vec![NewEventRecord {
        event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
        storage_class: "indexed".to_string(),
        payload: json!({}),
    }]
}

pub(super) fn in_process_child_record(child_id: &str, parent_id: &str) -> NewThreadRecord {
    let mut record = thread_record(child_id, parent_id);
    record.upstream_thread_id = Some(parent_id.to_string());
    record
}

pub(super) fn in_process_reservation_phase(
    store: &StateStore,
    thread_id: &str,
) -> Option<runtime_db::InProcessHandlerReservationPhase> {
    store
        .in_process_handler_reservation(thread_id)
        .expect("read in-process reservation")
        .map(|reservation| reservation.phase)
}

pub(super) fn requested_failure() -> FinalizeThreadRecord {
    FinalizeThreadRecord {
        status: ThreadStatus::Failed.as_str().to_string(),
        outcome_code: Some(ThreadStatus::Failed.as_str().to_string()),
        result_json: None,
        error_json: Some(json!({"reason": "handler_failed"})),
        artifacts: Vec::new(),
        final_cost: None,
        managed_envelope: None,
        result_project_snapshot_hash: None,
        result_workspace_output_capture_hash: None,
    }
}

pub(super) fn birth_in_process_child(
    store: &StateStore,
    parent_id: &str,
    child_id: &str,
) -> InProcessHandlerControl {
    birth_unspawned_in_process_child(store, parent_id, child_id).0
}

pub(super) fn birth_unspawned_in_process_child(
    store: &StateStore,
    parent_id: &str,
    child_id: &str,
) -> (InProcessHandlerControl, UnspawnedInProcessChild) {
    let owner = store
        .register_in_process_handler(child_id)
        .expect("register in-process child owner");
    let (_, unspawned) = store
        .create_in_process_child_with_events_and_launch_metadata(
            &in_process_child_record(child_id, parent_id),
            in_process_child_birth_events(),
            &in_process_launch_metadata(),
            &owner,
        )
        .expect("publish in-process child");
    (owner, unspawned)
}
