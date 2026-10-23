//! Controller-side construction of everything the owner imports: the sealed
//! supervisor bootstrap, the guest inputs, the signed import and assignment,
//! and the framed package. The candidate is a command that exits at once, so
//! the exchange ends through the direct-command terminal acknowledgement.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write as _;

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::crypto::SigningKey;
use ryeos_external_execution::guest_import_authorization::{
    sign_guest_import_authorization, sign_guest_occurrence_assignment,
};
use ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority;
use ryeos_external_execution::guest_package_producer::{
    PreparedGuestPackage, prepare_private_guest_package,
};
use ryeos_external_execution_contract::guest_import_authorization::{
    GUEST_IMPORT_AUTHORIZATION_SCHEMA, GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
    GuestImportAuthorization, GuestOccurrenceAssignment, GuestOccurrenceAssignmentDocument,
};
use ryeos_external_execution_contract::staging_package::{
    GUEST_IMPORT_TICKET_SCHEMA, GuestImportTicket, GuestStagingExpected,
};
use ryeos_external_execution_contract::{
    EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalExecutionMode, ExternalGuestInputProjection,
    GuestBaseSnapshotInput, GuestMountAccess, GuestMountContentAuthority, GuestMountInput,
    GuestMountKind, GuestMountRole, GuestProductManifestKind, GuestWorkspaceView,
};
use ryeos_state::external_execution::admission::{
    AdmittedExternalDirectProgram, AdmittedExternalExecutionProgram,
    ExternalDirectCommandProjection, ExternalDirectNativeRequirements, ExternalDirectSealedInput,
};
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_ROUTE_CONTRACT, EXTERNAL_SUPERVISOR_HTTP_BOOTSTRAP_SCHEMA,
    ExternalControllerTransportContract, ExternalNetworkInputPolicy, ExternalNetworkInputSelection,
    ExternalSupervisorBootstrap, ExternalSupervisorNativeEnvelope, external_tls_root_bundle_digest,
};
use ryeos_state::objects::{
    ADMITTED_DIRECT_PROJECT_ROOT, AdmittedDirectCommandClosure, AdmittedExecutionClosure,
    AdmittedLaunchArtifactIdentity, DirectExecutableIdentity, DirectRootSourceIdentity,
    DirectRuntimeIdentity, DirectRuntimeSourceSpace, ExternalContentMountRoot, ProjectSnapshot,
    ProjectSnapshotPolicy, ProjectTree,
};

pub const PLACEMENT_ID: &str = "T-guest-wire-probe";
pub const OCCURRENCE_ID: &str = "occ-guest-wire-probe";
pub const IMPORT_NAME: &str = "signed-import.json";
pub const PACKAGE_NAME: &str = "guest-package";
pub const RESOLVER_NAME: &str = "network-resolver";
pub const HOSTS_NAME: &str = "network-hosts";
const PRODUCT_MOUNT: &str = "runtime";
const COMMAND_RELATIVE: &str = "bin/evaluate";
const COMMAND_PATH: &str = "/ryeos/realizations/runtime/bin/evaluate";
const PRODUCT_DESTINATION: &str = "/ryeos/realizations/runtime";

/// The candidate: a statically linked image whose only behavior is exiting
/// with status zero. Its identity carries no claim; the exchange does.
pub fn exit_zero_image() -> Result<Vec<u8>> {
    ensure!(
        cfg!(target_arch = "x86_64"),
        "the minimal candidate image is only defined for x86_64 guests"
    );
    let code: [u8; 12] = [
        0xbf, 0, 0, 0, 0, // mov edi, 0
        0xb8, 0x3c, 0, 0, 0, // mov eax, 60
        0x0f, 0x05, // syscall
    ];
    let total = 120 + code.len() as u64;
    let mut image = Vec::with_capacity(total as usize);
    image.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    image.extend_from_slice(&[0; 8]);
    image.extend_from_slice(&2u16.to_le_bytes()); // executable
    image.extend_from_slice(&0x3eu16.to_le_bytes()); // x86-64
    image.extend_from_slice(&1u32.to_le_bytes());
    image.extend_from_slice(&0x400078u64.to_le_bytes()); // entry
    image.extend_from_slice(&64u64.to_le_bytes()); // program header offset
    image.extend_from_slice(&0u64.to_le_bytes()); // no sections
    image.extend_from_slice(&0u32.to_le_bytes());
    image.extend_from_slice(&64u16.to_le_bytes());
    image.extend_from_slice(&56u16.to_le_bytes());
    image.extend_from_slice(&1u16.to_le_bytes());
    image.extend_from_slice(&[0; 6]);
    image.extend_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    image.extend_from_slice(&5u32.to_le_bytes()); // R+X
    image.extend_from_slice(&0u64.to_le_bytes());
    image.extend_from_slice(&0x400000u64.to_le_bytes());
    image.extend_from_slice(&0x400000u64.to_le_bytes());
    image.extend_from_slice(&total.to_le_bytes());
    image.extend_from_slice(&total.to_le_bytes());
    image.extend_from_slice(&0x1000u64.to_le_bytes());
    image.extend_from_slice(&code);
    ensure!(
        image.len() as u64 == total,
        "candidate image layout drifted"
    );
    Ok(image)
}

fn digest_of(label: &str) -> String {
    lillux::sha256_hex(format!("ryeos.guest-wire-probe.{label}").as_bytes())
}

pub fn write_file(
    directory: &lillux::PinnedDirectory,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<lillux::PinnedRegularFile> {
    let mut file = directory.open_regular_create(OsStr::new(name), true, true, 0o600)?;
    file.write_all(bytes)?;
    lillux::set_open_regular_file_mode(&file, mode)?;
    file.sync_all()?;
    drop(file);
    directory
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("written file {name} vanished"))
}

pub struct FixtureInputs<'a> {
    pub work: &'a lillux::PinnedDirectory,
    pub root_key: &'a SigningKey,
    pub controller_key: &'a SigningKey,
    pub probe_manifest_hash: &'a str,
    pub supervisor: &'a [u8],
    pub launcher: &'a [u8],
    pub tls_root_der: &'a [u8],
    pub port: u16,
    pub activation_root: &'a str,
}

pub struct Prepared {
    pub bootstrap_bytes: Vec<u8>,
    pub signed_import: Vec<u8>,
    pub signed_assignment: Vec<u8>,
    pub package: PreparedGuestPackage,
    pub projection: ExternalGuestInputProjection,
    pub expected_ready_inputs: Vec<String>,
    pub resolver_bytes: Vec<u8>,
    pub hosts_bytes: Vec<u8>,
}

fn base_snapshot(
    work: &lillux::PinnedDirectory,
) -> Result<(
    lillux::PinnedDirectory,
    ryeos_project_capture::ProjectSnapshotTransferMeasurement,
)> {
    let base = work.create_child(OsStr::new("base"), 0o700)?;
    let cas = lillux::CasStore::from_pinned_root(base.try_clone()?);
    let policy = ProjectSnapshotPolicy::new(
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
        vec![],
        vec![],
        BTreeMap::new(),
    )?;
    let policy_hash = cas.store_object(&policy.to_value())?;
    let tree_hash = cas.store_object(
        &ProjectTree {
            files: BTreeMap::new(),
        }
        .to_value(),
    )?;
    let snapshot_hash = cas.store_object(
        &ProjectSnapshot {
            project_tree_hash: tree_hash,
            effective_policy_hash: policy_hash,
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-10-23T00:00:00Z".into(),
            source: "guest-wire-probe".into(),
        }
        .to_value(),
    )?;
    let measurement =
        ryeos_project_capture::inspect_project_snapshot_transfer(&base, &snapshot_hash)?;
    Ok((base, measurement))
}

pub fn prepare(inputs: &FixtureInputs<'_>) -> Result<Prepared> {
    let now = lillux::time::timestamp_millis();
    let (base, measurement) = base_snapshot(inputs.work)?;

    // Candidate product tree and its retained manifest record.
    let image = exit_zero_image()?;
    let product = inputs.work.create_child(OsStr::new("product"), 0o755)?;
    let bin = product.create_child(OsStr::new("bin"), 0o755)?;
    write_file(&bin, "evaluate", &image, 0o755)?;
    let manifest = ryeos_state::observe_external_content_tree_exact(&product)?;
    let manifest_record = lillux::canonical_json(&serde_json::to_value(&manifest)?)?.into_bytes();
    let product_manifest_hash = lillux::sha256_hex(&manifest_record);
    ensure!(
        ryeos_state::external_content_manifest_digest(&manifest)? == product_manifest_hash,
        "candidate manifest record differs from its digest"
    );
    let record_file = write_file(
        inputs.work,
        "product-manifest.json",
        &manifest_record,
        0o400,
    )?;

    // Protected supervisor bootstrap, launcher and supervisor members.
    let supervisor_sha256 = lillux::sha256_hex(inputs.supervisor);
    let launcher_sha256 = lillux::sha256_hex(inputs.launcher);
    let supervisor_file = write_file(inputs.work, "supervisor", inputs.supervisor, 0o755)?;
    let launcher_file = write_file(inputs.work, "launcher", inputs.launcher, 0o755)?;

    // Guest input projection with this process's real descriptor coordinates.
    let base_authority = base.inherited_descriptor_authority()?;
    let product_authority = product.inherited_descriptor_authority()?;
    let record_authority = record_file.inherited_descriptor_authority()?;
    let projection = ExternalGuestInputProjection {
        workspace_view: GuestWorkspaceView::FullSnapshot,
        schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
        base_snapshot: GuestBaseSnapshotInput {
            descriptor: base_authority
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            snapshot_hash: measurement.snapshot_hash.clone(),
            closure_digest: measurement.closure_digest.clone(),
            object_count: measurement.object_count,
            blob_count: measurement.blob_count,
            total_bytes: measurement.total_bytes,
        },
        workspace_outputs: None,
        inputs: vec![GuestMountInput {
            role: GuestMountRole::Product,
            authority_id: PRODUCT_MOUNT.into(),
            descriptor: product_authority
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            destination: PRODUCT_DESTINATION.into(),
            kind: GuestMountKind::Directory,
            access: GuestMountAccess::ReadOnly,
            normalized_mode: None,
            content_authority: GuestMountContentAuthority::ProductManifest {
                manifest_kind: GuestProductManifestKind::Content,
                manifest_hash: product_manifest_hash.clone(),
                manifest_descriptor: record_authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?,
                manifest_bytes: manifest_record.len() as u64,
            },
            bytes: image.len() as u64,
        }],
        executable_search: vec![],
        environment: BTreeMap::new(),
    };
    projection.validate()?;
    let guest_input_identity = projection.identity_digest()?;

    // Direct command program: the candidate exits and the supervisor reports it.
    let execution_plan = serde_json::json!({"guest_wire_probe": true});
    let image_hash = lillux::sha256_hex(&image);
    let closure = AdmittedExecutionClosure::DirectItemExecutor {
        execution_plan: execution_plan.clone(),
        protocol_descriptor_document: "guest wire probe; no production signature claim".into(),
        command: AdmittedDirectCommandClosure::RealizationMember {
            executable_blob_hash: image_hash.clone(),
            realization_id: PRODUCT_MOUNT.into(),
            realization_manifest_hash: product_manifest_hash.clone(),
            realization_mount_root: ExternalContentMountRoot::ExecutionRuntime,
            realization_mount: PRODUCT_MOUNT.into(),
            relative_path: COMMAND_RELATIVE.into(),
            execution_path: COMMAND_PATH.into(),
        },
        admitted_project_root: Some(ADMITTED_DIRECT_PROJECT_ROOT.into()),
    };
    let artifact = AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executor_ref: "tool:guest-wire/probe".into(),
        root_subject_source_content_digest: digest_of("subject-source"),
        root_subject_signer_fingerprint: Some(digest_of("signer")),
        root_subject_source_identity: DirectRootSourceIdentity::Project,
        protocol_ref: "protocol:guest-wire/probe".into(),
        protocol_content_hash: digest_of("protocol"),
        protocol_signer_fingerprint: digest_of("protocol-signer"),
        execution_plan_hash: lillux::sha256_hex(
            lillux::canonical_json(&execution_plan)?.as_bytes(),
        ),
        executable_identity: DirectExecutableIdentity::CapturedContent {
            content_hash: image_hash,
        },
        runtime_identity: DirectRuntimeIdentity {
            runtime_ref: "runtime:guest-wire/probe".into(),
            runtime_source_space: DirectRuntimeSourceSpace::Project,
            runtime_content_hash: digest_of("runtime"),
            runtime_signer_fingerprint: digest_of("runtime-signer"),
            runtime_bundle_manifest_hash: None,
            runtime_bundle_signer_fingerprint: None,
        },
    };
    let endpoint_binding_digest = digest_of("endpoint-binding");
    let command_projection = ExternalDirectCommandProjection {
        workspace_view: GuestWorkspaceView::FullSnapshot,
        target_limits: None,
        argv0: COMMAND_PATH.into(),
        arguments: Vec::new(),
        cwd: "/workspace".into(),
        environment: BTreeMap::new(),
        stdin: ExternalDirectSealedInput::from_bytes(b"guest wire probe\n")?,
        endpoint_binding_id: "guest-wire-probe".into(),
        endpoint_binding_digest: endpoint_binding_digest.clone(),
        execution_mode: ExternalExecutionMode::DirectCommand {
            stdout_max_bytes: 1024,
            stderr_max_bytes: 1024,
        },
        timeout_seconds: 30,
        native: ExternalDirectNativeRequirements::LinuxIsolatedReadOnly {
            required_arch: "x86_64".into(),
        },
    };
    let program = AdmittedExternalDirectProgram::from_compiled_closure(
        &closure,
        &artifact,
        command_projection,
        &projection,
    )?;
    let supervisor_runtime_hash = program.runtime_manifest_hash()?.to_owned();

    // Sealed bootstrap naming the loopback controller and its ephemeral root.
    let roots = vec![STANDARD.encode(inputs.tls_root_der)];
    let resolver_bytes = b"nameserver 127.0.0.1\n".to_vec();
    let hosts_bytes = b"127.0.0.1 localhost\n".to_vec();
    let attachment_deadline_ms = now + 180_000;
    let allocation_request_digest = digest_of("allocation");
    let admitted_capsule_hash = digest_of("capsule");
    let activation_request_digest = digest_of("activation");
    let bootstrap = ExternalSupervisorBootstrap {
        schema: EXTERNAL_SUPERVISOR_HTTP_BOOTSTRAP_SCHEMA,
        controller: ExternalControllerTransportContract {
            schema: 2,
            network_inputs: ExternalNetworkInputPolicy {
                resolver: ExternalNetworkInputSelection {
                    source: format!("{}/{RESOLVER_NAME}", inputs.activation_root),
                    max_bytes: 65_536,
                },
                hosts: ExternalNetworkInputSelection {
                    source: format!("{}/{HOSTS_NAME}", inputs.activation_root),
                    max_bytes: 65_536,
                },
            },
            https_origin: format!("https://127.0.0.1:{}", inputs.port),
            route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
            tls_root_bundle_digest: external_tls_root_bundle_digest(&roots)?,
            connect_timeout_ms: 5_000,
            request_timeout_ms: 10_000,
            maximum_response_bytes: 1024 * 1024,
        }
        .into(),
        tls_root_certificates_der_base64: roots,
        placement_thread_id: PLACEMENT_ID.into(),
        occurrence_id: OCCURRENCE_ID.into(),
        allocation_request_digest: allocation_request_digest.clone(),
        admitted_capsule_hash: admitted_capsule_hash.clone(),
        base_snapshot_hash: measurement.snapshot_hash.clone(),
        execution_binding_hash: endpoint_binding_digest.clone(),
        supervisor_runtime_hash: supervisor_runtime_hash.clone(),
        launcher_artifact_hash: launcher_sha256.clone(),
        candidate_program: AdmittedExternalExecutionProgram::DirectCommand(program),
        guest_input_identity: guest_input_identity.clone(),
        guest_inputs: projection.clone(),
        owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
            &inputs.controller_key.verifying_key(),
        )?,
        bootstrap_capability: STANDARD.encode(lillux::crypto::generate_random_bytes::<32>()),
        attachment_deadline_ms,
        execution_timeout_seconds: 30,
        post_execution_timeout_seconds: 60,
        candidate_export_max_bytes: 0,
        channel_max_bytes: 1024 * 1024,
        native_journal: ExternalSupervisorNativeEnvelope {
            storage_bytes: 67_108_864,
            allocator_steps: 2_000_000_000,
            connections: 16,
            operation_millis: 4_860_000,
            cleanup_millis: 120_000,
            cleanup_steps: 3_000_000,
        },
        report_capabilities: true,
    };
    let bootstrap_bytes = bootstrap.canonical_bytes()?;
    let bootstrap_sha256 = lillux::sha256_hex(&bootstrap_bytes);
    let bootstrap_file = write_file(inputs.work, "bootstrap", &bootstrap_bytes, 0o600)?;

    // Package: real producer, including its local re-import preflight.
    let known_regular = measurement.total_bytes
        + (inputs.supervisor.len() + inputs.launcher.len() + bootstrap_bytes.len()) as u64
        + image.len() as u64
        + manifest_record.len() as u64;
    let maximum_regular_bytes = known_regular + 64 * 1024;
    let maximum_framed_bytes = maximum_regular_bytes + 4 * 1024 * 1024;
    let expected = GuestStagingExpected {
        inputs: &projection,
        activation_request_digest: &activation_request_digest,
        bootstrap_sha256: &bootstrap_sha256,
        supervisor_sha256: &supervisor_sha256,
        launcher_sha256: &launcher_sha256,
        maximum_regular_bytes,
        maximum_framed_bytes,
    };
    let authority = ExternalGuestInputAuthority::new(
        projection.clone(),
        base_authority,
        None,
        vec![product_authority],
        vec![record_authority],
        Vec::new(),
    )?;
    let package = prepare_private_guest_package(
        inputs.work,
        &authority,
        &bootstrap_file.inherited_descriptor_authority()?,
        &supervisor_file.inherited_descriptor_authority()?,
        &launcher_file.inherited_descriptor_authority()?,
        &expected,
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(60)),
    )?;

    // Root-signed assignment and owner-signed import over the exact package.
    let ticket = GuestImportTicket {
        schema: GUEST_IMPORT_TICKET_SCHEMA,
        binding_hash: endpoint_binding_digest.clone(),
        allocation_request_digest: allocation_request_digest.clone(),
        occurrence_id: OCCURRENCE_ID.into(),
        activation_request_digest: activation_request_digest.clone(),
        guest_input_identity,
        payload_sha256: package.sha256().to_owned(),
        manifest_sha256: package.manifest_sha256().to_owned(),
        framed_bytes: package.bytes(),
        regular_bytes: package.manifest().total_regular_bytes,
        bootstrap_sha256,
        supervisor_sha256,
        launcher_sha256,
        maximum_regular_bytes,
        maximum_framed_bytes,
    };
    let assignment = GuestOccurrenceAssignment {
        placement_thread_id: PLACEMENT_ID,
        admitted_capsule_hash: &admitted_capsule_hash,
        base_snapshot_hash: &measurement.snapshot_hash,
        execution_binding_hash: &endpoint_binding_digest,
        allocation_request_digest: &allocation_request_digest,
        occurrence_id: OCCURRENCE_ID,
        activation_request_digest: &activation_request_digest,
        supervisor_runtime_hash: &supervisor_runtime_hash,
        guest_runtime_manifest_hash: inputs.probe_manifest_hash,
        attachment_deadline_ms,
    };
    let document = GuestOccurrenceAssignmentDocument {
        schema: GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
        placement_thread_id: PLACEMENT_ID.into(),
        admitted_capsule_hash: admitted_capsule_hash.clone(),
        base_snapshot_hash: measurement.snapshot_hash.clone(),
        execution_binding_hash: endpoint_binding_digest.clone(),
        allocation_request_digest: allocation_request_digest.clone(),
        occurrence_id: OCCURRENCE_ID.into(),
        activation_request_digest: activation_request_digest.clone(),
        supervisor_runtime_hash: supervisor_runtime_hash.clone(),
        guest_runtime_manifest_hash: inputs.probe_manifest_hash.into(),
        owner_public_key_hex: hex::encode(inputs.controller_key.verifying_key().to_bytes()),
        attachment_deadline_ms,
    };
    let signed_assignment = ryeos_external_execution_contract::canonical_json(
        &sign_guest_occurrence_assignment(document, inputs.root_key)?,
    )?;
    let authorization = GuestImportAuthorization {
        schema: GUEST_IMPORT_AUTHORIZATION_SCHEMA,
        placement_thread_id: PLACEMENT_ID.into(),
        admitted_capsule_hash: admitted_capsule_hash.clone(),
        base_snapshot_hash: measurement.snapshot_hash.clone(),
        execution_binding_hash: endpoint_binding_digest.clone(),
        allocation_request_digest: allocation_request_digest.clone(),
        occurrence_id: OCCURRENCE_ID.into(),
        activation_request_digest: activation_request_digest.clone(),
        supervisor_runtime_hash: supervisor_runtime_hash.clone(),
        guest_runtime_manifest_hash: inputs.probe_manifest_hash.into(),
        attachment_deadline_ms,
        admission_deadline_ms: attachment_deadline_ms - 1_000,
        nonce_sha256: lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>()),
        ticket,
        guest_inputs: projection.clone(),
    };
    let signed_import = ryeos_external_execution_contract::canonical_json(
        &sign_guest_import_authorization(authorization, inputs.controller_key, &assignment)?,
    )?;
    let expected_ready_inputs =
        ryeos_external_execution_contract::lease_ready_inputs::ordered_lease_ready_inputs(
            &activation_request_digest,
            &ryeos_external_execution_contract::lease_ready_inputs::signed_import_sha256(
                &signed_import,
            ),
            package.sha256(),
        )
        .into_iter()
        .map(str::to_owned)
        .collect();
    Ok(Prepared {
        bootstrap_bytes,
        signed_import,
        signed_assignment,
        package,
        projection,
        expected_ready_inputs,
        resolver_bytes,
        hosts_bytes,
    })
}
