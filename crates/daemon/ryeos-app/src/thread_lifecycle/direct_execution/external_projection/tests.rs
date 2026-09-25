use super::compile_external_direct_program_parts as compile_external_direct_program;
use super::*;
use ryeos_engine::contracts::{ExecutionPlan, PlanStdin};
use ryeos_state::objects::{
    AdmittedDirectCommandClosure, DirectExecutableIdentity, DirectRootSourceIdentity,
    DirectRuntimeIdentity, DirectRuntimeSourceSpace, ExternalContentMountRoot,
};
use serde_json::json;
use std::path::PathBuf;

const PROJECT: &str = "/controller/project";
const COMMAND: &str = "/ryeos/admitted-project/vendor/runtime/bin/check";
const GUEST_COMMAND: &str = "/workspace/vendor/runtime/bin/check";
const THREAD: &str = "T-direct-projection-fixture";
const CHAIN: &str = "T-direct-projection-root";

// Content/storage fixture only; this deliberately cannot manufacture the
// opaque full-capsule compiler token or born-thread admission.
pub(super) fn program_fixture() -> AdmittedExternalDirectProgram {
    Fixture::new().compile().unwrap()
}

#[test]
fn compiled_direct_token_requires_exact_reservation_owner_tuple() {
    use crate::runtime_db::external_execution::{
        ExternalAllocationOwner, ExternalDedicatedSessionOwner,
    };

    let directory = tempfile::tempdir().unwrap();
    let db = crate::runtime_db::RuntimeDb::open(&directory.path().join("runtime.sqlite3")).unwrap();
    let mut reservation =
        crate::runtime_db::external_execution::tests::direct_owner_reservation(&db);
    let exact_program = Fixture::new().compile().unwrap();
    let ExternalAllocationOwner::DirectThread {
        chain_root_id,
        launch_owner,
        program,
    } = &mut reservation.owner
    else {
        unreachable!()
    };
    *program = exact_program;
    // Child-module construction tests only the opaque token's exact tuple
    // comparison, not full compiler/capsule/born-thread admission. The token
    // receives neither Clone nor Deserialize for this test.
    let token = CompiledExternalDirectProgram {
        program: program.clone(),
        capsule_hash: reservation.admitted_capsule_hash.clone(),
        thread_id: reservation.placement_thread_id.clone(),
        chain_root_id: chain_root_id.clone(),
        launch_owner: launch_owner.clone(),
    };
    token.verify_reservation(&reservation).unwrap();

    for coordinate in [
        "epoch",
        "daemon",
        "nonce",
        "thread",
        "claim_thread",
        "chain",
        "capsule",
        "program",
    ] {
        let mut changed = reservation.clone();
        let ExternalAllocationOwner::DirectThread {
            chain_root_id,
            launch_owner,
            program,
        } = &mut changed.owner
        else {
            unreachable!()
        };
        match coordinate {
            "epoch" => launch_owner.monotonic_launch_epoch += 1,
            "daemon" => launch_owner.daemon_generation_id.push_str("-other"),
            "nonce" => launch_owner.unpredictable_nonce.push_str("-other"),
            "thread" => changed.placement_thread_id.push_str("-other"),
            "claim_thread" => launch_owner.thread_id.push_str("-other"),
            "chain" => chain_root_id.push_str("-other"),
            "capsule" => changed.admitted_capsule_hash = "f".repeat(64),
            "program" => {
                let mut fixture = Fixture::new();
                fixture.spec().timeout_secs += 1;
                *program = fixture.compile().unwrap();
                assert_ne!(&*program, token.program());
            }
            _ => unreachable!(),
        }
        assert!(
            token.verify_reservation(&changed).is_err(),
            "compiler token accepted substituted {coordinate}"
        );
    }
    let mut session = reservation;
    session.owner = ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
        workspace_id: "workspace-other".into(),
        worker_instance_id: "worker-other".into(),
        worker_boot_epoch: 1,
    });
    assert!(token.verify_reservation(&session).is_err());
}

// These are authority-join unit tests, not a fabricated authenticated capsule
// or born-thread allocation. Parts-level B-source independence tests below
// intentionally remain independent of C's launch identity.
fn immutable_project_authority(snapshot_hash: String) -> ExecutionProjectAuthority {
    ExecutionProjectAuthority::pinned(
        "direct-projection-project".into(),
        None,
        snapshot_hash,
        PinnedProjectRealization::ReadOnly,
        EnvironmentAuthority::None,
        Vec::new(),
    )
    .unwrap()
}

#[test]
fn direct_project_inputs_join_exact_immutable_snapshot_and_refuse_substitution() {
    let inputs = Fixture::new().inputs;
    // Read-only authority requires its lineage base and operational generation
    // to coincide. Substitute only the independently supplied guest snapshot.
    let authority = immutable_project_authority(inputs.base_snapshot.snapshot_hash.clone());
    validate_external_direct_project_inputs(&authority, &inputs).unwrap();
    let mut substituted = inputs;
    substituted.base_snapshot.snapshot_hash = "9".repeat(64);
    let error = validate_external_direct_project_inputs(&authority, &substituted).unwrap_err();
    assert!(error.to_string().contains("snapshot differs"));
}

#[test]
fn direct_project_inputs_refuse_projectless_and_writable_generations() {
    let inputs = Fixture::new().inputs;
    assert!(
        validate_external_direct_project_inputs(&ExecutionProjectAuthority::PROJECTLESS, &inputs)
            .is_err()
    );
    let mut authority = immutable_project_authority(inputs.base_snapshot.snapshot_hash.clone());
    let ExecutionProjectAuthority::PinnedGeneration { realization, .. } = &mut authority else {
        unreachable!()
    };
    *realization = PinnedProjectRealization::Cow {
        terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
    };
    authority.validate().unwrap();
    assert!(validate_external_direct_project_inputs(&authority, &inputs).is_err());
}

#[test]
fn direct_project_inputs_refuse_environment_authority() {
    let inputs = Fixture::new().inputs;
    let mut authority = immutable_project_authority(inputs.base_snapshot.snapshot_hash.clone());
    let ExecutionProjectAuthority::PinnedGeneration { environment, .. } = &mut authority else {
        unreachable!()
    };
    *environment = EnvironmentAuthority::Vault {
        namespace: "operator".into(),
        name_authority: ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
    };
    authority.validate().unwrap();
    assert!(validate_external_direct_project_inputs(&authority, &inputs).is_err());
}

#[test]
fn direct_project_inputs_refuse_unadmitted_workspace_output_contract() {
    let mut inputs = Fixture::new().inputs;
    let authority = immutable_project_authority(inputs.base_snapshot.snapshot_hash.clone());
    inputs.workspace_outputs = Some(
        ryeos_external_execution_contract::GuestWorkspaceOutputAuthorityInput {
            descriptor: 12,
            authority_hash: "a".repeat(64),
            bytes: 128,
            producer_chain_root_id: CHAIN.into(),
            producer_thread_id: THREAD.into(),
            admitted_launch_capsule_hash: "b".repeat(64),
        },
    );
    inputs.validate().unwrap();
    let error = validate_external_direct_project_inputs(&authority, &inputs).unwrap_err();
    assert!(error.to_string().contains("workspace output authority"));
}

struct Fixture {
    plan: ExecutionPlan,
    protocol: VerifiedProtocol,
    inputs: ExternalGuestInputProjection,
}

#[test]
fn ordinary_direct_preflight_two_node_plan_reaches_endpoint_admission() {
    let root = tempfile::tempdir().unwrap();
    let state = crate::state::test_support::build(root.path()).unwrap();
    let fixture = Fixture::new();
    assert!(state.node_config.external_execution.is_empty());
    let prepared = super::super::PreparedItemPlan {
        timeout_secs: 30,
        plan: fixture.plan,
        root_subject_source_identity: ryeos_state::objects::DirectRootSourceIdentity::Project,
        admitted_command: None,
        realization_command: None,
    };
    // This is prebirth refusal evidence, not a born-program or launch fixture.
    // The real AppState endpoint resolver must run instead of a topology panic.
    let error = prepared
        .preflight_external_execution(
            &state,
            &fixture.protocol,
            &immutable_project_authority("1".repeat(64)),
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "signed direct execution endpoint is not installed"
    );
}

#[test]
fn ordinary_direct_topology_requires_subprocess_then_unique_complete() {
    let fixture = Fixture::new();
    validate_external_direct_plan(&fixture.plan, &fixture.protocol.descriptor).unwrap();
    for mutation in [
        "missing_complete",
        "extra_subprocess",
        "extra_complete",
        "reordered",
        "duplicate_ids",
        "complete_entrypoint",
        "unknown_entrypoint",
        "missing_subprocess",
    ] {
        let mut plan = fixture.plan.clone();
        match mutation {
            "missing_complete" => {
                plan.nodes.pop();
            }
            "extra_subprocess" => plan.nodes.insert(1, plan.nodes[0].clone()),
            "extra_complete" => plan.nodes.push(plan.nodes[1].clone()),
            "reordered" => plan.nodes.swap(0, 1),
            "duplicate_ids" => {
                let id = plan.nodes[0].id().clone();
                plan.nodes[1] = PlanNode::Complete { id };
            }
            "complete_entrypoint" => plan.entrypoint = plan.nodes[1].id().clone(),
            "unknown_entrypoint" => {
                plan.entrypoint = ryeos_engine::contracts::PlanNodeId("missing".into());
            }
            "missing_subprocess" => {
                plan.nodes.remove(0);
            }
            _ => unreachable!(),
        }
        assert!(
            validate_external_direct_plan(&plan, &fixture.protocol.descriptor).is_err(),
            "accepted {mutation}"
        );
    }
}

impl Fixture {
    fn new() -> Self {
        let mut plan = super::super::tests::portable_direct_plan(Path::new(PROJECT));
        plan.filesystem_authority_ceiling = IsolationFilesystemAuthorityCeiling::CapturedExecution;
        plan.network_authority_ceiling = IsolationNetworkAuthorityCeiling::Isolated;
        plan.target_requirement = Some(
            serde_json::from_value(json!({
                "os":"linux", "arch":"x86_64", "resources":[]
            }))
            .unwrap(),
        );
        plan.endpoint_requirement = serde_json::from_value(json!({
            "kind":"external", "binding_id":"test-direct", "stdout_max_bytes":4096,
            "stderr_max_bytes":2048
        }))
        .unwrap();
        plan.external_endpoint_binding = Some(
            serde_json::from_value(json!({
                "binding_id":"test-direct", "binding_digest":"e".repeat(64)
            }))
            .unwrap(),
        );
        plan.runtime_identity = Some(
            serde_json::from_value(json!({
                "runtime_ref":"runtime:test/direct", "runtime_source_space":"project",
                "runtime_content_hash":"b".repeat(64), "runtime_signer_fingerprint":"c".repeat(64),
                "runtime_bundle_manifest_hash":null, "runtime_bundle_signer_fingerprint":null
            }))
            .unwrap(),
        );
        let PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0] else {
            unreachable!()
        };
        spec.cmd = COMMAND.into();
        spec.verified_command = Some(serde_json::from_value(json!({
            "authority":"captured_content", "code":{"source_path":COMMAND,"content_hash":"a".repeat(64)}
        })).unwrap());
        spec.timeout_secs = 30;
        spec.env.clear();
        spec.env
            .insert("FARM_DATA".into(), format!("{PROJECT}/data.json"));
        let descriptor = serde_yaml::from_str("kind: protocol\nname: direct\ncategory: test\nabi_version: v1\nstdin: {shape: opaque}\nstdout: {shape: opaque_bytes, mode: terminal}\nenv_injections:\n  - {name: RYE_THREAD_ID, source: thread_id}\n  - {name: RYE_PROJECT_PATH, source: project_path}\ncapabilities: {allows_pushed_head: false, allows_target_site: false, allows_detached: false}\nlifecycle: {mode: managed}\ncallback_channel: none\n").unwrap();
        let protocol = VerifiedProtocol {
            canonical_ref: "protocol:test/direct".into(),
            raw_content_digest: String::new(),
            signer_fingerprint: String::new(),
            descriptor,
            trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            bundle_root: PathBuf::from("/fixture/bundle"),
            descriptor_path: PathBuf::from("/fixture/protocol.yaml"),
        };
        let inputs = serde_json::from_value(json!({
            "schema":4, "base_snapshot":{"descriptor":3,"snapshot_hash":"1".repeat(64),
                "closure_digest":"2".repeat(64),"object_count":3,"blob_count":1,"total_bytes":1024},
            "workspace_outputs":null,
            "inputs":[{"role":"product","authority_id":"runtime","descriptor":4,
                "destination":"/workspace/vendor/runtime", "kind":"directory", "access":"read_only",
                "normalized_mode":null, "bytes":1024,
                "content_authority":{"kind":"product_manifest","manifest_kind":"large_content",
                    "manifest_hash":"d".repeat(64),"manifest_descriptor":5,"manifest_bytes":128}}],
            "executable_search":[],
            "environment":{"FARM_DATA":"/workspace/data.json","RYE_THREAD_ID":THREAD,"RYE_PROJECT_PATH":"/workspace",
                "RYEOS_THREAD_ID":THREAD,"RYEOS_CHAIN_ROOT_ID":CHAIN}
        })).unwrap();
        Self {
            plan,
            protocol,
            inputs,
        }
    }

    fn spec(&mut self) -> &mut ryeos_engine::contracts::PlanSubprocessSpec {
        let PlanNode::DispatchSubprocess { spec, .. } = &mut self.plan.nodes[0] else {
            unreachable!()
        };
        spec
    }

    fn seal(&mut self) -> (AdmittedExecutionClosure, AdmittedLaunchArtifactIdentity) {
        let body = serde_yaml::to_string(&self.protocol.descriptor).unwrap();
        let key = lillux::crypto::SigningKey::from_bytes(&[73; 32]);
        self.protocol.signer_fingerprint =
            lillux::signature::compute_fingerprint(&key.verifying_key());
        self.protocol.raw_content_digest = lillux::signature::content_hash(&body);
        let execution_plan = serde_json::to_value(&self.plan).unwrap();
        let artifact = AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executor_ref: "tool:test/runtime".into(),
            root_subject_source_content_digest: "f".repeat(64),
            root_subject_signer_fingerprint: Some("c".repeat(64)),
            root_subject_source_identity: DirectRootSourceIdentity::Project,
            protocol_ref: self.protocol.canonical_ref.clone(),
            protocol_content_hash: self.protocol.raw_content_digest.clone(),
            protocol_signer_fingerprint: self.protocol.signer_fingerprint.clone(),
            execution_plan_hash: lillux::sha256_hex(
                lillux::canonical_json(&execution_plan).unwrap().as_bytes(),
            ),
            executable_identity: DirectExecutableIdentity::CapturedContent {
                content_hash: "a".repeat(64),
            },
            runtime_identity: DirectRuntimeIdentity {
                runtime_ref: "runtime:test/direct".into(),
                runtime_source_space: DirectRuntimeSourceSpace::Project,
                runtime_content_hash: "b".repeat(64),
                runtime_signer_fingerprint: "c".repeat(64),
                runtime_bundle_manifest_hash: None,
                runtime_bundle_signer_fingerprint: None,
            },
        };
        let closure = AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan,
            admitted_project_root: Some(PathBuf::from(PROJECT)),
            protocol_descriptor_document: lillux::signature::sign_content_at(
                &body,
                &key,
                "#",
                None,
                "2026-09-23T00:00:00Z",
            ),
            command: AdmittedDirectCommandClosure::RealizationMember {
                executable_blob_hash: "a".repeat(64),
                realization_id: "runtime".into(),
                realization_manifest_hash: "d".repeat(64),
                realization_mount_root: ExternalContentMountRoot::Project,
                realization_mount: "vendor/runtime".into(),
                relative_path: "bin/check".into(),
                execution_path: PathBuf::from(COMMAND),
            },
        };
        (closure, artifact)
    }

    fn compile(&mut self) -> Result<AdmittedExternalDirectProgram> {
        let (closure, artifact) = self.seal();
        compile_external_direct_program(
            &closure,
            &artifact,
            &self.protocol,
            THREAD,
            CHAIN,
            &self.inputs,
            None,
        )
    }

    fn with_source(records: &VerifiedAdmittedSourceRecords) -> Self {
        use ryeos_external_execution_contract::{
            GuestMountAccess, GuestMountInput, GuestMountKind,
        };
        let mut fixture = Self::new();
        fixture.spec().args = vec![PlanArgument::AdmittedSourceEntry];
        fixture.inputs.inputs.push(GuestMountInput {
            role: GuestMountRole::Source,
            authority_id: records.binding_hash().into(),
            descriptor: 6,
            destination: records.runtime_destination().to_str().unwrap().into(),
            kind: GuestMountKind::Directory,
            access: GuestMountAccess::ReadOnly,
            normalized_mode: None,
            content_authority: GuestMountContentAuthority::SourceClosure {
                binding_hash: records.binding_hash().into(),
                binding_descriptor: 7,
                binding_bytes: lillux::canonical_json(&records.binding().to_value().unwrap())
                    .unwrap()
                    .len() as u64,
                manifest_hash: records.content_manifest_hash().into(),
                manifest_descriptor: 8,
                manifest_bytes: lillux::canonical_json(&records.manifest().to_value().unwrap())
                    .unwrap()
                    .len() as u64,
            },
            bytes: records.manifest().totals.total_bytes,
        });
        fixture.inputs.environment.insert(
            "RYEOS_ADMITTED_SOURCE".into(),
            records.sealed_identity_env().into(),
        );
        fixture
    }

    fn compile_with_source(
        &mut self,
        records: &VerifiedAdmittedSourceRecords,
    ) -> Result<AdmittedExternalDirectProgram> {
        let (closure, artifact) = self.seal();
        compile_external_direct_program(
            &closure,
            &artifact,
            &self.protocol,
            THREAD,
            CHAIN,
            &self.inputs,
            Some(records),
        )
    }
}

/// Test-only admitted source records. No production fixture or new attestation:
/// callers still supply their expected identities from the retained capsule.
fn source_records(root_digest: &str) -> VerifiedAdmittedSourceRecords {
    use ryeos_state::objects::*;
    let manifest = SourceClosureManifest::new(
        vec![LogicalSourceRoot {
            id: "source".into(),
        }],
        vec![SourceClosureFile {
            root: "source".into(),
            path: "run.py".into(),
            blob_hash: lillux::sha256_hex(b"run"),
            size: 3,
            mode: SourceFileMode::ReadOnly,
        }],
    )
    .unwrap();
    let schema_body = "kind: kind\n".to_owned();
    let binding = EffectiveSourceBinding {
        schema: EFFECTIVE_SOURCE_BINDING_SCHEMA,
        kind: EFFECTIVE_SOURCE_BINDING_KIND.into(),
        owner: SourceOwnerIdentity {
            canonical_ref: "tool:test/run".into(),
            item_kind: "tool".into(),
            source_space: SourceSpaceIdentity::Project,
            source_root: SourceRootIdentity::Project,
            root_source_content_digest: root_digest.into(),
            root_raw_content_digest: "b".repeat(64),
            signer_fingerprint: "c".repeat(64),
            logical_item_key: "test/run".into(),
        },
        kind_ceiling: SignedKindSourceCeiling {
            schema_ref: "kind:tool".into(),
            source_content_digest: "d".repeat(64),
            raw_content_digest: lillux::signature::content_hash(&schema_body),
            signer_fingerprint: "f".repeat(64),
            signature_header: "signed".into(),
            schema_body,
            schema_document: json!({"kind":"kind", "location":{"directory":"tools"}}),
            normalized_declaration: json!({"derived":SOURCE_CLOSURE_DERIVED_KEY,
                "location":{"type":"item_namespace"}, "testimony":"owner_signed_files",
                "max_files":8, "max_total_bytes":1024, "max_file_bytes":512, "max_depth":8}),
            root_kind_format: json!({"extensions":["yaml"]}),
            root_signature_envelope: json!({"style":"header"}),
        },
        content_manifest_hash: manifest.digest().unwrap(),
        testimony: SourceTestimonyProof::OwnerSignedFiles {
            signer_fingerprint: "c".repeat(64),
            file_count: 1,
            entries_digest: "2".repeat(64),
        },
        execution_policy: SourceExecutionPolicyIdentity::Executor {
            declarer_ref: "tool:ryeos/core/runtimes/python/function".into(),
            signer_fingerprint: "3".repeat(64),
            source_content_digest: "4".repeat(64),
            raw_content_digest: "5".repeat(64),
            policy_digest: "6".repeat(64),
            chain_digest: "7".repeat(64),
        },
        logical_binding: SourceLogicalBinding::Tool {
            loader_roots: vec![SourceLoaderRoot::ItemDirectory],
            root_entry: "run.py".into(),
        },
    };
    VerifiedAdmittedSourceRecords::from_canonical_bytes(
        &binding.digest().unwrap(),
        &manifest.digest().unwrap(),
        lillux::canonical_json(&binding.to_value().unwrap())
            .unwrap()
            .as_bytes(),
        lillux::canonical_json(&manifest.to_value().unwrap())
            .unwrap()
            .as_bytes(),
    )
    .unwrap()
}

#[test]
fn external_direct_source_entry_uses_exact_b_runtime_mount_independent_of_candidate() {
    let records = source_records(&"a".repeat(64));
    let mut fixture = Fixture::with_source(&records);
    let first = fixture.compile_with_source(&records).unwrap();
    let expected = Path::new(ryeos_state::objects::EXECUTION_RUNTIME_REALIZATIONS_ROOT)
        .join("source-closures")
        .join(records.binding_hash())
        .join("run.py");
    assert_eq!(first.projection().arguments, [expected.to_str().unwrap()]);
    assert!(!expected.starts_with("/workspace"));
    assert_eq!(first.projection().cwd, "/workspace");
    let environment: serde_json::Value =
        serde_json::from_str(&first.projection().environment["RYEOS_ADMITTED_SOURCE"]).unwrap();
    assert_eq!(
        environment,
        json!({"schema":1,"binding_hash":records.binding_hash(),
        "content_manifest_hash":records.content_manifest_hash(),"owner_key":records.owner_key()})
    );
    assert_eq!(environment.as_object().unwrap().len(), 4);
    fixture.inputs.base_snapshot.snapshot_hash = "9".repeat(64);
    fixture.inputs.base_snapshot.closure_digest = "8".repeat(64);
    let changed_candidate = fixture.compile_with_source(&records).unwrap();
    assert_eq!(
        changed_candidate.projection().arguments,
        first.projection().arguments
    );
    assert_eq!(
        changed_candidate.projection().environment["RYEOS_ADMITTED_SOURCE"],
        first.projection().environment["RYEOS_ADMITTED_SOURCE"]
    );
    assert_ne!(changed_candidate.digest().unwrap(), first.digest().unwrap());
    let different_source = source_records(&"e".repeat(64));
    assert_eq!(
        different_source.content_manifest_hash(),
        records.content_manifest_hash()
    );
    let second = Fixture::with_source(&different_source)
        .compile_with_source(&different_source)
        .unwrap();
    assert_ne!(second.projection().arguments, first.projection().arguments);
    assert_ne!(second.digest().unwrap(), first.digest().unwrap());
}

#[test]
fn external_direct_source_requires_exactly_one_typed_entry_and_retained_mount() {
    let records = source_records(&"a".repeat(64));
    let mut unbound = Fixture::with_source(&records);
    assert!(
        unbound
            .compile()
            .unwrap_err()
            .to_string()
            .contains("unbound source")
    );
    let mut missing = Fixture::with_source(&records);
    missing.inputs.inputs.pop();
    assert!(
        missing
            .compile_with_source(&records)
            .unwrap_err()
            .to_string()
            .contains("exactly one retained mount")
    );
    let mut duplicate = Fixture::with_source(&records);
    let mut mount = duplicate.inputs.inputs[1].clone();
    mount.authority_id = "other-source".into();
    mount.destination = "/other-source".into();
    mount.descriptor = 9;
    if let GuestMountContentAuthority::SourceClosure {
        binding_descriptor,
        manifest_descriptor,
        ..
    } = &mut mount.content_authority
    {
        *binding_descriptor = 10;
        *manifest_descriptor = 11;
    }
    duplicate.inputs.inputs.push(mount);
    duplicate.inputs.validate().unwrap();
    assert!(
        duplicate
            .compile_with_source(&records)
            .unwrap_err()
            .to_string()
            .contains("exactly one retained mount")
    );
    for args in [
        vec![],
        vec![
            PlanArgument::AdmittedSourceEntry,
            PlanArgument::AdmittedSourceEntry,
        ],
        vec![PlanArgument::Literal {
            value: records.runtime_entry_path().to_str().unwrap().into(),
        }],
    ] {
        let mut fixture = Fixture::with_source(&records);
        fixture.spec().args = args;
        assert!(
            fixture
                .compile_with_source(&records)
                .unwrap_err()
                .to_string()
                .contains("one typed entry")
        );
    }
}

#[test]
fn external_direct_source_mount_refuses_any_retained_identity_or_destination_substitution() {
    let records = source_records(&"a".repeat(64));
    for (field, value) in [
        ("authority_id", json!("substituted")),
        ("destination", json!("/workspace/.ai/tools/test")),
        ("bytes", json!(4)),
    ] {
        let mut fixture = Fixture::with_source(&records);
        let mut mount = serde_json::to_value(&fixture.inputs.inputs[1]).unwrap();
        mount[field] = value;
        fixture.inputs.inputs[1] = serde_json::from_value(mount).unwrap();
        fixture.inputs.validate().unwrap();
        assert!(
            fixture
                .compile_with_source(&records)
                .unwrap_err()
                .to_string()
                .contains("differs from retained B authority"),
            "{field}"
        );
    }
    for (field, value) in [
        ("binding_hash", json!("0".repeat(64))),
        ("manifest_hash", json!("0".repeat(64))),
        ("binding_bytes", json!(1)),
        ("manifest_bytes", json!(1)),
    ] {
        let mut fixture = Fixture::with_source(&records);
        let mut authority =
            serde_json::to_value(&fixture.inputs.inputs[1].content_authority).unwrap();
        authority[field] = value;
        fixture.inputs.inputs[1].content_authority = serde_json::from_value(authority).unwrap();
        fixture.inputs.validate().unwrap();
        assert!(
            fixture
                .compile_with_source(&records)
                .unwrap_err()
                .to_string()
                .contains("differs from retained B authority"),
            "{field}"
        );
    }
    let mut fixture = Fixture::with_source(&records);
    let wrong = source_records(&"e".repeat(64));
    assert!(
        fixture
            .compile_with_source(&wrong)
            .unwrap_err()
            .to_string()
            .contains("differs from retained B authority")
    );
}

#[test]
fn external_direct_source_identity_environment_is_protected_not_authored() {
    let records = source_records(&"a".repeat(64));
    for provenance in [
        ryeos_engine::contracts::RuntimeEnvSource::RuntimeDescriptor,
        ryeos_engine::contracts::RuntimeEnvSource::EnginePlan,
    ] {
        let mut fixture = Fixture::with_source(&records);
        fixture.spec().env.insert(
            "RYEOS_ADMITTED_SOURCE".into(),
            records.sealed_identity_env().into(),
        );
        fixture
            .spec()
            .env_sources
            .insert("RYEOS_ADMITTED_SOURCE".into(), provenance);
        // Even identical bytes cannot upgrade author-written environment to
        // the independently verified B-owned source authority.
        assert!(fixture.compile_with_source(&records).is_err());
    }
    let mut altered = Fixture::with_source(&records);
    altered
        .inputs
        .environment
        .insert("RYEOS_ADMITTED_SOURCE".into(), "{}".into());
    assert!(
        altered
            .compile_with_source(&records)
            .unwrap_err()
            .to_string()
            .contains("guest environment differs")
    );
}

#[test]
fn external_closure_capture_is_portable_and_rechecks_final_authority() {
    // Closure-construction test, not opaque compiler/born allocation evidence.
    // Member content verification itself is exercised by the current-schema
    // realization_identity tests in the owning direct-plan module.
    let root = tempfile::tempdir().unwrap();
    let (store, _) = super::super::tests::realization_identity_fixture(root.path());
    let authority = store.pinned_state_authority().unwrap();
    let cas = authority.cas_store().unwrap();
    let policy = ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring();
    for mutation in [
        "none",
        "local",
        "mutable",
        "callback",
        "protocol_projection",
        "unsupported",
        "command",
        "network",
        "missing_project",
    ] {
        let mut fixture = Fixture::new();
        let (closure, _) = fixture.seal();
        let AdmittedExecutionClosure::DirectItemExecutor {
            protocol_descriptor_document,
            ..
        } = closure
        else {
            unreachable!()
        };
        fixture.protocol.descriptor_path = root.path().join("protocol.yaml");
        std::fs::write(
            &fixture.protocol.descriptor_path,
            protocol_descriptor_document,
        )
        .unwrap();
        let key = lillux::crypto::SigningKey::from_bytes(&[73; 32]);
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: fixture.protocol.signer_fingerprint.clone(),
                verifying_key: key.verifying_key(),
                label: None,
            },
        ]);
        let mut project = immutable_project_authority("1".repeat(64));
        let mut prepared = super::super::PreparedItemPlan {
            timeout_secs: 30,
            plan: fixture.plan,
            root_subject_source_identity: DirectRootSourceIdentity::Project,
            admitted_command: None,
            realization_command: Some(super::super::PreparedRealizationCommand {
                realization_id: "runtime".into(),
                manifest_hash: "d".repeat(64),
                mount_root: ExternalContentMountRoot::Project,
                mount: "vendor/runtime".into(),
                relative_path: "bin/check".into(),
                executable_blob_hash: "a".repeat(64),
            }),
        };
        match mutation {
            "local" => {
                prepared.plan.endpoint_requirement = ExecutionEndpointRequirement::Local {};
                prepared.plan.external_endpoint_binding = None;
            }
            "mutable" => {
                let ExecutionProjectAuthority::PinnedGeneration { realization, .. } = &mut project
                else {
                    unreachable!()
                };
                *realization = PinnedProjectRealization::Cow {
                    terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
                };
            }
            "callback" => {
                fixture.protocol.descriptor.env_injections[0].source =
                    EnvInjectionSource::ThreadAuthToken;
            }
            "protocol_projection" => {
                // Still allowed by external-plan preflight, but not the
                // retained signed descriptor. Exercise the final exact join.
                fixture.protocol.descriptor.env_injections[0].name = "ALTERED_THREAD_ID".into();
            }
            "unsupported" => prepared.realization_command = None,
            "command" => {
                let PlanNode::DispatchSubprocess { spec, .. } = &mut prepared.plan.nodes[0] else {
                    unreachable!()
                };
                spec.cmd = "/changed/member".into();
            }
            "network" => prepared.plan.capabilities.requires_network = true,
            _ => {}
        }
        let result = prepared.admit_execution_closure(
            &cas,
            &policy,
            &fixture.protocol,
            &trust,
            Some(Path::new(PROJECT)),
            if mutation == "missing_project" {
                None
            } else {
                Some(&project)
            },
        );
        if mutation == "none" {
            let closure = result.unwrap();
            let AdmittedExecutionClosure::DirectItemExecutor { command, .. } = closure else {
                unreachable!()
            };
            assert!(
                matches!(command, AdmittedDirectCommandClosure::RealizationMember { execution_path, .. } if execution_path == Path::new(COMMAND))
            );
        } else if mutation == "protocol_projection" {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("external closure protocol projection changed its signed descriptor")
            );
        } else {
            assert!(result.is_err(), "accepted {mutation}");
        }
        assert!(prepared.admitted_command.is_none());
    }
}

#[test]
fn endpoint_classification_allows_local_node_policy_without_weakening_external() {
    let mut fixture = Fixture::new();
    fixture.plan.endpoint_requirement = ExecutionEndpointRequirement::Local {};
    fixture.plan.external_endpoint_binding = None;
    let (mut closure, mut artifact) = fixture.seal();
    let AdmittedExecutionClosure::DirectItemExecutor { command, .. } = &mut closure else {
        unreachable!()
    };
    *command = AdmittedDirectCommandClosure::NodePolicy;
    let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executable_identity,
        ..
    } = &mut artifact
    else {
        unreachable!()
    };
    *executable_identity = DirectExecutableIdentity::NodePolicy;
    assert!(!closure_requires_external_direct(&closure, &artifact).unwrap());
    assert!(super::super::decode_retained_direct_plan(&closure, &artifact).is_err());

    let mut external = Fixture::new();
    let (mut external_closure, mut external_artifact) = external.seal();
    assert!(closure_requires_external_direct(&external_closure, &external_artifact).unwrap());
    let AdmittedExecutionClosure::DirectItemExecutor { command, .. } = &mut external_closure else {
        unreachable!()
    };
    *command = AdmittedDirectCommandClosure::NodePolicy;
    let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executable_identity,
        ..
    } = &mut external_artifact
    else {
        unreachable!()
    };
    *executable_identity = DirectExecutableIdentity::NodePolicy;
    assert!(closure_requires_external_direct(&external_closure, &external_artifact).is_err());

    let AdmittedExecutionClosure::DirectItemExecutor { execution_plan, .. } = &mut closure else {
        unreachable!()
    };
    execution_plan["endpoint_requirement"] = serde_json::json!({"kind":"external", "binding_id":"forged",
        "stdout_max_bytes":1024,"stderr_max_bytes":1024});
    assert!(closure_requires_external_direct(&closure, &artifact).is_err());
}

#[test]
fn external_direct_projection_preserves_exact_closure_and_relocates_typed_project_paths() {
    let mut fixture = Fixture::new();
    let (closure, artifact) = fixture.seal();
    let before = serde_json::to_value(&closure).unwrap();
    let first = compile_external_direct_program(
        &closure,
        &artifact,
        &fixture.protocol,
        THREAD,
        CHAIN,
        &fixture.inputs,
        None,
    )
    .unwrap();
    let second = compile_external_direct_program(
        &closure,
        &artifact,
        &fixture.protocol,
        THREAD,
        CHAIN,
        &fixture.inputs,
        None,
    )
    .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.digest().unwrap(), second.digest().unwrap());
    assert_eq!(serde_json::to_value(&closure).unwrap(), before);
    assert_eq!(first.namespace_executable().unwrap(), GUEST_COMMAND);
    assert_eq!(first.projection().argv0, GUEST_COMMAND);
    assert_eq!(first.projection().cwd, "/workspace");
    assert_eq!(first.projection().arguments, ["/workspace/tool.py"]);
    assert_eq!(first.projection().environment, fixture.inputs.environment);
    let stdin: serde_json::Value =
        serde_json::from_slice(&first.projection().stdin.decoded_bytes().unwrap()).unwrap();
    assert_eq!(
        stdin,
        json!({"message":"hello","project_path":"/workspace"})
    );
    assert_eq!(first.projection().timeout_seconds, 30);
    assert_eq!(
        first.projection().execution_mode,
        ExternalExecutionMode::DirectCommand {
            stdout_max_bytes: 4096,
            stderr_max_bytes: 2048
        }
    );
}

#[test]
fn external_direct_projection_preserves_opaque_input_and_rejects_unbound_paths() {
    let mut fixture = Fixture::new();
    let opaque = "{\"path\":\"/arbitrary/not-a-project-path\"}\nnot json";
    fixture.spec().stdin = Some(PlanStdin::Opaque {
        data: opaque.into(),
    });
    assert_eq!(
        fixture
            .compile()
            .unwrap()
            .projection()
            .stdin
            .decoded_bytes()
            .unwrap(),
        opaque.as_bytes()
    );
    fixture.spec().stdin = Some(PlanStdin::Opaque {
        data: format!("literal {PROJECT}"),
    });
    assert!(
        fixture
            .compile()
            .unwrap_err()
            .to_string()
            .contains("opaque stdin embeds")
    );
}

#[test]
fn external_direct_projection_rejects_changed_retained_plan_or_protocol_identity() {
    let mut fixture = Fixture::new();
    let (mut closure, artifact) = fixture.seal();
    let AdmittedExecutionClosure::DirectItemExecutor { execution_plan, .. } = &mut closure else {
        unreachable!()
    };
    execution_plan["nodes"][0]["spec"]["timeout_secs"] = json!(31);
    assert!(
        compile_external_direct_program(
            &closure,
            &artifact,
            &fixture.protocol,
            THREAD,
            CHAIN,
            &fixture.inputs,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("plan hash mismatch")
    );
    let (closure, artifact) = fixture.seal();
    fixture.protocol.descriptor.description = Some("changed interpretation".into());
    assert!(
        compile_external_direct_program(
            &closure,
            &artifact,
            &fixture.protocol,
            THREAD,
            CHAIN,
            &fixture.inputs,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("interpretation changed")
    );
    fixture.protocol.raw_content_digest = "0".repeat(64);
    assert!(
        compile_external_direct_program(
            &closure,
            &artifact,
            &fixture.protocol,
            THREAD,
            CHAIN,
            &fixture.inputs,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("differs from its retained authority")
    );
}

#[test]
fn retained_external_direct_endpoint_rejoins_exact_sealed_binding() {
    let mut fixture = Fixture::new();
    let (closure, artifact) = fixture.seal();
    let endpoint = retained_external_direct_endpoint(&closure, &artifact).unwrap();
    assert_eq!(endpoint.binding_id, "test-direct");
    assert_eq!(endpoint.binding_digest, "e".repeat(64));

    fixture
        .plan
        .external_endpoint_binding
        .as_mut()
        .unwrap()
        .binding_digest = "9".repeat(64);
    let (changed_closure, changed_artifact) = fixture.seal();
    assert!(retained_external_direct_endpoint(&changed_closure, &artifact).is_err());
    assert!(retained_external_direct_endpoint(&closure, &changed_artifact).is_err());
    assert_eq!(
        retained_external_direct_endpoint(&changed_closure, &changed_artifact)
            .unwrap()
            .binding_digest,
        "9".repeat(64)
    );
}

#[test]
fn retained_external_direct_endpoint_refuses_local_missing_and_wrong_binding() {
    for mutation in ["local", "missing", "wrong-id"] {
        let mut fixture = Fixture::new();
        match mutation {
            "local" => {
                fixture.plan.endpoint_requirement = ExecutionEndpointRequirement::Local {};
                fixture.plan.external_endpoint_binding = None;
            }
            "missing" => fixture.plan.external_endpoint_binding = None,
            "wrong-id" => {
                fixture
                    .plan
                    .external_endpoint_binding
                    .as_mut()
                    .unwrap()
                    .binding_id = "other-endpoint".into();
            }
            _ => unreachable!(),
        }
        let (closure, artifact) = fixture.seal();
        assert!(
            retained_external_direct_endpoint(&closure, &artifact).is_err(),
            "accepted {mutation} endpoint authority"
        );
    }
}

#[test]
fn external_direct_prebirth_constraints_need_no_born_program_or_guest_inputs() {
    let mut fixture = Fixture::new();
    let authority = immutable_project_authority("1".repeat(64));
    validate_external_direct_project_authority(&authority).unwrap();
    validate_external_direct_plan(&fixture.plan, &fixture.protocol.descriptor).unwrap();
    // Native target and wire budget bounds use the same contract before birth
    // as the final compiler. No synthetic launch, capsule or inputs are needed.
    for timeout in [0, 3601, u64::MAX] {
        fixture.spec().timeout_secs = timeout;
        assert!(
            validate_external_direct_plan(&fixture.plan, &fixture.protocol.descriptor).is_err()
        );
    }
    fixture.spec().timeout_secs = 3600;
    validate_external_direct_plan(&fixture.plan, &fixture.protocol.descriptor).unwrap();
    for source in [
        EnvInjectionSource::CasRoot,
        EnvInjectionSource::ThreadAuthToken,
    ] {
        let mut fixture = Fixture::new();
        fixture.protocol.descriptor.env_injections.push(
            ryeos_engine::protocol_vocabulary::EnvInjection {
                name: "AUTHORITY".into(),
                source,
            },
        );
        assert!(
            validate_external_direct_plan(&fixture.plan, &fixture.protocol.descriptor).is_err()
        );
    }
}

#[test]
fn external_direct_projection_refuses_semantics_it_cannot_preserve() {
    for (name, mutate) in [
        (
            "model",
            (|f: &mut Fixture| f.plan.capabilities.requires_model = true) as fn(&mut Fixture),
        ),
        ("network", |f: &mut Fixture| {
            f.plan.capabilities.requires_network = true
        }),
        ("custom", |f: &mut Fixture| {
            f.plan.capabilities.custom.push("unsupported".into())
        }),
        ("materialization", |f: &mut Fixture| {
            f.plan.materialization_requirements.push(
                ryeos_engine::contracts::MaterializationRequirement {
                    kind: "project".into(),
                    ref_string: "fixture".into(),
                },
            )
        }),
        ("local", |f: &mut Fixture| {
            f.plan.endpoint_requirement = ExecutionEndpointRequirement::Local {};
            f.plan.external_endpoint_binding = None;
        }),
        ("async", |f: &mut Fixture| {
            f.spec().execution.native_async = Some(ryeos_engine::contracts::NativeAsyncSpec {
                cancellation_mode: ryeos_engine::contracts::CancellationMode::Hard,
            })
        }),
        ("resume", |f: &mut Fixture| {
            f.spec().execution.native_resume = Some(Default::default())
        }),
        ("source-entry", |f: &mut Fixture| {
            f.spec().args.push(PlanArgument::AdmittedSourceEntry)
        }),
        ("missing-target", |f: &mut Fixture| {
            f.plan.target_requirement = None
        }),
        ("wrong-os", |f: &mut Fixture| {
            f.plan.target_requirement.as_mut().unwrap().os = "windows".into()
        }),
        ("wrong-arch", |f: &mut Fixture| {
            f.plan.target_requirement.as_mut().unwrap().arch = "unknown".into()
        }),
        ("resources", |f: &mut Fixture| {
            f.plan.target_requirement.as_mut().unwrap().resources.push(serde_json::from_value(json!({"class":"gpu","count":1,"allocation":"exclusive","access":"execution_restricted"})).unwrap())
        }),
        ("filesystem", |f: &mut Fixture| {
            f.plan.filesystem_authority_ceiling = IsolationFilesystemAuthorityCeiling::NodePolicy
        }),
        ("network-ceiling", |f: &mut Fixture| {
            f.plan.network_authority_ceiling = IsolationNetworkAuthorityCeiling::NodePolicy
        }),
    ] {
        let mut fixture = Fixture::new();
        mutate(&mut fixture);
        if name != "source-entry" {
            // Source redemption needs the later retained closure join; all
            // other unsupported semantics are knowable before thread birth.
            assert!(
                validate_external_direct_plan(&fixture.plan, &fixture.protocol.descriptor).is_err(),
                "prebirth constraints accepted unsupported {name}"
            );
        }
        assert!(
            fixture.compile().is_err(),
            "unsupported {name} was silently projected"
        );
    }
}

#[test]
fn external_direct_projection_refuses_protocol_controller_authority_and_env_substitution() {
    let mut collision = Fixture::new();
    collision
        .spec()
        .env
        .insert("RYE_THREAD_ID".into(), THREAD.into());
    assert!(
        collision
            .compile()
            .unwrap_err()
            .to_string()
            .contains("protocol-owned")
    );
    let mut foreign = Fixture::new();
    foreign
        .inputs
        .environment
        .insert("FOREIGN".into(), "injected".into());
    assert!(
        foreign
            .compile()
            .unwrap_err()
            .to_string()
            .contains("guest environment differs")
    );
    let mut authority = Fixture::new();
    authority.protocol.descriptor.env_injections.push(
        ryeos_engine::protocol_vocabulary::EnvInjection {
            name: "RYE_CAS".into(),
            source: EnvInjectionSource::CasRoot,
        },
    );
    assert!(authority.compile().is_err());
    let mut callback = Fixture::new();
    callback.protocol.descriptor.callback_channel = CallbackChannel::Http;
    assert!(callback.compile().is_err());
    let mut session = Fixture::new();
    session.protocol.descriptor.session=Some(serde_json::from_value(json!({
        "process_mode":"exclusive_session","cleanup_authority":"local_process_scope",
        "workspace_authority":"runtime_workspace","network_authority":"node_policy",
        "runtime_env_allowlist":[],"readiness_identity_env":"RYEOS_SESSION_BOOT_IDENTITY",
        "channel":"inherited_unix_socket","channel_env":"RYEOS_SESSION_FD","framing":"u32_be_json",
        "wire_protocol":"ryeos.structured-session","wire_version":2,"max_frame_bytes":1048576
    })).unwrap());
    assert!(session.compile().is_err());
}

#[test]
fn external_direct_projection_enforces_timeout_and_input_caps() {
    for timeout in [0, 3601] {
        let mut fixture = Fixture::new();
        fixture.spec().timeout_secs = timeout;
        assert!(fixture.compile().is_err(), "timeout {timeout}");
    }
    let mut fixture = Fixture::new();
    fixture.spec().timeout_secs = 3600;
    fixture.spec().stdin = Some(PlanStdin::Opaque {
        data: "x".repeat(64 * 1024),
    });
    assert_eq!(
        fixture
            .compile()
            .unwrap()
            .projection()
            .stdin
            .decoded_bytes()
            .unwrap()
            .len(),
        64 * 1024
    );
    fixture.spec().stdin = Some(PlanStdin::Opaque {
        data: "x".repeat(64 * 1024 + 1),
    });
    assert!(fixture.compile().is_err());
}

#[test]
fn external_direct_projection_preserves_ordinary_environment_source_authority() {
    for name in ["PATH", "RYEOSD_CALLBACK_TOKEN", "RYEOS_APP_ROOT"] {
        let mut fixture = Fixture::new();
        fixture.spec().env.insert(name.into(), "forged".into());
        fixture.spec().env_sources.insert(
            name.into(),
            ryeos_engine::contracts::RuntimeEnvSource::RuntimeDescriptor,
        );
        fixture
            .inputs
            .environment
            .insert(name.into(), "forged".into());
        assert!(
            fixture.compile().is_err(),
            "runtime descriptor gained {name} authority"
        );
    }
    let mut fixture = Fixture::new();
    let retained_search = format!("{PROJECT}/vendor/runtime/bin");
    let exact_search = "/workspace/vendor/runtime/bin";
    fixture.spec().env.insert("PATH".into(), retained_search);
    fixture.spec().env_sources.insert(
        "PATH".into(),
        ryeos_engine::contracts::RuntimeEnvSource::RuntimePathMutation,
    );
    fixture.inputs.executable_search = vec![exact_search.into()];
    fixture
        .inputs
        .environment
        .insert("PATH".into(), exact_search.into());
    assert_eq!(
        fixture.compile().unwrap().projection().environment["PATH"],
        exact_search
    );
    for search in [Vec::new(), vec![exact_search.to_owned()]] {
        fixture.spec().env.insert("PATH".into(), "/usr/bin".into());
        fixture
            .inputs
            .environment
            .insert("PATH".into(), "/usr/bin".into());
        fixture.inputs.executable_search = search;
        // Matching plan/guest environment is insufficient: the search list
        // must be the exact immutable input inventory, not ambient host PATH.
        assert!(fixture.compile().is_err());
    }
    let mut no_path = Fixture::new();
    no_path.inputs.executable_search = vec![exact_search.to_owned()];
    assert!(
        no_path.compile().is_err(),
        "search without PATH was discarded"
    );
}
