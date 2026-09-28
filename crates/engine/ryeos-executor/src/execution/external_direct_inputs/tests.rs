//! Component assembly through the real sealed invocation decoder and CAS.
//! These tests do not claim born-thread, endpoint or provider admission.

use super::*;
use ryeos_engine::protocols::VerifiedProtocol;
use ryeos_state::objects::*;
use serde_json::json;
use std::collections::BTreeMap;

const THREAD: &str = "T-retained-inputs";
const CHAIN: &str = "T-retained-inputs-root";

fn signed_protocol() -> (VerifiedProtocol, String) {
    let body = "kind: protocol\nname: direct\ncategory: test\nabi_version: v1\nstdin: {shape: opaque}\nstdout: {shape: opaque_bytes, mode: terminal}\nenv_injections:\n  - {name: RYE_THREAD_ID, source: thread_id}\n  - {name: RYE_PROJECT_PATH, source: project_path}\ncapabilities: {allows_pushed_head: false, allows_target_site: false, allows_detached: false}\nlifecycle: {mode: managed}\ncallback_channel: none\n";
    let key = lillux::crypto::SigningKey::from_bytes(&[57; 32]);
    let document =
        lillux::signature::sign_content_at(body, &key, "#", None, "2026-09-23T00:00:00Z");
    let header =
        lillux::signature::parse_signature_line(document.lines().next().unwrap(), "#", None)
            .unwrap();
    assert!(lillux::signature::verify_signature(
        &header.content_hash,
        &header.signature_b64,
        &key.verifying_key(),
    ));
    let protocol = VerifiedProtocol {
        canonical_ref: "protocol:test/direct".into(),
        raw_content_digest: header.content_hash,
        signer_fingerprint: header.signer_fingerprint,
        descriptor: serde_yaml::from_str(body).unwrap(),
        trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
        bundle_root: "/fixture/bundle".into(),
        descriptor_path: "/fixture/not-opened/protocol.yaml".into(),
    };
    (protocol, document)
}

#[cfg(target_os = "linux")]
fn retained_inputs_capsule(
    state: &ryeos_app::state::AppState,
    records: &VerifiedAdmittedSourceRecords,
    live: &Path,
) -> (AdmittedLaunchCapsule, String, VerifiedProtocol) {
    use ryeos_app::thread_lifecycle::SealedRootExecutionRequest;
    let (mut capsule, mut resolution) =
        crate::execution::source_closure::tests::external_delivery::capsule(records);
    let authority = state.state_store.pinned_state_authority().unwrap();
    let cas = authority.cas_store().unwrap();
    let project_bytes = b"retained base data";
    let project_file = ProjectFile {
        blob_hash: cas.store_blob(project_bytes).unwrap(),
        size: project_bytes.len() as u64,
        normalized_mode: ProjectFile::REGULAR_MODE,
    };
    let tree = ProjectTree {
        files: BTreeMap::from([(
            "data.txt".into(),
            cas.store_object(&project_file.to_value()).unwrap(),
        )]),
    };
    let policy = ProjectSnapshotPolicy::new(
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
    )
    .unwrap();
    let snapshot = ProjectSnapshot {
        project_tree_hash: cas.store_object(&tree.to_value()).unwrap(),
        effective_policy_hash: cas.store_object(&policy.to_value()).unwrap(),
        message: None,
        parent_hashes: Vec::new(),
        created_at: "2026-09-23T00:00:00Z".into(),
        source: "external-direct-input-component-test".into(),
    };
    let snapshot_hash = cas.store_object(&snapshot.to_value()).unwrap();
    let project = ExecutionProjectAuthority::pinned(
        "external-direct-input-fixture".into(),
        Some(live.to_path_buf()),
        snapshot_hash,
        PinnedProjectRealization::ReadOnly,
        EnvironmentAuthority::None,
        Vec::new(),
    )
    .unwrap();

    let command = b"#!/bin/sh\nexit 0\n";
    let command_hash = cas.store_blob(command).unwrap();
    let manifest = json!({
        "schema": EXTERNAL_CONTENT_TREE_SCHEMA, "kind": EXTERNAL_CONTENT_MANIFEST_KIND,
        "entries": [{"path":"check", "kind":"file", "mode":493,
            "blob_hash":command_hash, "size":command.len()}],
        "entry_count":1, "total_bytes":command.len(),
    });
    ExternalContentManifestObject::from_value(&manifest).unwrap();
    let product_hash = cas.store_object(&manifest).unwrap();
    let product = ExternalContentRealization {
        id: "runtime".into(),
        kind: ExternalContentKind::Tree,
        mode: ExternalContentMode::Pinned,
        manifest_hash: product_hash.clone(),
        entry_count: 1,
        total_bytes: command.len() as u64,
        mount_root: ExternalContentMountRoot::Project,
        mount: "vendor/runtime".into(),
    };
    resolution.composed.derived.insert(
        EXTERNAL_REALIZATIONS_DERIVED_KEY.into(),
        ExternalContentRealizationSet::new(vec![product])
            .unwrap()
            .to_value()
            .unwrap(),
    );
    // Diagnostic paths deliberately name a mutable location. The sealed source
    // bytes and source closure, not that location, must determine guest inputs.
    resolution.root.source_path = live.join("runtime.yaml");
    let invocation = &mut capsule.sealed_invocation;
    invocation["executor_route"] = json!({
        "route":"root_executor_chain", "executor_ref":capsule.executor_ref,
    });
    let project_invocation = serde_json::to_value(
        SealedRootExecutionRequest::storage_test_fixture_with_project_identity(
            ryeos_engine::contracts::ProjectContext::LocalPath {
                path: live.to_path_buf(),
            },
            project.clone(),
        ),
    )
    .unwrap();
    for field in [
        "project_context",
        "project_authority",
        "project_binding_subject_authority",
        "resolution_subject_authority",
    ] {
        invocation[field] = project_invocation[field].clone();
    }
    invocation["verified_subject"]["subject_resolution_authority"] =
        project_invocation["verified_subject"]["subject_resolution_authority"].clone();
    invocation["verified_subject"]["source_path"] = json!(live.join("runtime.yaml"));
    invocation["verified_signer_fingerprint"] = json!(records.binding().owner.signer_fingerprint);
    invocation["verified_trust_class"] =
        serde_json::to_value(ryeos_engine::contracts::TrustClass::Trusted).unwrap();
    invocation["resolution_output"] = serde_json::to_value(&resolution).unwrap();
    invocation["effective_definition_digest"] =
        serde_json::to_value(resolution.effective_definition_digest().unwrap()).unwrap();
    capsule.project_authority = project;
    capsule.exact_program =
        admitted_launch_capsule::project_sealed_root_exact_program(invocation).unwrap();
    capsule.exact_program_hash = lillux::sha256_hex(
        lillux::canonical_json(&capsule.exact_program)
            .unwrap()
            .as_bytes(),
    );
    let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executable_identity,
        execution_plan_hash,
        protocol_content_hash,
        protocol_signer_fingerprint,
        runtime_identity,
        ..
    } = &mut capsule.artifact_identity
    else {
        unreachable!()
    };
    *executable_identity = DirectExecutableIdentity::CapturedContent {
        content_hash: command_hash.clone(),
    };
    let (protocol, protocol_document) = signed_protocol();
    *protocol_content_hash = protocol.raw_content_digest.clone();
    *protocol_signer_fingerprint = protocol.signer_fingerprint.clone();
    let command_path = Path::new(ADMITTED_DIRECT_PROJECT_ROOT).join("vendor/runtime/check");
    let plan: ryeos_engine::contracts::ExecutionPlan = serde_json::from_value(json!({
        "plan_id":"retained-inputs-component", "root_executor_id":capsule.executor_ref,
        "root_ref":records.binding().owner.canonical_ref, "item_kind":"tool",
        "nodes":[{"node_type":"dispatch_subprocess", "id":"spawn", "spec":{
            "cmd":command_path, "verified_command":{"authority":"captured_content",
                "code":{"source_path":command_path,"content_hash":command_hash}},
            "args":[{"kind":"admitted_source_entry"}], "cwd":live,
            "env":{"FARM_DATA":live.join("data.txt"),"RETAINED_ONLY":"retained-value"},
            "stdin":null, "timeout_secs":30
        }, "executor_chain":[records.binding().owner.canonical_ref,capsule.executor_ref]}],
        "entrypoint":"spawn", "capabilities":{"requires_model":false,
            "requires_subprocess":true,"requires_network":false,"custom":[]},
        "materialization_requirements":[], "network_authority_ceiling":"isolated",
        "filesystem_authority_ceiling":"captured_execution",
        "target_requirement":{"os":"linux","arch":std::env::consts::ARCH,"resources":[]},
        "endpoint_requirement":{"kind":"external","binding_id":"component-direct",
            "stdout_max_bytes":4096,"stderr_max_bytes":2048},
        "external_endpoint_binding":{"binding_id":"component-direct","binding_digest":"e".repeat(64)},
        "resource_authority_ceiling":"node_policy", "cache_key":"component",
        "thread_kind":"tool", "executor_chain":[records.binding().owner.canonical_ref,capsule.executor_ref],
        "executor_authorities":[], "runtime_identity":runtime_identity, "debug_raw":false
    })).unwrap();
    plan.validate_endpoint_for_sealing().unwrap();
    let plan_value = serde_json::to_value(&plan).unwrap();
    *execution_plan_hash =
        lillux::sha256_hex(lillux::canonical_json(&plan_value).unwrap().as_bytes());
    let AdmittedExecutionClosure::DirectItemExecutor {
        command,
        admitted_project_root,
        execution_plan,
        protocol_descriptor_document,
    } = &mut capsule.execution_closure
    else {
        unreachable!()
    };
    *command = AdmittedDirectCommandClosure::RealizationMember {
        executable_blob_hash: command_hash,
        realization_id: "runtime".into(),
        realization_manifest_hash: product_hash.clone(),
        realization_mount_root: ExternalContentMountRoot::Project,
        realization_mount: "vendor/runtime".into(),
        relative_path: "check".into(),
        execution_path: Path::new(ADMITTED_DIRECT_PROJECT_ROOT).join("vendor/runtime/check"),
    };
    *admitted_project_root = Some(live.to_path_buf());
    *execution_plan = plan_value;
    *protocol_descriptor_document = protocol_document;
    capsule.validate().unwrap();
    let sealed = SealedRootExecutionRequest::decode_from_admitted_capsule(&capsule).unwrap();
    sealed.admitted_effective_resolution().unwrap();
    (capsule, product_hash, protocol)
}

#[test]
#[cfg(target_os = "linux")]
fn direct_inputs_assemble_exact_retained_base_product_source_and_keep_source_lease() {
    use ryeos_external_execution_contract::{
        GuestMountAccess, GuestMountContentAuthority, GuestMountRole,
    };
    let (root, state, records, source_bytes) =
        crate::execution::source_closure::tests::external_delivery::state_and_source();
    let live = root.path().join("mutable-project");
    let (capsule, product_hash, protocol) = retained_inputs_capsule(&state, &records, &live);
    // No source, product or project bytes may be recovered from this live root.
    std::fs::create_dir_all(live.join("vendor/runtime")).unwrap();
    std::fs::write(live.join("runtime.yaml"), b"wrong live source").unwrap();
    std::fs::write(live.join("vendor/runtime/check"), b"wrong live runtime").unwrap();
    std::fs::write(live.join("data.txt"), b"wrong live base").unwrap();
    std::fs::write(
        live.join(".env"),
        b"RETAINED_ONLY=wrong-live-value\nLIVE_ONLY=must-not-be-inherited\n",
    )
    .unwrap();
    let prepared =
        prepare_external_direct_inputs(&state, &capsule, &protocol, THREAD, CHAIN).unwrap();
    assert_eq!(
        prepared.authority.projection().environment,
        BTreeMap::from([
            ("FARM_DATA".into(), "/workspace/data.txt".into()),
            ("RETAINED_ONLY".into(), "retained-value".into()),
            ("RYE_THREAD_ID".into(), THREAD.into()),
            ("RYE_PROJECT_PATH".into(), "/workspace".into()),
            ("RYEOS_THREAD_ID".into(), THREAD.into()),
            ("RYEOS_CHAIN_ROOT_ID".into(), CHAIN.into()),
            (
                "RYEOS_ADMITTED_SOURCE".into(),
                records.sealed_identity_env().to_owned()
            ),
        ])
    );
    assert!(prepared.authority.projection().executable_search.is_empty());
    let source = prepared.source.as_ref().unwrap();
    assert_eq!(source.binding_hash(), records.binding_hash());
    let projection = prepared.authority.projection();
    let ExecutionProjectAuthority::PinnedGeneration { snapshot_hash, .. } =
        &capsule.project_authority
    else {
        unreachable!()
    };
    assert_eq!(&projection.base_snapshot.snapshot_hash, snapshot_hash);
    assert!(projection.workspace_outputs.is_none());
    assert_eq!(projection.inputs.len(), 2);
    assert_eq!(projection.inputs[0].role, GuestMountRole::Product);
    assert_eq!(
        projection.inputs[0].destination,
        "/workspace/vendor/runtime"
    );
    assert!(matches!(&projection.inputs[0].content_authority,
        GuestMountContentAuthority::ProductManifest { manifest_hash, .. } if manifest_hash == &product_hash));
    assert_eq!(projection.inputs[1].role, GuestMountRole::Source);
    assert!(matches!(&projection.inputs[1].content_authority,
        GuestMountContentAuthority::SourceClosure { binding_hash, manifest_hash, .. }
            if binding_hash == records.binding_hash() && manifest_hash == records.content_manifest_hash()));
    assert_eq!(
        projection.inputs[1].destination,
        records.runtime_destination().to_str().unwrap()
    );
    assert!(
        projection
            .inputs
            .iter()
            .all(|input| input.access == GuestMountAccess::ReadOnly)
    );
    let descriptors = prepared.authority.retained_descriptors();
    assert_eq!(descriptors.len(), 6); // base, product, source, product manifest, source binding + manifest
    assert_eq!(
        descriptors[0].inherited_descriptor().unwrap(),
        projection.base_snapshot.descriptor
    );
    let transfer = descriptors[0]
        .try_clone_pinned_directory("<retained-base>".into())
        .unwrap();
    let observed =
        ryeos_project_capture::inspect_project_snapshot_transfer(&transfer, snapshot_hash).unwrap();
    assert_eq!(
        observed.closure_digest,
        projection.base_snapshot.closure_digest
    );
    assert_eq!(observed.object_count, projection.base_snapshot.object_count);
    assert_eq!(observed.blob_count, 1);
    assert_eq!(observed.blob_count, projection.base_snapshot.blob_count);
    assert_eq!(observed.total_bytes, projection.base_snapshot.total_bytes);
    for (input, descriptor) in projection.inputs.iter().zip(&descriptors[1..3]) {
        assert_eq!(input.descriptor, descriptor.inherited_descriptor().unwrap());
    }
    let product_dir = descriptors[1]
        .try_clone_pinned_directory("<retained-product>".into())
        .unwrap();
    assert_eq!(
        product_dir
            .open_pinned_regular_descendant(Path::new("check"), false)
            .unwrap()
            .unwrap()
            .read_bounded(1024)
            .unwrap(),
        b"#!/bin/sh\nexit 0\n"
    );
    let source_dir = descriptors[2]
        .try_clone_pinned_directory("<retained-source>".into())
        .unwrap();
    records.verify_tree(&source_dir).unwrap();
    assert_eq!(
        source_dir
            .open_pinned_regular_descendant(Path::new("runtime.yaml"), false)
            .unwrap()
            .unwrap()
            .read_bounded(source_bytes.len() as u64)
            .unwrap(),
        source_bytes
    );
    for ((fd, hash, bytes), descriptor) in projection.record_descriptors().zip(&descriptors[3..]) {
        assert_eq!(fd, descriptor.inherited_descriptor().unwrap());
        let (observed, _) = descriptor.read_regular_file_stable_bounded(bytes).unwrap();
        assert_eq!(observed.len() as u64, bytes);
        assert_eq!(lillux::sha256_hex(&observed), hash);
    }
    let cache = crate::execution::cache::MaterializationCache::new(
        state
            .config
            .runtime_state_dir()
            .join("cache/source-closures"),
    );
    assert!(!cache._evict(records.content_manifest_hash()).unwrap());
    // Keep cloned directory descriptors open: only the owner's retained lease,
    // not merely a directory FD, prevents cache retirement while executing.
    drop(prepared);
    assert!(cache._evict(records.content_manifest_hash()).unwrap());
}

#[test]
#[cfg(target_os = "linux")]
fn direct_input_assembly_refuses_storage_only_source_capsule_before_materialization() {
    use crate::execution::source_closure::tests::external_delivery::{capsule, state_and_source};

    let (_root, state, records, _) = state_and_source();
    let (capsule, _) = capsule(&records);
    // This fixture has genuine signed source records, but its sealed invocation
    // still carries the synthetic managed runtime route. The direct input owner
    // must not skip its production decoder merely because source CAS is valid.
    capsule.validate().unwrap();
    let (protocol, _) = signed_protocol();
    let error = match prepare_external_direct_inputs(&state, &capsule, &protocol, THREAD, CHAIN) {
        Ok(_) => panic!("storage-only source capsule became executable input authority"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}")
            .contains("sealed executor route contradicts admitted capsule executor identity"),
        "unexpected refusal: {error:#}"
    );
    assert!(
        !state
            .config
            .runtime_state_dir()
            .join("cache/source-closures")
            .exists(),
        "invalid sealed invocation caused source materialization"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn direct_inputs_refuse_substituted_protocol_identity_and_interpretation() {
    let (root, state, records, _) =
        crate::execution::source_closure::tests::external_delivery::state_and_source();
    let (capsule, _, protocol) = retained_inputs_capsule(&state, &records, root.path());
    for coordinate in ["ref", "digest", "signer", "descriptor"] {
        let mut changed = protocol.clone();
        match coordinate {
            "ref" => changed.canonical_ref.push_str("-other"),
            "digest" => changed.raw_content_digest = "d".repeat(64),
            "signer" => changed.signer_fingerprint = "c".repeat(64),
            "descriptor" => changed.descriptor.env_injections.clear(),
            _ => unreachable!(),
        }
        let error = match prepare_external_direct_inputs(&state, &capsule, &changed, THREAD, CHAIN)
        {
            Ok(_) => panic!("accepted changed protocol {coordinate}"),
            Err(error) => error,
        };
        let expected = if coordinate == "descriptor" {
            "external direct protocol interpretation changed its retained document"
        } else {
            "external direct protocol differs from its retained authority"
        };
        assert!(
            format!("{error:#}").contains(expected),
            "{coordinate}: {error:#}"
        );
    }
}
