//! Actual signed protocol/CAS/node-attestation/descriptor fixture for the
//! production read-only J1 factory. No raw proof constructor or native launch.
use super::*;
use crate::node_policy::sections::execution::*;
use ryeos_engine::{contracts::*, resolution::*};
use ryeos_state::objects::*;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

fn fixture() -> (tempfile::TempDir, crate::state::AppState, String) {
    fixture_with_startup(None)
}

fn fixture_with_startup(
    startup: Option<ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection>,
) -> (tempfile::TempDir, crate::state::AppState, String) {
    let root = tempfile::tempdir().unwrap();
    let mut state = crate::state::test_support::build(root.path()).unwrap();
    let hash = populate_fixture(&mut state, startup);
    (root, state, hash)
}

// Existing genuine factory on the SAME budget fixture State; Linux cfg(test)
// visibility only. No raw proof/original binder constructor.
pub(crate) fn populate_fixture(
    state: &mut crate::state::AppState,
    startup: Option<ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection>,
) -> String {
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
    let mut protocol_body = format!(
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
    if let Some(startup) = startup {
        protocol_body.push_str("  startup_contract:\n");
        for line in serde_yaml::to_string(&startup).unwrap().lines() {
            protocol_body.push_str("    ");
            protocol_body.push_str(line);
            protocol_body.push('\n');
        }
    }
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
        components: crate::execution_realization::execution_components(state, &resolution)
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
    hash
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

#[test]
fn genuine_signed_startup_selection_remains_static_without_original_binder() {
    let selected = ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
        source_root: "fixture".into(),
        member: "startup.json".into(),
        sha256: lillux::sha256_hex(b"fixture contract bytes not admitted here"),
    };
    let (_root, mut state, hash) = fixture_with_startup(Some(selected.clone()));
    let inputs = VerifiedTrustedPooledSessionInputs::verify(&state, &hash).unwrap();
    assert!(inputs.has_selected_startup_contract());
    assert_eq!(inputs.startup_contract_selection.as_ref(), Some(&selected));
    // Actual signed static admission is NOT the original protected binder or
    // current GPU/device/provider result; the genuine fixture uses /dev/null.
    assert!(inputs.original_isolation.is_none());
    assert!(inputs.original_startup_contract.is_none());
    assert!(inputs.original_startup_request_inputs(&state).is_err());
    let mut engine = (*state.engine).clone();
    engine.node_trust_store = ryeos_engine::trust::TrustStore::from_signers(vec![]);
    state.engine = Arc::new(engine);
    assert!(VerifiedTrustedPooledSessionInputs::verify(&state, &hash).is_err());
}

#[test]
fn genuine_signed_resolution_refuses_absent_source_binding_in_actual_cas() {
    let (_root, state, hash) = fixture();
    let inputs = VerifiedTrustedPooledSessionInputs::verify(&state, &hash).unwrap();
    let mut resolution = inputs.exact().resolution_output.restore();
    let missing = lillux::sha256_hex(b"absent source binding test object");
    let authority = state.state_store.pinned_state_authority().unwrap();
    assert!(
        authority
            .cas_store()
            .unwrap()
            .get_object(&missing)
            .unwrap()
            .is_none()
    );
    let projection = EffectiveSourceClosureProjection {
        schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
        binding_hash: missing,
        content_manifest_hash: lillux::sha256_hex(b"absent source manifest"),
        owner_key: lillux::sha256_hex(b"unadmitted projection address"),
        file_count: 1,
        total_bytes: 1,
    };
    resolution.composed.derived.insert(
        ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.into(),
        projection.to_value().unwrap(),
    );
    // A genuine signed-resolution/CAS negative: the altered projection is NOT
    // admitted source authority, and no AdmittedSourceClosure is fabricated.
    let error =
        crate::source_closure_admission::recover_source_closure(&state, &state.engine, &resolution)
            .err()
            .unwrap();
    assert_eq!(error.to_string(), "retained source binding is absent");
}

#[test]
fn genuine_retained_signed_startup_selector_refuses_tampered_member() {
    let selected = ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
        source_root: "fixture".into(),
        member: "startup.json".into(),
        sha256: lillux::sha256_hex(b"retained signed selector fixture"),
    };
    let (_root, state, hash) = fixture_with_startup(Some(selected));
    VerifiedTrustedPooledSessionInputs::verify(&state, &hash).unwrap();
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
    assert!(protocol_descriptor_document.contains("startup.json"));
    *protocol_descriptor_document =
        protocol_descriptor_document.replace("startup.json", "foreign.json");
    let authority = state.state_store.pinned_state_authority().unwrap();
    let altered = authority
        .cas_store()
        .unwrap()
        .store_object(&serde_json::to_value(&capsule).unwrap())
        .unwrap();
    assert_ne!(altered, hash);
    assert!(VerifiedTrustedPooledSessionInputs::verify(&state, &altered).is_err());
    // Actual signed descriptor/CAS verification refuses modified original
    // bytes; no signature refresh, admitted source or original budget forged.
}

// This is a raw-member C2 fixture, not an accepted startup contract. The static
// signed protocol selects these exact bytes; its existing capsule has no
// original source binder. A separate genuine temporary source admission below
// exercises the production member reader without claiming that missing join.
const C2_RAW_MEMBER: &[u8] = b"{\"fixture\":\"C2 raw source member only\"}\n";

fn temporary_c2_fixture() -> (
    tempfile::TempDir,
    crate::state::AppState,
    VerifiedTrustedPooledSessionInputs,
    crate::source_closure_admission::temporary_source_fixture::Fixture,
) {
    let selected = ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
        source_root: "source".into(),
        member: "startup.json".into(),
        sha256: lillux::sha256_hex(C2_RAW_MEMBER),
    };
    let (root, mut state, hash) = fixture_with_startup(Some(selected.clone()));
    let inputs = VerifiedTrustedPooledSessionInputs::verify(&state, &hash).unwrap();
    assert_eq!(inputs.startup_contract_selection.as_ref(), Some(&selected));
    assert!(inputs.original_isolation.is_none());
    assert!(inputs.original_startup_contract.is_none());
    let source =
        crate::source_closure_admission::temporary_source_fixture::admit(&mut state, C2_RAW_MEMBER)
            .unwrap();
    (root, state, inputs, source)
}

#[test]
fn temporary_c2_actual_admission_publication_recovery_reads_signed_selected_raw_member() {
    let (_root, state, inputs, source) = temporary_c2_fixture();
    let authority = source.publication.authority();
    let guard = authority.acquire_shared_guard().unwrap();
    let selection = inputs.startup_contract_selection.as_ref().unwrap();
    let loaded = crate::source_closure_admission::load_selected_startup_contract_member(
        &source.recovered,
        selection,
        &guard,
    )
    .unwrap();
    assert_eq!(loaded.bytes, C2_RAW_MEMBER);
    assert_eq!(loaded.file.root, selection.source_root);
    assert_eq!(loaded.file.path, selection.member);
    assert_eq!(loaded.file.blob_hash, lillux::sha256_hex(&loaded.bytes));
    // Recovery/member reads use immutable retained CAS, not reopened Bundle
    // bytes. Changing the live test member cannot substitute the retained one.
    std::fs::write(
        source
            .bundle
            .path()
            .join(".ai/workers/test/source/startup.json"),
        b"changed live temporary member",
    )
    .unwrap();
    let recovered = crate::source_closure_admission::recover_source_closure(
        &state,
        &state.engine,
        &source.resolution,
    )
    .unwrap()
    .unwrap();
    let retained = crate::source_closure_admission::load_selected_startup_contract_member(
        &recovered, selection, &guard,
    )
    .unwrap();
    assert_eq!(retained.bytes, C2_RAW_MEMBER);
    assert!(inputs.original_startup_request_inputs(&state).is_err());
}

#[test]
fn temporary_c2_actual_recovered_source_refuses_foreign_guard_before_member_read() {
    let (_root, _state, inputs, source) = temporary_c2_fixture();
    let foreign_root = tempfile::tempdir().unwrap();
    let foreign_state = crate::state::test_support::build(foreign_root.path()).unwrap();
    let foreign = foreign_state.state_store.pinned_state_authority().unwrap();
    let foreign_guard = foreign.acquire_shared_guard().unwrap();
    assert!(
        crate::source_closure_admission::load_selected_startup_contract_member(
            &source.recovered,
            inputs.startup_contract_selection.as_ref().unwrap(),
            &foreign_guard,
        )
        .is_err()
    );
}

#[test]
fn temporary_c2_actual_recovered_source_refuses_substituted_member_root_and_hash() {
    let (_root, _state, inputs, source) = temporary_c2_fixture();
    let guard = source
        .publication
        .authority()
        .acquire_shared_guard()
        .unwrap();
    let selected = inputs.startup_contract_selection.as_ref().unwrap();
    for altered in [
        ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
            member: "missing.json".into(),
            ..selected.clone()
        },
        ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
            source_root: "foreign".into(),
            ..selected.clone()
        },
        ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
            sha256: lillux::sha256_hex(b"substituted raw bytes"),
            ..selected.clone()
        },
    ] {
        // Altered selectors are negative test data, never signed authority.
        assert!(
            crate::source_closure_admission::load_selected_startup_contract_member(
                &source.recovered,
                &altered,
                &guard,
            )
            .is_err()
        );
    }
}

#[test]
fn temporary_c2_actual_source_recovery_refuses_revoked_temporary_signer() {
    let (_root, mut state, _inputs, source) = temporary_c2_fixture();
    let mut engine = (*state.engine).clone();
    engine.node_trust_store = ryeos_engine::trust::TrustStore::from_signers(vec![]);
    engine.trust_store = ryeos_engine::trust::TrustStore::from_signers(vec![]);
    state.engine = Arc::new(engine);
    assert!(
        crate::source_closure_admission::recover_source_closure(
            &state,
            &state.engine,
            &source.resolution,
        )
        .is_err()
    );
}

fn temporary_c2_nondefault_source() -> (
    tempfile::TempDir,
    crate::state::AppState,
    crate::source_closure_admission::temporary_source_fixture::Fixture,
    StartupSourceMember,
) {
    let root = tempfile::tempdir().unwrap();
    let mut state = crate::state::test_support::build(root.path()).unwrap();
    let source =
        crate::source_closure_admission::temporary_source_fixture::admit_with_logical_root(
            &mut state,
            C2_RAW_MEMBER,
            "lib/local-tinygrad-gnu",
        )
        .unwrap();
    let file = source
        .recovered
        .manifest()
        .entries
        .iter()
        .find(|file| file.root == "source" && file.path == "bootstrap.py")
        .unwrap();
    let member = StartupSourceMember {
        root: file.root.clone(),
        path: file.path.clone(),
        sha256: file.blob_hash.clone(),
    };
    (root, state, source, member)
}

#[test]
fn temporary_c2_nondefault_worker_directory_accepts_actual_manifest_entry_coordinate() {
    let (_root, _state, source, member) = temporary_c2_nondefault_source();
    assert!(matches!(
        &source.recovered.binding().logical_binding,
        SourceLogicalBinding::Worker { root, entry }
            if root == "lib/local-tinygrad-gnu" && entry == "bootstrap.py"
    ));
    assert_eq!(source.recovered.manifest().roots[0].id, "source");
    require_startup_worker_entry(&source.recovered, &member).unwrap();
    // Also authenticate the actual raw selected member under SAME publication
    // guard. This is not a full original startup/binder or funded-owner test.
    let guard = source
        .publication
        .authority()
        .acquire_shared_guard()
        .unwrap();
    let selection =
        ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
            source_root: "source".into(),
            member: "startup.json".into(),
            sha256: lillux::sha256_hex(C2_RAW_MEMBER),
        };
    let raw = crate::source_closure_admission::load_selected_startup_contract_member(
        &source.recovered,
        &selection,
        &guard,
    )
    .unwrap();
    assert_eq!(raw.bytes, C2_RAW_MEMBER);
}

#[test]
fn temporary_c2_nondefault_worker_entry_refuses_directory_foreign_root_path_and_hash() {
    let (_root, _state, source, member) = temporary_c2_nondefault_source();
    let other = source
        .recovered
        .manifest()
        .entries
        .iter()
        .find(|file| file.path == "startup.json")
        .unwrap();
    for altered in [
        StartupSourceMember {
            root: "lib/local-tinygrad-gnu".into(),
            path: member.path.clone(),
            sha256: member.sha256.clone(),
        },
        StartupSourceMember {
            root: "foreign".into(),
            path: member.path.clone(),
            sha256: member.sha256.clone(),
        },
        // This is an actual admitted member/hash, but not the Worker entry.
        StartupSourceMember {
            root: other.root.clone(),
            path: other.path.clone(),
            sha256: other.blob_hash.clone(),
        },
        StartupSourceMember {
            root: member.root.clone(),
            path: member.path.clone(),
            sha256: lillux::sha256_hex(b"substituted entry bytes"),
        },
    ] {
        assert!(require_startup_worker_entry(&source.recovered, &altered).is_err());
    }
}

#[test]
fn temporary_c2_nondefault_worker_recovery_refuses_changed_logical_source_declaration() {
    let (_root, state, mut source, _member) = temporary_c2_nondefault_source();
    source.resolution.composed.composed["source"]["root"] =
        serde_json::Value::String("lib/foreign-worker".into());
    // Negative resolution data is not resigned or promoted into a binding.
    assert!(
        crate::source_closure_admission::recover_source_closure(
            &state,
            &state.engine,
            &source.resolution,
        )
        .is_err()
    );
}

#[test]
fn temporary_c2_logical_root_builder_refuses_noncanonical_paths() {
    let root = tempfile::tempdir().unwrap();
    let mut state = crate::state::test_support::build(root.path()).unwrap();
    let engine = Arc::clone(&state.engine);
    for logical in [
        "",
        "/outside",
        "../outside",
        "lib/../outside",
        "lib//worker",
        "lib/",
        "lib\\worker",
        "lib/worker\nsource: injected",
    ] {
        assert!(
            crate::source_closure_admission::temporary_source_fixture::admit_with_logical_root(
                &mut state,
                C2_RAW_MEMBER,
                logical,
            )
            .is_err()
        );
        assert!(Arc::ptr_eq(&engine, &state.engine));
    }
}

#[test]
fn temporary_c2_logical_root_yaml_metacharacters_preserve_signed_declaration() {
    let root = tempfile::tempdir().unwrap();
    let mut state = crate::state::test_support::build(root.path()).unwrap();
    let logical = "lib/worker: quoted";
    let source =
        crate::source_closure_admission::temporary_source_fixture::admit_with_logical_root(
            &mut state,
            C2_RAW_MEMBER,
            logical,
        )
        .unwrap();
    assert_eq!(
        source.resolution.composed.composed["source"]["root"],
        logical
    );
    assert!(matches!(
        &source.recovered.binding().logical_binding,
        SourceLogicalBinding::Worker { root, .. } if root == logical
    ));
    assert_eq!(source.recovered.manifest().roots[0].id, "source");
}

// Closed syntax data only: product/launch hashes below are placeholders. The
// nested-source control authenticates actual source bytes, not a full original
// binder, bootstrap execution, qualified product pair or funded held child.
fn fixed_v2_syntax() -> serde_json::Value {
    let entry = serde_json::json!({"root":"source","path":"bootstrap.py","sha256":"a".repeat(64)});
    let consumer =
        serde_json::json!({"root":"source","path":"bootstrap_support.py","sha256":"b".repeat(64)});
    serde_json::json!({
        "schema":"ryeos.local_tinygrad_current_assignment_startup.v2",
        "role":"current_assignment_nvml_preflight_v2",
        "entry":entry.clone(), "bootstrap_members":[entry,consumer],
        "launch":{"command_sha256":"b".repeat(64),"args":[{"kind":"admitted_source_entry"}],"env":{},
            "process_environment_sha256":"c".repeat(64),"dynamic_channels":[{"env":"RYEOS_SESSION_FD","target_fd":0},{"env":"RYEOS_RESOURCE_CLEANUP_FD","target_fd":4}]},
        "products":{"gnu_slot":"python-gnu","driver_slot":"driver","nvml_member":"lib/libnvidia-ml.so.1",
            "provider_checks":"trusted-gnu-loader-provider-and-function-origin-v1","gnu_manifest_sha256":"d".repeat(64),
            "driver_manifest_sha256":"e".repeat(64),"tinygrad_manifest_sha256":"f".repeat(64)},
        "policy":{"schema":"ryeos.local_tinygrad_nvml_assignment_policy.v1","primary_device_role":"primary","l4_query_policy":null},
        "wire":{"target_fd":4,"preflight_version":3,"result_version":3,"init_version":2,"maximum_frame_bytes":4096},
        "manifest_delivery":{"abi":"fixed_namespace_v1",
            "gnu":{"destination":"/opt/ryeos-original-manifests/gnu.json","access":"read_only"},
            "driver":{"destination":"/opt/ryeos-original-manifests/driver.json","access":"read_only"},
            "consumer":{"root":"source","path":"bootstrap_support.py"}}
    })
}

fn fixed_v2_selection(
    bytes: &[u8],
) -> ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
    ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
        source_root: "source".into(),
        member: "contracts/current-assignment-startup-v2.json".into(),
        sha256: lillux::sha256_hex(bytes),
    }
}

#[test]
fn current_v2_fixed_selector_reads_actual_nested_recovered_member_under_same_guard() {
    let raw = serde_json::to_vec(&fixed_v2_syntax()).unwrap();
    let selected = fixed_v2_selection(&raw);
    let (root, mut state, capsule_hash) = fixture_with_startup(Some(selected.clone()));
    let inputs = VerifiedTrustedPooledSessionInputs::verify(&state, &capsule_hash).unwrap();
    assert_eq!(inputs.startup_contract_selection.as_ref(), Some(&selected));
    assert!(inputs.original_startup_contract.is_none());
    let source = crate::source_closure_admission::temporary_source_fixture::admit_with_logical_root_and_member(
        &mut state, &raw, "lib/local-tinygrad-gnu", &selected.member,
    ).unwrap();
    assert!(matches!(&source.recovered.binding().logical_binding,
        SourceLogicalBinding::Worker { root, entry } if root == "lib/local-tinygrad-gnu" && entry == "bootstrap.py"));
    let guard = source
        .publication
        .authority()
        .acquire_shared_guard()
        .unwrap();
    let loaded = crate::source_closure_admission::load_selected_startup_contract_member(
        &source.recovered,
        inputs.startup_contract_selection.as_ref().unwrap(),
        &guard,
    )
    .unwrap();
    assert_eq!(loaded.bytes, raw);
    assert_eq!(loaded.file.root, "source");
    assert_eq!(loaded.file.path, selected.member);
    assert_eq!(loaded.file.blob_hash, lillux::sha256_hex(&loaded.bytes));
    let decoded = decode_startup_contract(&loaded.bytes).unwrap();
    require_current_v2_startup_selector(&selected, &decoded).unwrap();
    let mut substituted = selected.clone();
    substituted.sha256 = lillux::sha256_hex(b"different raw member");
    assert!(
        crate::source_closure_admission::load_selected_startup_contract_member(
            &source.recovered,
            &substituted,
            &guard,
        )
        .is_err()
    );
    // No legacy filename fallback exists in either selector or actual CAS read.
    let mut legacy = selected.clone();
    legacy.member = "startup.json".into();
    assert!(require_current_v2_startup_selector(&legacy, &decoded).is_err());
    assert!(
        crate::source_closure_admission::load_selected_startup_contract_member(
            &source.recovered,
            &legacy,
            &guard,
        )
        .is_err()
    );
    assert!(inputs.original_startup_request_inputs(&state).is_err());
    drop(guard);
    drop(source);
    drop(root);
}

#[test]
fn current_v2_fixed_selector_refuses_canonical_alternates_and_invalid_selection() {
    let raw = serde_json::to_vec(&fixed_v2_syntax()).unwrap();
    let selection = fixed_v2_selection(&raw);
    let decoded = decode_startup_contract(&raw).unwrap();
    require_current_v2_startup_selector(&selection, &decoded).unwrap();
    for (root, member, hash) in [
        (
            "foreign",
            selection.member.as_str(),
            selection.sha256.as_str(),
        ),
        ("source", "contracts/other.json", selection.sha256.as_str()),
        ("source", "startup.json", selection.sha256.as_str()),
        (
            "source",
            "contracts/../current-assignment-startup-v2.json",
            selection.sha256.as_str(),
        ),
        ("source", selection.member.as_str(), "invalid-sha"),
    ] {
        let alternate =
            ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
                source_root: root.into(),
                member: member.into(),
                sha256: hash.into(),
            };
        assert!(require_current_v2_startup_selector(&alternate, &decoded).is_err());
    }
    // Change the unique membership together with the consumer so these remain
    // closed-schema-valid documents; refusal must be the finite predicate.
    for (root, path) in [("foreign", "bootstrap_support.py"), ("source", "other.py")] {
        let mut value = fixed_v2_syntax();
        value["manifest_delivery"]["consumer"]["root"] = root.into();
        value["manifest_delivery"]["consumer"]["path"] = path.into();
        value["bootstrap_members"][1]["root"] = root.into();
        value["bootstrap_members"][1]["path"] = path.into();
        let changed = decode_startup_contract(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(require_current_v2_startup_selector(&selection, &changed).is_err());
    }
}

#[test]
fn current_v2_fixed_selector_preserves_separate_legacy_role() {
    let mut value = fixed_v2_syntax();
    value["schema"] = "ryeos.local_tinygrad_current_assignment_startup.v1".into();
    value["role"] = "current_assignment_nvml_preflight_v1".into();
    value.as_object_mut().unwrap().remove("manifest_delivery");
    let raw = serde_json::to_vec(&value).unwrap();
    let decoded = decode_startup_contract(&raw).unwrap();
    let legacy = ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
        source_root: "source".into(),
        member: "startup.json".into(),
        sha256: lillux::sha256_hex(&raw),
    };
    require_current_v2_startup_selector(&legacy, &decoded).unwrap();
    value["role"] = "current_assignment_nvml_preflight_v2".into();
    assert!(decode_startup_contract(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn current_v2_missing_nested_member_refuses_actual_source_read() {
    let (_root, _state, _inputs, source) = temporary_c2_fixture();
    let guard = source
        .publication
        .authority()
        .acquire_shared_guard()
        .unwrap();
    let selected = fixed_v2_selection(C2_RAW_MEMBER);
    let error = crate::source_closure_admission::load_selected_startup_contract_member(
        &source.recovered,
        &selected,
        &guard,
    )
    .err()
    .expect("missing nested member must refuse");
    assert!(
        error
            .to_string()
            .contains("member absent from actual source")
    );
}

#[test]
fn current_v2_member_builder_refuses_noncanonical_and_entry_collision_paths() {
    for member in [
        "",
        "/contracts/x.json",
        "../x.json",
        "contracts/../x.json",
        "contracts//x.json",
        "contracts/./x.json",
        "bootstrap.py",
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::state::test_support::build(root.path()).unwrap();
        assert!(crate::source_closure_admission::temporary_source_fixture::admit_with_logical_root_and_member(
            &mut state, b"inert syntax", "lib/local-tinygrad-gnu", member,
        ).is_err());
    }
}
