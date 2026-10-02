//! Actual signed protocol/CAS/node-attestation/descriptor fixture for the
//! production read-only J1 factory. No raw proof constructor or native launch.
use super::*;
use crate::node_policy::sections::execution::*;
use ryeos_engine::{contracts::*, resolution::*};
use ryeos_state::objects::*;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

fn fixture() -> (tempfile::TempDir, crate::state::AppState, String) {
    let root = tempfile::tempdir().unwrap();
    let mut state = crate::state::test_support::build(root.path()).unwrap();
    let key = lillux::crypto::SigningKey::from_bytes(&[113; 32]);
    let fp = lillux::signature::compute_fingerprint(&key.verifying_key());
    let mut engine = (*state.engine).clone();
    engine.node_trust_store =
        ryeos_engine::trust::TrustStore::from_signers(vec![ryeos_engine::trust::TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: key.verifying_key(),
            label: None,
        }]);
    state.engine = Arc::new(engine);
    let cleanup = lillux::sha256_hex(b"test original product cleanup contract");
    let devices = vec![lillux::CharacterDeviceSpec {
        role: "primary".into(),
        source: "/dev/null".into(),
        destination: "/dev/null".into(),
        access: lillux::CharacterDeviceAccess::ReadWrite,
    }];
    let observed = lillux::CharacterDeviceSet::observe(&devices).unwrap();
    let facts = BTreeMap::new();
    let observation = lillux::resource_observation_contract_digest(
        "test-device",
        "test-character-device",
        &facts,
        observed.identities(),
    )
    .unwrap();
    let meter = ryeos_accounting::ResourceMeterContract {
        version: ryeos_accounting::RESOURCE_METER_CONTRACT_VERSION,
        kind: ryeos_accounting::ResourceMeterKind::OccupancyDuration,
        clock_contract_digest: ryeos_accounting::HexDigest::new(
            lillux::time::OccupancyClockContract::current()
                .unwrap()
                .contract_digest,
        )
        .unwrap(),
        contract_digest: ryeos_accounting::HexDigest::new("0".repeat(64)).unwrap(),
    }
    .sealed()
    .unwrap();
    let tariff = ryeos_accounting::ResourceTariffDocument {
        version: ryeos_accounting::RESOURCE_TARIFF_VERSION,
        currency: ryeos_accounting::Currency::Usd,
        pricing_generation: "factory-fixture".into(),
        charge_class: ryeos_accounting::ResourceChargeClass::InternalAllocation,
        rate_per_million_milliseconds: ryeos_accounting::UsdNanos::parse_canonical("1").unwrap(),
        billing_quantum_milliseconds: 1000,
        minimum_billable_milliseconds: 1000,
        expires_at_ms: None,
    };
    let authority = ryeos_accounting::ResourceAccountingAuthority {
        version: ryeos_accounting::RESOURCE_ACCOUNTING_AUTHORITY_VERSION,
        authority_digest: ryeos_accounting::HexDigest::new("0".repeat(64)).unwrap(),
        stable_resource_id: "test-device".into(),
        resource_class: "test-character-device".into(),
        observation_contract_digest: ryeos_accounting::HexDigest::new(observation.clone()).unwrap(),
        meter,
        spend: ryeos_accounting::ResourceSpendAuthority::Bounded {
            maximum_occupancy_milliseconds: 10000,
            maximum: tariff.charge_for_nanoseconds(10_000_000_000).unwrap(),
        },
        tariff,
    }
    .sealed()
    .unwrap();
    let resources = NodeExecutionResourcePolicy {
        admission: ExecutionResourceAdmissionPolicy {
            limits: ExecutionTargetLimits {
                max_requirements: 1,
                max_resource_count: 1,
                max_facts_per_requirement: 1,
            },
            max_total_resource_count: 1,
            max_concurrent_exclusive_allocations: 1,
            allowed_classes: vec!["test-character-device".into()],
            allowed_allocations: vec![ExecutionResourceAllocation::Exclusive],
            allowed_access: vec![ExecutionResourceAccess::ExecutionRestricted],
        },
        cleanup_allowance_ms: 1000,
        trusted_cleanup_contract_digest: Some(cleanup.clone()),
        resources: vec![NodeExecutionResourceDescriptor {
            stable_id: "test-device".into(),
            class: "test-character-device".into(),
            observation_contract_digest: observation,
            facts,
            character_devices: devices,
            accounting: Some(authority),
        }],
    };
    let policy = NodeExecutionAdmissionPolicy {
        schema: 4,
        max_live_fanout: 1,
        max_private_materialization_copy_bytes: 1024 * 1024,
        host_env_passthrough: vec![],
        workload_client: None,
        resource_authority: Some(resources.clone()),
        producer_resource_ceiling: None,
    };
    policy.validate().unwrap();
    state.node_policy = Arc::new(crate::node_policy::NodePolicySnapshot::from_test_records(
        vec![Arc::new(policy)],
    ));
    state.execution_resources = Arc::new(
        crate::execution_resources::ExecutionResourcePool::observe(Some(&resources)).unwrap(),
    );
    let target = ExecutionTargetRequirement {
        os: lillux::platform::current_target().os.into(),
        arch: lillux::platform::current_target().arch.into(),
        resources: vec![ExecutionResourceRequirement {
            class: "test-character-device".into(),
            count: 1,
            allocation: ExecutionResourceAllocation::Exclusive,
            access: ExecutionResourceAccess::ExecutionRestricted,
            exact_facts: Default::default(),
            minimum_facts: Default::default(),
        }],
    };
    let protocol_body = format!(
        r#"kind: protocol
name: factory_session
category: test
abi_version: v1
stdin: {{ shape: opaque }}
stdout: {{ shape: opaque_bytes, mode: terminal }}
env_injections: []
capabilities: {{ allows_pushed_head: false, allows_target_site: false, allows_detached: false }}
lifecycle: {{ mode: managed }}
callback_channel: none
session:
  process_mode: pooled_requests
  cleanup_authority: trusted_process_group
  resource_cleanup_contract: {cleanup}
  workspace_authority: ephemeral_scratch
  network_authority: isolated
  runtime_env_allowlist: []
  channel: inherited_unix_socket
  channel_env: RYEOS_SESSION_FD
  framing: u32_be_json
  wire_protocol: test.factory-session
  wire_version: 1
  max_frame_bytes: 4096
"#
    );
    let protocol_document = lillux::signature::sign_content(&protocol_body, &key, "#", None);
    let protocol_hash = lillux::signature::content_hash(&protocol_body);
    let subject_body = "kind: worker\nname: factory\ncategory: test\n";
    let subject_document = lillux::signature::sign_content(subject_body, &key, "#", None);
    let subject_hash = lillux::signature::content_hash(subject_body);
    let runtime_document = lillux::signature::sign_content(
        "kind: runtime\nname: factory\ncategory: test\n",
        &key,
        "#",
        None,
    );
    let runtime_hash =
        lillux::signature::content_hash("kind: runtime\nname: factory\ncategory: test\n");
    let resolution = ResolutionOutput {
        root: ResolvedAncestor {
            requested_id: "worker:test/factory".into(),
            resolved_ref: "worker:test/factory".into(),
            source_path: PathBuf::from("/fixture/worker.yaml"),
            source_space: ItemSpace::Bundle,
            source_root: ItemSourceRoot::Bundle {
                name: "factory-fixture".into(),
            },
            trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            signer_fingerprint: Some(fp.clone()),
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: subject_document.clone(),
            source_content_digest: subject_hash.clone(),
            raw_content_digest: lillux::sha256_hex(subject_document.as_bytes()),
        },
        ancestors: vec![],
        references_edges: vec![],
        referenced_items: vec![],
        step_outputs: Default::default(),
        effective_trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
        composed: KindComposedView::identity(serde_json::json!({"supported_target":target})),
    };
    let exact = PersistentSessionExactProgram {
        effective_definition_digest: resolution
            .effective_definition_digest()
            .unwrap()
            .as_str()
            .into(),
        resolution_output: RetainedResolutionOutput::capture(&resolution),
        evidence_attachments: vec![],
    };
    let pinned = state.state_store.pinned_state_authority().unwrap();
    let cas = pinned.cas_store().unwrap();
    // All fixture content identities are hashes of actual retained bytes.
    let executable_hash = cas
        .store_blob(b"fixture content only; never executed")
        .unwrap();
    let root_manifest = cas.store_blob(subject_document.as_bytes()).unwrap();
    let runtime_manifest = cas.store_blob(runtime_document.as_bytes()).unwrap();
    let path = admitted_direct_command_execution_path(
        &executable_hash,
        std::path::Path::new("factory-command"),
    )
    .unwrap();
    let runtime = DirectRuntimeIdentity {
        runtime_ref: "runtime:test/factory".into(),
        runtime_source_space: DirectRuntimeSourceSpace::Bundle,
        runtime_content_hash: runtime_hash,
        runtime_signer_fingerprint: fp.clone(),
        runtime_bundle_manifest_hash: Some(runtime_manifest),
        runtime_bundle_signer_fingerprint: Some(fp.clone()),
    };
    let plan:ExecutionPlan=serde_json::from_value(serde_json::json!({
        "plan_id":"factory-plan","root_executor_id":"native:factory","root_ref":"worker:test/factory","item_kind":"worker",
        "nodes":[{"node_type":"dispatch_subprocess","id":"spawn","spec":{"cmd":path,"verified_command":{"authority":"captured_content","code":{"source_path":path,"content_hash":executable_hash}},"args":[],"cwd":"/workspace","env":{},"stdin":null,"timeout_secs":30},"executor_chain":["worker:test/factory","native:factory"]}],
        "entrypoint":"spawn","capabilities":{"requires_model":false,"requires_subprocess":true,"requires_network":false,"custom":[]},
        "materialization_requirements":[],"network_authority_ceiling":"isolated","filesystem_authority_ceiling":"captured_execution",
        "target_requirement":target,"endpoint_requirement":{"kind":"local"},"external_endpoint_binding":null,
        "subprocess_limits":null, "resource_authority_ceiling":"node_policy","cache_key":"factory","executor_chain":["worker:test/factory","native:factory"],"executor_authorities":[],"runtime_identity":runtime
    })).unwrap();
    plan.validate_endpoint_for_sealing().unwrap();
    let execution_plan = serde_json::to_value(&plan).unwrap();
    let exact_program = serde_json::to_value(&exact).unwrap();
    let mut capsule = AdmittedPersistentSessionCapsule {
        external_candidate: None,
        schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
        kind: PERSISTENT_SESSION_CAPSULE_KIND.into(),
        exact_program_hash: lillux::sha256_hex(
            lillux::canonical_json(&exact_program).unwrap().as_bytes(),
        ),
        exact_program,
        retained_product_selections: None,
        retained_external_runtime_qualification: None,
        retained_external_runtime_content_qualification: None,
        lifecycle: PersistentSessionLifecycleContract {
            max_processes: 1,
            max_inflight_per_process: 1,
            max_address_space_bytes: 64 * 1024 * 1024,
            max_cpu_seconds: 1,
            real_uid_process_limit: 1,
            ready_timeout_ms: 1000,
            request_timeout_ms: 1000,
            idle_timeout_ms: 1000,
        },
        wire: PersistentSessionWireContract {
            channel_env: "RYEOS_SESSION_FD".into(),
            wire_protocol: "test.factory-session".into(),
            wire_version: 1,
            max_frame_bytes: 4096,
        },
        artifact_identity: AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executor_ref: "native:factory".into(),
            root_subject_source_content_digest: subject_hash,
            root_subject_signer_fingerprint: Some(fp.clone()),
            root_subject_source_identity: DirectRootSourceIdentity::Bundle {
                manifest_hash: root_manifest,
                manifest_signer_fingerprint: fp.clone(),
            },
            protocol_ref: "protocol:test/factory_session".into(),
            protocol_content_hash: protocol_hash.clone(),
            protocol_signer_fingerprint: fp,
            execution_plan_hash: lillux::sha256_hex(
                lillux::canonical_json(&execution_plan).unwrap().as_bytes(),
            ),
            executable_identity: DirectExecutableIdentity::CapturedContent {
                content_hash: executable_hash.clone(),
            },
            runtime_identity: runtime,
        },
        execution_closure: AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan,
            protocol_descriptor_document: protocol_document,
            command: AdmittedDirectCommandClosure::ContentAddressed {
                executable_blob_hash: executable_hash,
                execution_path: path,
            },
            admitted_project_root: Some(PathBuf::from("/workspace")),
        },
        execution_realization_hash: "0".repeat(64),
        source_binding_hash: None,
        structured_session_profile: None,
        executable_search: vec![],
        process_environment: Default::default(),
        runtime_ref: "runtime:test/factory".into(),
        executor_ref: "native:factory".into(),
    };
    capsule.validate().unwrap();
    let node = crate::execution_identity_probe::boot_node_execution_identity(
        &state.state_store,
        &state.identity,
        &crate::build_info::get(),
    )
    .unwrap();
    let mut extensions = (*state.extensions).clone();
    extensions.insert(Arc::clone(&node));
    state.extensions = Arc::new(extensions);
    let selected = state.execution_resources.select(Some(&target)).unwrap();
    let authority = capsule.authority();
    let realization = AdmittedExecutionRealization {
        schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
        kind: ADMITTED_EXECUTION_REALIZATION_KIND.into(),
        substrate_identity_hash: node.identity_hash.clone(),
        substrate_attestation_hash: node.attestation_hash.clone(),
        launch_authority_digest: authority.digest().unwrap(),
        effective_definition_digest: exact.effective_definition_digest,
        artifact_identity_digest: authority.artifact_identity_digest().unwrap(),
        execution_closure_digest: authority.execution_closure_digest().unwrap(),
        contract_ref: "protocol:test/factory_session".into(),
        contract_digest: protocol_hash,
        components: crate::execution_realization::execution_components(&state, &resolution)
            .unwrap(),
        properties: crate::execution_realization::execution_properties(
            &state,
            plan.filesystem_authority_ceiling,
            plan.network_authority_ceiling,
            plan.target_requirement.as_ref(),
            plan.resource_authority_ceiling,
            selected.selections(),
        )
        .unwrap(),
    };
    capsule.execution_realization_hash =
        cas.store_object(&realization.to_value().unwrap()).unwrap();
    let hash = cas.store_object(&capsule.to_value().unwrap()).unwrap();
    (root, state, hash)
}

#[test]
fn genuine_j1_factory_retains_actual_signed_cas_and_same_descriptor_selection() {
    let (_root, state, hash) = fixture();
    let proof = VerifiedTrustedPooledSessionInputs::verify(&state, &hash).unwrap();
    proof.require_same_state(&state).unwrap();
    assert_eq!(proof.capsule().content_hash().unwrap(), hash);
    assert_eq!(proof.selected().selections().len(), 1);
    let selected = &proof.selected().selections()[0];
    assert_eq!(selected.character_devices[0].major, 1);
    assert_eq!(selected.character_devices[0].minor, 3);
    assert!(selected.matched_facts.is_empty());
    assert!(
        proof.selected().devices().unwrap().authorities()[0]
            .inherited_descriptor()
            .unwrap()
            > 2
    );
    let other_root = tempfile::tempdir().unwrap();
    let other = crate::state::test_support::build(other_root.path()).unwrap();
    assert!(proof.require_same_state(&other).is_err());
}

#[test]
fn genuine_j1_factory_refuses_revoked_protocol_signer_and_changed_node_agreement() {
    let (_root, mut state, hash) = fixture();
    assert!(VerifiedTrustedPooledSessionInputs::verify(&state, &hash).is_ok());
    let original_policy = Arc::clone(&state.node_policy);
    let mut policy = state
        .node_policy
        .require::<NodeExecutionAdmissionPolicy>()
        .unwrap()
        .clone();
    policy
        .resource_authority
        .as_mut()
        .unwrap()
        .trusted_cleanup_contract_digest = Some(lillux::sha256_hex(b"changed cleanup"));
    state.node_policy = Arc::new(crate::node_policy::NodePolicySnapshot::from_test_records(
        vec![Arc::new(policy)],
    ));
    assert!(VerifiedTrustedPooledSessionInputs::verify(&state, &hash).is_err());
    state.node_policy = original_policy;
    assert!(VerifiedTrustedPooledSessionInputs::verify(&state, &hash).is_ok());
    let mut engine = (*state.engine).clone();
    engine.node_trust_store = ryeos_engine::trust::TrustStore::from_signers(vec![]);
    state.engine = Arc::new(engine);
    assert!(VerifiedTrustedPooledSessionInputs::verify(&state, &hash).is_err());
}

#[test]
fn genuine_j1_factory_refuses_tampered_retained_protocol_bytes_before_selection_authority() {
    let (_root, state, hash) = fixture();
    assert!(VerifiedTrustedPooledSessionInputs::verify(&state, &hash).is_ok());
    let mut capsule = state
        .state_store
        .admitted_persistent_session_capsule(&hash)
        .unwrap();
    let AdmittedExecutionClosure::DirectItemExecutor {
        protocol_descriptor_document,
        ..
    } = &mut capsule.execution_closure
    else {
        unreachable!()
    };
    *protocol_descriptor_document =
        protocol_descriptor_document.replace("max_frame_bytes: 4096", "max_frame_bytes: 8192");
    // Retain actual contradictory bytes under their own CAS hash. The trusted
    // signer/realization cannot turn this changed signed body into a proof.
    let authority = state.state_store.pinned_state_authority().unwrap();
    let altered = authority
        .cas_store()
        .unwrap()
        .store_object(&serde_json::to_value(&capsule).unwrap())
        .unwrap();
    assert_ne!(altered, hash);
    let error = VerifiedTrustedPooledSessionInputs::verify(&state, &altered)
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("verify retained session protocol")
    );
}
