//! Source redemption tests, not launch admission or live guest qualification.
//! Source/kind/protocol bytes are genuinely signed by a disposable fixture key.
//! The retained invocation scaffold reuses the existing storage fixture, so no
//! assertion here claims that a born thread or endpoint has been admitted.

use super::*;
use base64::Engine as _;
use ryeos_state::objects::admitted_launch_capsule::project_sealed_root_exact_program;
use ryeos_state::objects::*;
use ryeos_state::source_verification::VerifiedAdmittedSourceRecords;
use serde_json::json;

fn signed(body: &str) -> (String, lillux::signature::SignatureHeader) {
    let key = lillux::crypto::SigningKey::from_bytes(&[57; 32]);
    let document =
        lillux::signature::sign_content_at(body, &key, "#", None, "2026-09-23T00:00:00Z");
    let header =
        lillux::signature::parse_signature_line(document.lines().next().unwrap(), "#", None)
            .unwrap();
    assert!(lillux::signature::verify_signature(
        &header.content_hash,
        &header.signature_b64,
        &key.verifying_key()
    ));
    (document, header)
}

pub(super) fn source_records() -> (VerifiedAdmittedSourceRecords, Vec<u8>) {
    let (source, owner_signature) = signed(
        "kind: tool\nname: runtime\ncategory: ryeos/development/authoring-environment-production\n",
    );
    let bytes = source.into_bytes();
    let manifest = SourceClosureManifest::new(
        vec![LogicalSourceRoot {
            id: "source".to_owned(),
        }],
        vec![SourceClosureFile {
            root: "source".to_owned(),
            path: "runtime.yaml".to_owned(),
            blob_hash: lillux::sha256_hex(&bytes),
            size: bytes.len() as u64,
            mode: SourceFileMode::ReadOnly,
        }],
    )
    .unwrap();
    let mut binding = directory_binding();
    let (kind_document, kind_signature) = signed("kind: tool\nlocation: {directory: tools}\n");
    binding.owner.root_source_content_digest = lillux::sha256_hex(&bytes);
    binding.owner.root_raw_content_digest = owner_signature.content_hash;
    binding.owner.signer_fingerprint = owner_signature.signer_fingerprint.clone();
    binding.kind_ceiling.schema_body = lillux::signature::strip_signature_lines(&kind_document);
    binding.kind_ceiling.source_content_digest = lillux::sha256_hex(kind_document.as_bytes());
    binding.kind_ceiling.raw_content_digest = kind_signature.content_hash;
    binding.kind_ceiling.signer_fingerprint = kind_signature.signer_fingerprint;
    binding.kind_ceiling.signature_header = kind_document.lines().next().unwrap().to_owned();
    binding.kind_ceiling.normalized_declaration = json!({
        "derived": SOURCE_CLOSURE_DERIVED_KEY, "location": {"type": "item_namespace"},
        "testimony": "owner_signed_files", "max_files": 8, "max_total_bytes": 8192,
        "max_file_bytes": 8192, "max_depth": 8,
    });
    binding.content_manifest_hash = manifest.digest().unwrap();
    binding.testimony = SourceTestimonyProof::OwnerSignedFiles {
        signer_fingerprint: owner_signature.signer_fingerprint,
        file_count: 1,
        entries_digest: lillux::sha256_hex(
            lillux::canonical_json(&json!([{
                "path":"runtime.yaml", "blob_hash":lillux::sha256_hex(&bytes),
                "signer": binding.owner.signer_fingerprint,
                "content_hash": binding.owner.root_raw_content_digest,
            }]))
            .unwrap()
            .as_bytes(),
        ),
    };
    let records = VerifiedAdmittedSourceRecords::from_canonical_bytes(
        &binding.digest().unwrap(),
        &manifest.digest().unwrap(),
        lillux::canonical_json(&binding.to_value().unwrap())
            .unwrap()
            .as_bytes(),
        lillux::canonical_json(&manifest.to_value().unwrap())
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    (records, bytes)
}

pub(crate) fn capsule(
    records: &VerifiedAdmittedSourceRecords,
) -> (
    AdmittedLaunchCapsule,
    ryeos_engine::resolution::ResolutionOutput,
) {
    let projection = EffectiveSourceClosureProjection {
        schema: 1,
        binding_hash: records.binding_hash().to_owned(),
        content_manifest_hash: records.content_manifest_hash().to_owned(),
        owner_key: records.owner_key().to_owned(),
        file_count: records.manifest().totals.file_count,
        total_bytes: records.manifest().totals.total_bytes,
    };
    let mut invocation = serde_json::to_value(
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture(),
    )
    .unwrap();
    invocation["resolution_output"]["composed"]["derived"][SOURCE_CLOSURE_DERIVED_KEY] =
        projection.to_value().unwrap();
    // The resolution/root bytes are retained inputs, not a later source lookup.
    let (_, source) = source_records();
    let source = String::from_utf8(source).unwrap();
    let owner = &records.binding().owner;
    let root = &mut invocation["resolution_output"]["root"];
    root["requested_id"] = json!(owner.canonical_ref);
    root["resolved_ref"] = json!(owner.canonical_ref);
    root["raw_content"] = json!(lillux::signature::strip_signature_lines(&source));
    root["source_content_digest"] = json!(owner.root_source_content_digest);
    root["raw_content_digest"] = json!(owner.root_raw_content_digest);
    root["source_space"] = json!("bundle");
    root["source_root"] = json!({"kind":"bundle","name":"standard"});
    root["trust_class"] = json!("trusted_bundle");
    root["signer_fingerprint"] = json!(owner.signer_fingerprint);
    invocation["resolution_output"]["effective_trust_class"] = json!("trusted_bundle");
    invocation["item_ref"] = json!(owner.canonical_ref);
    invocation["runtime_ref"] = json!("tool:test/runtime");
    invocation["executor_ref"] = json!("tool:test/executor");
    let subject = &mut invocation["verified_subject"];
    subject["canonical_ref"] = json!(owner.canonical_ref);
    subject["kind"] = json!("tool");
    subject["content_hash"] = json!(owner.root_source_content_digest);
    subject["raw_content_digest"] = json!(owner.root_raw_content_digest);
    subject["source_content_b64"] =
        json!(base64::engine::general_purpose::STANDARD.encode(&source));
    let header =
        lillux::signature::parse_signature_line(source.lines().next().unwrap(), "#", None).unwrap();
    subject["signature_header"] = json!({"timestamp": header.timestamp, "content_hash": header.content_hash,
        "signature_b64": header.signature_b64, "signer_fingerprint": header.signer_fingerprint});
    let resolution = serde_json::from_value(invocation["resolution_output"].clone()).unwrap();
    let exact_program = project_sealed_root_exact_program(&invocation).unwrap();
    let (protocol_document, protocol) = signed("protocol: direct\n");
    let execution_plan = json!({"plan_id":"source-redemption-fixture"});
    let capsule = AdmittedLaunchCapsule {
        schema: ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
        kind: "admitted_launch_capsule".to_owned(),
        exact_program_hash: lillux::sha256_hex(
            lillux::canonical_json(&exact_program).unwrap().as_bytes(),
        ),
        exact_program,
        sealed_invocation: invocation,
        project_authority: ExecutionProjectAuthority::PROJECTLESS,
        lifecycle_authority: ExecutionLifecycleAuthority {
            ownership: ExecutionOwnershipAuthority::DaemonOwned,
            recovery: ExecutionRecoveryAuthority::RestartRecoverable,
        },
        launch_driver: ExecutionLaunchDriver::DirectItemExecutor,
        artifact_identity: AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executor_ref: "tool:test/executor".to_owned(),
            root_subject_source_content_digest: owner.root_source_content_digest.clone(),
            root_subject_signer_fingerprint: Some(owner.signer_fingerprint.clone()),
            root_subject_source_identity: DirectRootSourceIdentity::Bundle {
                manifest_hash: "b".repeat(64),
                manifest_signer_fingerprint: owner.signer_fingerprint.clone(),
            },
            protocol_ref: "protocol:test/direct".to_owned(),
            protocol_content_hash: protocol.content_hash,
            protocol_signer_fingerprint: protocol.signer_fingerprint,
            execution_plan_hash: lillux::sha256_hex(
                lillux::canonical_json(&execution_plan).unwrap().as_bytes(),
            ),
            executable_identity: DirectExecutableIdentity::CapturedContent {
                content_hash: "a".repeat(64),
            },
            runtime_identity: DirectRuntimeIdentity {
                runtime_ref: "tool:test/runtime".to_owned(),
                runtime_source_space: DirectRuntimeSourceSpace::Bundle,
                runtime_content_hash: "f".repeat(64),
                runtime_signer_fingerprint: "4".repeat(64),
                runtime_bundle_manifest_hash: Some("1".repeat(64)),
                runtime_bundle_signer_fingerprint: Some("3".repeat(64)),
            },
        },
        execution_closure: AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan,
            protocol_descriptor_document: protocol_document,
            command: AdmittedDirectCommandClosure::ContentAddressed {
                executable_blob_hash: "a".repeat(64),
                execution_path: admitted_direct_command_execution_path(
                    &"a".repeat(64),
                    Path::new("/executor"),
                )
                .unwrap(),
            },
            admitted_project_root: None,
        },
        execution_realization_hash: "9".repeat(64),
        source_binding_hash: Some(records.binding_hash().to_owned()),
        accounting_scope: None,
        effective_caps: Vec::new(),
        parent_delegation_caps: None,
        runtime_ref: "tool:test/runtime".to_owned(),
        executor_ref: "tool:test/executor".to_owned(),
    };
    capsule.validate().unwrap();
    (capsule, resolution)
}

#[cfg(target_os = "linux")]
pub(crate) fn state_and_source() -> (
    tempfile::TempDir,
    ryeos_app::state::AppState,
    VerifiedAdmittedSourceRecords,
    Vec<u8>,
) {
    let root = tempfile::tempdir().unwrap();
    let state = ryeos_app::state::test_support::build(root.path()).unwrap();
    let (records, bytes) = source_records();
    let authority = super::super::super::pinned_state_authority(&state).unwrap();
    let cas = authority.cas_store().unwrap();
    assert_eq!(
        cas.store_blob(&bytes).unwrap(),
        records.manifest().entries[0].blob_hash
    );
    assert_eq!(
        cas.store_object(&records.binding().to_value().unwrap())
            .unwrap(),
        records.binding_hash()
    );
    assert_eq!(
        cas.store_object(&records.manifest().to_value().unwrap())
            .unwrap(),
        records.content_manifest_hash()
    );
    (root, state, records, bytes)
}

#[test]
#[cfg(target_os = "linux")]
fn external_source_delivery_keeps_b_bytes_and_exact_records_after_live_candidate_changes() {
    use ryeos_external_execution_contract::GuestMountContentAuthority;
    let (root, state, records, bytes) = state_and_source();
    let (capsule, mut resolution) = capsule(&records);
    let live = root
        .path()
        .join("live/.ai/tools/ryeos/development/authoring-environment-production");
    let candidate = root
        .path()
        .join("candidate/.ai/tools/ryeos/development/authoring-environment-production");
    // Harness deliberately makes both mutable roots disagree with retained B.
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(&candidate).unwrap();
    std::fs::write(live.join("runtime.yaml"), b"changed live source").unwrap();
    std::fs::write(
        candidate.join("runtime.yaml"),
        b"candidate replacement evaluator",
    )
    .unwrap();
    resolution.root.source_path = candidate.join("runtime.yaml");
    let delivery = bind_external_source(&state, &resolution, &capsule)
        .unwrap()
        .unwrap();
    assert_eq!(delivery.input.authority_id, records.binding_hash());
    assert_eq!(
        delivery.input.destination,
        records.runtime_destination().to_str().unwrap()
    );
    assert_eq!(
        delivery.input.descriptor,
        delivery.directory.inherited_descriptor().unwrap()
    );
    let GuestMountContentAuthority::SourceClosure {
        binding_descriptor,
        manifest_descriptor,
        binding_bytes,
        manifest_bytes,
        binding_hash,
        manifest_hash,
    } = &delivery.input.content_authority
    else {
        panic!("lost source authority")
    };
    assert_eq!(
        *binding_descriptor,
        delivery.content_records[0].inherited_descriptor().unwrap()
    );
    assert_eq!(
        *manifest_descriptor,
        delivery.content_records[1].inherited_descriptor().unwrap()
    );
    assert_eq!(binding_hash, records.binding_hash());
    assert_eq!(manifest_hash, records.content_manifest_hash());
    for (descriptor, bound, expected) in [
        (
            &delivery.content_records[0],
            *binding_bytes,
            records.binding().to_value().unwrap(),
        ),
        (
            &delivery.content_records[1],
            *manifest_bytes,
            records.manifest().to_value().unwrap(),
        ),
    ] {
        let (observed, _) = descriptor.read_regular_file_stable_bounded(bound).unwrap();
        assert_eq!(
            observed,
            lillux::canonical_json(&expected).unwrap().as_bytes()
        );
        assert_eq!(observed.len() as u64, bound);
    }
    let source = delivery
        .lifeline
        .source_directory()
        .open_pinned_regular_descendant(Path::new("runtime.yaml"), false)
        .unwrap()
        .unwrap();
    assert_eq!(source.read_bounded(bytes.len() as u64).unwrap(), bytes);
    records
        .verify_tree(delivery.lifeline.source_directory())
        .unwrap();
    assert_eq!(
        std::fs::read(candidate.join("runtime.yaml")).unwrap(),
        b"candidate replacement evaluator"
    );
    assert_eq!(
        std::fs::read(live.join("runtime.yaml")).unwrap(),
        b"changed live source"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn external_source_delivery_refuses_capsule_resolution_disagreement_before_materialization() {
    let (_root, state, records, _) = state_and_source();
    let (capsule, resolution) = capsule(&records);
    let mut wrong_resolution = resolution.clone();
    wrong_resolution
        .composed
        .derived
        .get_mut(SOURCE_CLOSURE_DERIVED_KEY)
        .unwrap()["owner_key"] = json!("f".repeat(64));
    assert!(
        bind_external_source(&state, &wrong_resolution, &capsule)
            .err()
            .unwrap()
            .to_string()
            .contains("differs from the retained launch capsule")
    );
    let mut missing = resolution.clone();
    missing.composed.derived.remove(SOURCE_CLOSURE_DERIVED_KEY);
    assert!(bind_external_source(&state, &missing, &capsule).is_err());
    let mut wrong_capsule = capsule;
    wrong_capsule.source_binding_hash = Some("f".repeat(64));
    assert!(bind_external_source(&state, &resolution, &wrong_capsule).is_err());
    assert!(
        !state
            .config
            .runtime_state_dir()
            .join("cache/source-closures")
            .exists()
    );
}

#[test]
#[cfg(target_os = "linux")]
fn external_source_delivery_retains_original_generation_lease_until_drop() {
    let (_root, state, records, _) = state_and_source();
    let (capsule, resolution) = capsule(&records);
    let delivery = bind_external_source(&state, &resolution, &capsule)
        .unwrap()
        .unwrap();
    let cache = super::super::super::cache::MaterializationCache::new(
        state
            .config
            .runtime_state_dir()
            .join("cache/source-closures"),
    );
    assert_eq!(delivery.lifeline._leases.len(), 1);
    assert!(!cache._evict(records.content_manifest_hash()).unwrap());
    records
        .verify_tree(delivery.lifeline.source_directory())
        .unwrap();
    drop(delivery);
    assert!(cache._evict(records.content_manifest_hash()).unwrap());
    assert!(!cache.cache_dir(records.content_manifest_hash()).exists());
}
