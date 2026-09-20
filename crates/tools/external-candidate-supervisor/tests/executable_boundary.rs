#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_external_candidate_supervisor::runtime::{
    SUPERVISOR_BOOTSTRAP_FD, SUPERVISOR_CANDIDATE_RUNTIME_FD, SUPERVISOR_LAUNCHER_FD,
    SUPERVISOR_PRIVATE_PARENT_FD, SUPERVISOR_RUNTIME_MOUNT_FD_BASE, SUPERVISOR_STATE_ROOT_FD,
};
use ryeos_state::external_execution::ExecutionChannelBinding;
use ryeos_state::external_execution::admission::{
    AdmittedExternalCandidateProgram, ExternalCandidateProcFilesystem,
    ExternalCandidateRequirement, ExternalCandidateRuntimeRecipe, PROTOCOL,
};
use ryeos_state::external_execution::encode_channel_public_key;
use ryeos_state::external_execution::guest_journal::PreparedGuestJournal;
use ryeos_state::external_execution::supervisor_journal::PreparedExternalSupervisorJournal;
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_ROUTE_CONTRACT, ExternalControllerTransportContract,
    ExternalSupervisorBootstrap, MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
    external_tls_root_bundle_digest,
};

fn request() -> lillux::SubprocessRequest {
    lillux::SubprocessRequest {
        cmd: env!("CARGO_BIN_EXE_ryeos-external-candidate-supervisor").into(),
        argv0: None,
        args: Vec::new(),
        cwd: None,
        envs: Vec::new(),
        stdin_data: None,
        timeout: 10.0,
        limits: None,
        inherited_fds: Vec::new(),
        inherited_fd_mappings: Vec::new(),
        supervised_status: None,
    }
}

fn run_with_bootstrap(
    bootstrap: &lillux::InheritedDescriptorAuthority,
) -> lillux::SubprocessResult {
    let mut request = request();
    bootstrap
        .bind_to_subprocess_request(&mut request, SUPERVISOR_BOOTSTRAP_FD)
        .unwrap();
    lillux::run(request)
}

fn bind_directory(
    request: &mut lillux::SubprocessRequest,
    directory: &lillux::PinnedDirectory,
    target_fd: u32,
) {
    directory
        .inherited_descriptor_authority()
        .unwrap()
        .bind_to_subprocess_request(request, target_fd)
        .unwrap();
}

fn bootstrap() -> ExternalSupervisorBootstrap {
    let roots = vec![STANDARD.encode(b"bounded executable-boundary root")];
    let recipe = ExternalCandidateRuntimeRecipe {
        schema: 1,
        runtime_mount_destination: "/runtime".into(),
        executable_relative_path: "bin/codex".into(),
        argv0: "codex".into(),
        arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
        cwd: "/workspace".into(),
        environment: BTreeMap::new(),
        max_stdout_bytes: 1024 * 1024,
        max_stderr_bytes: 1024 * 1024,
        proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
        contain_process_group: true,
        nested_sandbox: true,
    };
    let runtime_recipe_digest = recipe.digest().unwrap();
    ExternalSupervisorBootstrap {
        schema: 4,
        controller: ExternalControllerTransportContract {
            schema: 1,
            https_origin: "https://controller.invalid".into(),
            route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
            tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
            connect_timeout_ms: 1_000,
            request_timeout_ms: 2_000,
            maximum_response_bytes: 64 * 1024,
        },
        tls_root_certificates_der_base64: roots,
        placement_thread_id: "T-executable-boundary".into(),
        occurrence_id: "occurrence-executable-boundary".into(),
        allocation_request_digest: "a".repeat(64),
        admitted_capsule_hash: "b".repeat(64),
        base_snapshot_hash: "c".repeat(64),
        execution_binding_hash: "d".repeat(64),
        supervisor_runtime_hash: "e".repeat(64),
        launcher_artifact_hash: "4".repeat(64),
        candidate_program: AdmittedExternalCandidateProgram {
            requirement: ExternalCandidateRequirement {
                schema: 2,
                protocol: PROTOCOL.into(),
                runtime_product_declaration_id: "runtime".into(),
                runtime_recipe: recipe,
            },
            runtime_manifest_hash: "e".repeat(64),
            runtime_witness_hash: "1".repeat(64),
            qualification_attestation_hash: "2".repeat(64),
            selection_identity_digest: "3".repeat(64),
            runtime_recipe_digest,
        },
        owner_public_key: encode_channel_public_key(
            &lillux::crypto::SigningKey::from_bytes(&[51; 32]).verifying_key(),
        )
        .unwrap(),
        bootstrap_capability: STANDARD.encode([7_u8; 32]),
        attachment_deadline_ms: lillux::time::timestamp_millis() + 60_000,
        execution_timeout_seconds: 30,
        post_execution_timeout_seconds: 30,
        candidate_export_max_bytes: 1024 * 1024,
        channel_max_bytes: 2 * 1024 * 1024,
    }
}

#[test]
fn missing_bootstrap_descriptor_fails_closed() {
    let result = lillux::run(request());
    assert!(!result.success);
    assert_eq!(result.exit_code, 126, "{}", result.stderr);
    assert!(
        result
            .stderr
            .contains("ryeos-external-candidate-supervisor")
    );
}

#[test]
fn unsealed_bootstrap_descriptor_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("bootstrap.json");
    std::fs::write(&path, bootstrap().canonical_bytes().unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let document = directory
        .open_inherited_regular(OsStr::new("bootstrap.json"), false)
        .unwrap()
        .unwrap();

    let result = run_with_bootstrap(&document);
    assert!(!result.success);
    assert_eq!(result.exit_code, 126);
    assert!(result.stderr.contains("not sealed"), "{}", result.stderr);
}

#[test]
fn oversized_bootstrap_descriptor_fails_closed() {
    let bytes = vec![b'x'; MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES + 1];
    let document = lillux::sealed_memfd(c"external-supervisor-oversized", &bytes).unwrap();

    let result = run_with_bootstrap(&document);
    assert!(!result.success);
    assert_eq!(result.exit_code, 126);
    assert!(result.stderr.contains("exceeds"), "{}", result.stderr);
}

#[test]
fn noncanonical_bootstrap_descriptor_fails_closed() {
    let mut bytes = bootstrap().canonical_bytes().unwrap();
    bytes.insert(0, b' ');
    let document = lillux::sealed_memfd(c"external-supervisor-noncanonical", &bytes).unwrap();

    let result = run_with_bootstrap(&document);
    assert!(!result.success);
    assert_eq!(result.exit_code, 126);
    assert!(result.stderr.contains("not canonical"), "{}", result.stderr);
}

#[test]
fn canonical_bootstrap_advances_to_fixed_state_descriptor() {
    let document = lillux::sealed_memfd(
        c"external-supervisor-canonical",
        &bootstrap().canonical_bytes().unwrap(),
    )
    .unwrap();

    let result = run_with_bootstrap(&document);
    assert!(!result.success);
    assert_eq!(result.exit_code, 126, "{}", result.stderr);
    assert!(
        result.stderr.contains("Bad file descriptor"),
        "expected missing fixed state fd {SUPERVISOR_STATE_ROOT_FD}: {}",
        result.stderr
    );
    assert!(!result.stderr.contains("bootstrap is not canonical"));
}

#[test]
fn retained_launch_intent_is_recovery_only_across_the_executable_boundary() {
    let root = tempfile::tempdir().unwrap();
    let state_path = root.path().join("state");
    let candidate_path = root.path().join("candidate");
    let private_path = root.path().join("private");
    let runtime_path = root.path().join("runtime");
    for path in [&state_path, &candidate_path, &private_path, &runtime_path] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::create_dir(runtime_path.join("bin")).unwrap();
    std::fs::write(runtime_path.join("bin/codex"), b"runtime artifact").unwrap();

    let marker = root.path().join("launcher-contacted");
    let launcher_path = root.path().join("launcher");
    std::fs::write(
        &launcher_path,
        format!("#!/bin/sh\ntouch '{}'\nexit 99\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&launcher_path, std::fs::Permissions::from_mode(0o700)).unwrap();

    let state = lillux::PinnedDirectory::open(&state_path).unwrap().unwrap();
    let candidate = lillux::PinnedDirectory::open(&candidate_path)
        .unwrap()
        .unwrap();
    let private = lillux::PinnedDirectory::open(&private_path)
        .unwrap()
        .unwrap();
    let runtime = lillux::PinnedDirectory::open(&runtime_path)
        .unwrap()
        .unwrap();
    let launcher_parent = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    let launcher = launcher_parent
        .open_inherited_regular(OsStr::new("launcher"), false)
        .unwrap()
        .unwrap();

    let mut admitted = bootstrap();
    admitted.launcher_artifact_hash = launcher
        .digest_regular_file_stable_exact(&launcher.regular_file_observation().unwrap())
        .unwrap();
    admitted.candidate_program.runtime_manifest_hash =
        ryeos_state::external_content_manifest_digest(
            &ryeos_state::observe_external_content_tree_exact(&runtime).unwrap(),
        )
        .unwrap();
    admitted.supervisor_runtime_hash = admitted.candidate_program.runtime_manifest_hash.clone();

    let outer = state.create_child(OsStr::new("outer"), 0o700).unwrap();
    let guest = state.create_child(OsStr::new("guest"), 0o700).unwrap();
    let retained_bootstrap = serde_json::from_slice(&admitted.canonical_bytes().unwrap()).unwrap();
    let prepared = PreparedExternalSupervisorJournal::create(
        outer,
        retained_bootstrap,
        lillux::crypto::generate_signing_key(),
    )
    .unwrap();
    let store_identity = prepared.store_identity().clone();
    let issued_at_ms = lillux::time::timestamp_millis();
    let binding = ExecutionChannelBinding {
        schema: 3,
        placement_thread_id: admitted.placement_thread_id.clone(),
        allocation_request_digest: admitted.allocation_request_digest.clone(),
        occurrence_id: admitted.occurrence_id.clone(),
        admitted_capsule_hash: admitted.admitted_capsule_hash.clone(),
        base_snapshot_hash: admitted.base_snapshot_hash.clone(),
        execution_binding_hash: admitted.execution_binding_hash.clone(),
        supervisor_runtime_hash: admitted.supervisor_runtime_hash.clone(),
        candidate_program_digest: admitted.candidate_program.digest().unwrap(),
        channel_nonce: "f".repeat(64),
        owner_public_key: admitted.owner_public_key.clone(),
        supervisor_public_key: encode_channel_public_key(
            &prepared.supervisor_signing_key().verifying_key(),
        )
        .unwrap(),
        issued_at_ms,
        execution_deadline_ms: issued_at_ms + 30_000,
        expires_at_ms: issued_at_ms + 60_000,
        candidate_export_max_bytes: admitted.candidate_export_max_bytes,
        max_frames: admitted.binding_max_frames().unwrap(),
        max_bytes: admitted.channel_max_bytes,
    };
    let attached = prepared.record_binding(binding.clone()).unwrap();
    let authority_root = tempfile::tempdir().unwrap();
    let authority_db = ryeos_state::StateDb::open(
        authority_root.path(),
        std::sync::Arc::new(ryeos_state::TrustStore::new()),
    )
    .unwrap();
    let authority = authority_db.pinned_authority().unwrap();
    drop(authority_db);
    let launch_digest = "7".repeat(64);
    let reservation =
        PreparedGuestJournal::reserve(guest, &authority, &launch_digest, binding).unwrap();
    let launch = attached
        .begin_launch(
            reservation.store_identity().clone(),
            &launch_digest,
            &launch_digest,
            &admitted.launcher_artifact_hash,
        )
        .unwrap();
    let expected_binding_digest = launch.binding().digest().unwrap();
    drop(launch);
    drop(reservation);

    let anchor = serde_json::json!({
        "schema": 1,
        "state_root_identity": state.identity().unwrap(),
        "outer_store_identity": store_identity,
    });
    let anchor = lillux::canonical_json(&anchor).unwrap().into_bytes();
    state
        .atomic_write_if_same(OsStr::new("supervisor-state.json"), None, &anchor, 0o600)
        .unwrap();

    let bootstrap_document = lillux::sealed_memfd(
        c"external-supervisor-recovery",
        &admitted.canonical_bytes().unwrap(),
    )
    .unwrap();
    let mut child = request();
    bootstrap_document
        .bind_to_subprocess_request(&mut child, SUPERVISOR_BOOTSTRAP_FD)
        .unwrap();
    bind_directory(&mut child, &state, SUPERVISOR_STATE_ROOT_FD);
    bind_directory(&mut child, &candidate, SUPERVISOR_CANDIDATE_RUNTIME_FD);
    bind_directory(&mut child, &private, SUPERVISOR_PRIVATE_PARENT_FD);
    launcher
        .bind_to_subprocess_request(&mut child, SUPERVISOR_LAUNCHER_FD)
        .unwrap();
    bind_directory(&mut child, &runtime, SUPERVISOR_RUNTIME_MOUNT_FD_BASE);

    let result = lillux::run(child);
    assert!(!result.success);
    assert_eq!(result.exit_code, 76, "{}", result.stderr);
    let outcome: serde_json::Value = serde_json::from_str(result.stdout.trim()).unwrap();
    assert_eq!(outcome["status"], "recovery_only");
    assert_eq!(outcome["binding_digest"], expected_binding_digest);
    assert!(lillux::valid_hash(
        outcome["launch_intent_digest"].as_str().unwrap()
    ));
    assert!(!marker.exists(), "recovery spawned the candidate launcher");
}
