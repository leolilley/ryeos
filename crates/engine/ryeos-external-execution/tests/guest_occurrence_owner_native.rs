//! Opt-in, single-threaded production source-owner component probe.
//! `RYEOS_GUEST_OCCURRENCE_NATIVE=1 cargo test ... --test guest_occurrence_owner_native`
//! requires a host that admits Lillux user/mount namespaces. This is not a
//! supervisor, Render, or Codex qualification result.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_external_execution::guest_import_authorization::{
    ObservedGuestRuntime, sign_guest_import_authorization, sign_guest_occurrence_assignment,
    verify_guest_import_documents,
};
use ryeos_external_execution::guest_installation::{
    GuestOccurrenceOwner, GuestOccurrenceRecoveryPhase,
    prepare_authorized_held_guest_supervisor_once, recover_guest_occurrence_authorized,
};
use ryeos_external_execution_contract::guest_import_authorization::{
    GUEST_IMPORT_AUTHORIZATION_SCHEMA, GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
    GuestImportAuthorization, GuestOccurrenceAssignment, GuestOccurrenceAssignmentDocument,
    SignedGuestImportAuthorization,
};
use ryeos_external_execution_contract::staging_package::{
    GUEST_IMPORT_TICKET_SCHEMA, GUEST_STAGING_PACKAGE_SCHEMA, GuestImportContext,
    GuestImportTicket, GuestStagingEntry, GuestStagingExpected, GuestStagingPackageManifest,
    GuestStagingStreamWriter,
};
use ryeos_external_execution_contract::{
    EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalGuestInputProjection, GuestBaseSnapshotInput,
    GuestMountAccess, GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
};
use ryeos_state::objects::{ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};

fn supervisor_bootstrap(
    inputs: &ExternalGuestInputProjection,
    launcher_hash: &str,
    occurrence_id: &str,
) -> Result<Vec<u8>> {
    use ryeos_state::external_execution::admission::{
        AdmittedExternalCandidateProgram, ExternalCandidateExecutionRoute,
        ExternalCandidateProcFilesystem, ExternalCandidateRequirement,
        ExternalCandidateRuntimeRecipe, PROTOCOL,
    };
    use ryeos_state::external_execution::transport::{
        EXTERNAL_CHANNEL_ROUTE_CONTRACT, ExternalControllerTransportContract,
        ExternalNetworkInputPolicy, ExternalNetworkInputSelection, ExternalSupervisorBootstrap,
        external_tls_root_bundle_digest,
    };
    let roots = vec![STANDARD.encode(b"fixture DER root")];
    let recipe = ExternalCandidateRuntimeRecipe {
        schema: 2,
        runtime_mount_destination: "/runtime/product".into(),
        executable_relative_path: "bin/tool".into(),
        argv0: "tool".into(),
        arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
        cwd: "/workspace".into(),
        environment: BTreeMap::new(),
        max_stdout_bytes: 1024 * 1024,
        max_stderr_bytes: 1024 * 1024,
        proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
        contain_process_group: false,
        nested_sandbox: true,
    };
    let recipe_digest = recipe.digest()?;
    let requirement = ExternalCandidateRequirement {
        schema: 6,
        protocol: PROTOCOL.into(),
        connector_protocol: ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
        execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
        required_lifecycle_capabilities: Default::default(),
        provider_declaration_id: "codex-hosted".into(),
        provider_configuration_destination: "environments.toml".into(),
        runtime_product_declaration_id: "product".into(),
        runtime_recipe: recipe,
    };
    let qualification_use =
        ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
            &requirement,
        )?;
    let network_inputs = ExternalNetworkInputPolicy {
        resolver: ExternalNetworkInputSelection {
            source: "/etc/resolv.conf".into(),
            max_bytes: 65_536,
        },
        hosts: ExternalNetworkInputSelection {
            source: "/etc/hosts".into(),
            max_bytes: 65_536,
        },
    };
    ExternalSupervisorBootstrap {
        schema: 7,
        controller: ExternalControllerTransportContract {
            schema: 2,
            network_inputs,
            https_origin: "https://controller.example:7443".into(),
            route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
            tls_root_bundle_digest: external_tls_root_bundle_digest(&roots)?,
            connect_timeout_ms: 5_000,
            request_timeout_ms: 10_000,
            maximum_response_bytes: 1024 * 1024,
        },
        tls_root_certificates_der_base64: roots,
        placement_thread_id: "T-native-test".into(),
        occurrence_id: occurrence_id.into(),
        allocation_request_digest: "2".repeat(64),
        admitted_capsule_hash: "b".repeat(64),
        base_snapshot_hash: inputs.base_snapshot.snapshot_hash.clone(),
        execution_binding_hash: "d".repeat(64),
        supervisor_runtime_hash: "e".repeat(64),
        launcher_artifact_hash: launcher_hash.into(),
        candidate_program: AdmittedExternalCandidateProgram {
            requirement,
            qualification_use,
            runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_manifest_hash: "e".repeat(64),
            runtime_witness_hash: "1".repeat(64),
            qualification_attestation_hash: "2".repeat(64),
            selection_identity_digest: "3".repeat(64),
            runtime_recipe_digest: recipe_digest,
        }
        .into(),
        guest_input_identity: inputs.identity_digest()?,
        guest_inputs: inputs.clone(),
        owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
            &lillux::crypto::SigningKey::from_bytes(&[41; 32]).verifying_key(),
        )?,
        bootstrap_capability: STANDARD.encode([42_u8; 32]),
        attachment_deadline_ms: lillux::time::timestamp_millis() + 60_000,
        execution_timeout_seconds: 60,
        post_execution_timeout_seconds: 120,
        candidate_export_max_bytes: 512 * 1024,
        channel_max_bytes: 1024 * 1024,
    }
    .canonical_bytes()
}

fn occurrence_assignment<'a>(
    base_snapshot_hash: &'a str,
    attachment_deadline_ms: i64,
    guest_runtime_manifest_hash: &'a str,
) -> GuestOccurrenceAssignment<'a> {
    GuestOccurrenceAssignment {
        placement_thread_id: "T-native-test",
        admitted_capsule_hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        base_snapshot_hash,
        execution_binding_hash: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        allocation_request_digest: "2222222222222222222222222222222222222222222222222222222222222222",
        occurrence_id: "occ-native-test",
        activation_request_digest: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        supervisor_runtime_hash: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        guest_runtime_manifest_hash,
        attachment_deadline_ms,
    }
}

fn signed_native_assignment(
    assignment: &GuestOccurrenceAssignment<'_>,
    owner: &lillux::crypto::SigningKey,
) -> Result<Vec<u8>> {
    let document = GuestOccurrenceAssignmentDocument {
        schema: GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
        placement_thread_id: assignment.placement_thread_id.into(),
        admitted_capsule_hash: assignment.admitted_capsule_hash.into(),
        base_snapshot_hash: assignment.base_snapshot_hash.into(),
        execution_binding_hash: assignment.execution_binding_hash.into(),
        allocation_request_digest: assignment.allocation_request_digest.into(),
        occurrence_id: assignment.occurrence_id.into(),
        activation_request_digest: assignment.activation_request_digest.into(),
        supervisor_runtime_hash: assignment.supervisor_runtime_hash.into(),
        guest_runtime_manifest_hash: assignment.guest_runtime_manifest_hash.into(),
        owner_public_key_hex: hex::encode(owner.verifying_key().to_bytes()),
        attachment_deadline_ms: assignment.attachment_deadline_ms,
    };
    let root = lillux::crypto::SigningKey::from_bytes(&[43; 32]);
    let signed = sign_guest_occurrence_assignment(document, &root)?;
    ryeos_external_execution_contract::canonical_json(&signed)
}

fn verify_native_import(
    signed: &SignedGuestImportAuthorization,
    signed_assignment: &[u8],
    installed_runtime_manifest_hash: &str,
) -> Result<ryeos_external_execution::guest_import_authorization::VerifiedGuestImportAuthorization>
{
    let root = lillux::crypto::SigningKey::from_bytes(&[43; 32]);
    verify_guest_import_documents(
        &ryeos_external_execution_contract::canonical_json(signed)?,
        &root.verifying_key(),
        installed_runtime_manifest_hash,
        signed_assignment,
    )
}

fn run(
    fixture_path: &std::path::Path,
    release: bool,
    natural_probe: bool,
    cancel_probe: bool,
) -> Result<()> {
    // Keep durable fixture paths outside /tmp: Lillux replaces /tmp with the
    // process-private source mount after all uploaded bytes are pinned. The
    // invocation's writable working tree is also available on hosts where
    // /var/tmp is deliberately read-only.
    let state_path = fixture_path.join("state");
    std::fs::create_dir(&state_path)?;
    let db = ryeos_state::StateDb::open(&state_path, Arc::new(ryeos_state::TrustStore::new()))?;
    let authority = db.pinned_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
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
            created_at: "2026-09-27T00:00:00Z".into(),
            source: "native-guest-occurrence-test".into(),
        }
        .to_value(),
    )?;
    let transfer = ryeos_project_capture::prepare_project_snapshot_transfer(
        &authority,
        &guard,
        &snapshot_hash,
    )?;
    let measurement = transfer.measurement().clone();
    let transfer_root = transfer
        .descriptor()
        .try_clone_pinned_directory("<native-guest-base>".into())?;
    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let mut directory_entries = vec![GuestStagingEntry::Directory {
        path: "base".into(),
        mode: 0o700,
    }];
    let mut file_entries = Vec::new();
    transfer_root.visit_regular_files_bounded(
        lillux::DirectoryTraversalBudget::new(400_010, 4),
        |relative, is_directory| {
            if is_directory {
                directory_entries.push(GuestStagingEntry::Directory {
                    path: format!("base/{}", relative.to_str().context("non-UTF8 base path")?),
                    mode: 0o700,
                });
            }
            Ok(false)
        },
        |relative, file| {
            let bytes = lillux::read_open_regular_file_bounded(file, 1024 * 1024)?;
            let path = format!("base/{}", relative.to_str().context("non-UTF8 base file")?);
            file_entries.push(GuestStagingEntry::RegularFile {
                path: path.clone(),
                mode: 0o600,
                bytes: bytes.len() as u64,
                sha256: lillux::sha256_hex(&bytes),
            });
            files.insert(path, bytes);
            Ok(())
        },
    )?;
    let mut entries = directory_entries;
    entries.extend(file_entries);
    let configuration = b"native-config";
    let launcher = b"native-launcher";
    let supervisor = b"native-supervisor";
    let inputs = ExternalGuestInputProjection {
        schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
        base_snapshot: GuestBaseSnapshotInput {
            descriptor: 56,
            snapshot_hash: measurement.snapshot_hash.clone(),
            closure_digest: measurement.closure_digest.clone(),
            object_count: measurement.object_count,
            blob_count: measurement.blob_count,
            total_bytes: measurement.total_bytes,
        },
        workspace_outputs: None,
        inputs: vec![GuestMountInput {
            role: GuestMountRole::Configuration,
            authority_id: "native-config".into(),
            descriptor: 64,
            destination: "/runtime/config".into(),
            kind: GuestMountKind::RegularFile,
            access: GuestMountAccess::ReadOnly,
            normalized_mode: Some(0o644),
            content_authority: GuestMountContentAuthority::RawFile {
                sha256: lillux::sha256_hex(configuration),
            },
            bytes: configuration.len() as u64,
        }],
        executable_search: Vec::new(),
        environment: BTreeMap::new(),
    };
    let bootstrap =
        supervisor_bootstrap(&inputs, &lillux::sha256_hex(launcher), "occ-native-test")?;
    for (path, mode, bytes) in [
        ("bootstrap", 0o600, bootstrap.as_slice()),
        ("input-00", 0o644, configuration.as_slice()),
        ("launcher", 0o755, launcher.as_slice()),
        ("supervisor", 0o755, supervisor.as_slice()),
    ] {
        entries.push(GuestStagingEntry::RegularFile {
            path: path.into(),
            mode,
            bytes: bytes.len() as u64,
            sha256: lillux::sha256_hex(bytes),
        });
        files.insert(path.into(), bytes.to_vec());
    }
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    let manifest = GuestStagingPackageManifest {
        schema: GUEST_STAGING_PACKAGE_SCHEMA,
        activation_request_digest: "c".repeat(64),
        guest_input_identity: inputs.identity_digest()?,
        bootstrap_sha256: lillux::sha256_hex(&bootstrap),
        supervisor_sha256: lillux::sha256_hex(supervisor),
        launcher_sha256: lillux::sha256_hex(launcher),
        total_regular_bytes: files.values().map(|bytes| bytes.len() as u64).sum(),
        entries,
    };
    let expected = GuestStagingExpected {
        inputs: &inputs,
        activation_request_digest: &manifest.activation_request_digest,
        bootstrap_sha256: &manifest.bootstrap_sha256,
        supervisor_sha256: &manifest.supervisor_sha256,
        launcher_sha256: &manifest.launcher_sha256,
        maximum_regular_bytes: manifest.total_regular_bytes,
        maximum_framed_bytes: manifest.framed_bytes()? + 128,
    };
    let mut writer = GuestStagingStreamWriter::new(Vec::new(), manifest.clone(), &expected)?;
    while let Some(entry) = writer.next_file() {
        writer.copy_next_file(
            &mut files
                .get(entry.path())
                .context("missing package file")?
                .as_slice(),
        )?;
    }
    let bytes = writer.finish()?;
    let upload_dir = fixture_path.join("upload");
    let occurrence_dir = fixture_path.join("occurrence");
    std::fs::create_dir(&upload_dir)?;
    std::fs::create_dir(&occurrence_dir)?;
    let upload_parent = lillux::PinnedDirectory::open(&upload_dir)?.context("upload vanished")?;
    let occurrence =
        lillux::PinnedDirectory::open(&occurrence_dir)?.context("occurrence vanished")?;
    upload_parent.tighten_owner_private_directory()?;
    occurrence.tighten_owner_private_directory()?;
    let mut output = upload_parent.open_regular_create(OsStr::new("payload"), true, true, 0o600)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    drop(output);
    let upload = upload_parent
        .open_pinned_regular(OsStr::new("payload"), false)?
        .context("uploaded payload vanished")?;
    let ticket = GuestImportTicket {
        schema: GUEST_IMPORT_TICKET_SCHEMA,
        binding_hash: "d".repeat(64),
        allocation_request_digest: "2".repeat(64),
        occurrence_id: "occ-native-test".into(),
        activation_request_digest: manifest.activation_request_digest.clone(),
        guest_input_identity: manifest.guest_input_identity.clone(),
        payload_sha256: lillux::sha256_hex(&bytes),
        manifest_sha256: manifest.identity_digest()?,
        framed_bytes: bytes.len() as u64,
        regular_bytes: manifest.total_regular_bytes,
        bootstrap_sha256: manifest.bootstrap_sha256.clone(),
        supervisor_sha256: manifest.supervisor_sha256.clone(),
        launcher_sha256: manifest.launcher_sha256.clone(),
        maximum_regular_bytes: expected.maximum_regular_bytes,
        maximum_framed_bytes: expected.maximum_framed_bytes,
    };
    let context = GuestImportContext {
        binding_hash: &ticket.binding_hash,
        allocation_request_digest: &ticket.allocation_request_digest,
        occurrence_id: &ticket.occurrence_id,
        activation_request_digest: &ticket.activation_request_digest,
    };
    std::fs::write(
        fixture_path.join("ticket.json"),
        serde_json::to_vec(&ticket)?,
    )?;
    std::fs::write(
        fixture_path.join("inputs.json"),
        serde_json::to_vec(&inputs)?,
    )?;
    let parsed_bootstrap: ryeos_state::external_execution::transport::ExternalSupervisorBootstrap =
        serde_json::from_slice(&bootstrap)?;
    let guest_runtime_path = fixture_path.join("guest-runtime");
    std::fs::create_dir(&guest_runtime_path)?;
    std::fs::write(
        guest_runtime_path.join("controller-root.hex"),
        hex::encode(
            lillux::crypto::SigningKey::from_bytes(&[43; 32])
                .verifying_key()
                .to_bytes(),
        ),
    )?;
    let guest_runtime = lillux::PinnedDirectory::open(&guest_runtime_path)?
        .context("installed native fixture runtime vanished")?;
    let observed_runtime = ObservedGuestRuntime::observe(&guest_runtime)?;
    let assignment = occurrence_assignment(
        &measurement.snapshot_hash,
        parsed_bootstrap.attachment_deadline_ms,
        observed_runtime.manifest_hash(),
    );
    let authorization = GuestImportAuthorization {
        schema: GUEST_IMPORT_AUTHORIZATION_SCHEMA,
        placement_thread_id: assignment.placement_thread_id.into(),
        admitted_capsule_hash: assignment.admitted_capsule_hash.into(),
        base_snapshot_hash: assignment.base_snapshot_hash.into(),
        execution_binding_hash: assignment.execution_binding_hash.into(),
        allocation_request_digest: assignment.allocation_request_digest.into(),
        occurrence_id: assignment.occurrence_id.into(),
        activation_request_digest: assignment.activation_request_digest.into(),
        supervisor_runtime_hash: assignment.supervisor_runtime_hash.into(),
        guest_runtime_manifest_hash: assignment.guest_runtime_manifest_hash.into(),
        attachment_deadline_ms: assignment.attachment_deadline_ms,
        admission_deadline_ms: assignment.attachment_deadline_ms - 1_000,
        nonce_sha256: "8".repeat(64),
        ticket: ticket.clone(),
        guest_inputs: inputs.clone(),
    };
    let controller = lillux::crypto::SigningKey::from_bytes(&[41; 32]);
    let signed = sign_guest_import_authorization(authorization, &controller, &assignment)?;
    let signed_assignment = signed_native_assignment(&assignment, &controller)?;
    std::fs::write(
        fixture_path.join("signed-authorization.json"),
        serde_json::to_vec(&signed)?,
    )?;
    std::fs::write(
        fixture_path.join("signed-assignment.json"),
        &signed_assignment,
    )?;
    std::fs::write(
        fixture_path.join("assignment-deadline.txt"),
        assignment.attachment_deadline_ms.to_string(),
    )?;
    // No test-harness thread exists here. The old /tmp is replaced, while the
    // exact upload and durable occurrence remain pinned outside that mount.
    let source = lillux::sandbox::enter_linux_private_source_filesystem(
        lillux::sandbox::LinuxPrivateSourceLimits {
            max_bytes: 32 * 1024 * 1024,
            max_inodes: 1024,
        },
    )
    .map_err(anyhow::Error::msg)?;
    let source_root = source.root().try_clone()?;
    let mut held = if release {
        let verified = verify_native_import(
            &signed,
            &signed_assignment,
            assignment.guest_runtime_manifest_hash,
        )?;
        let owner = GuestOccurrenceOwner::begin_authorized(&occurrence, verified)?;
        let staged = owner.stage_uploaded_once(
            &upload,
            source,
            &context,
            &inputs,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
        )?;
        let installed = staged.install_base_once(&context, &inputs)?;
        let observation = installed.recheck_for_adoption(&context, &inputs)?;
        ensure!(
            observation.occurrence_id == context.occurrence_id,
            "native owner changed occurrence"
        );
        ensure!(
            observation.stage.manifest_sha256() == ticket.manifest_sha256,
            "native stage changed manifest"
        );
        ensure!(
            lillux::PinnedDirectory::open(std::path::Path::new("/tmp"))?
                .context("private source disappeared")?
                .identity()?
                == source_root.identity()?,
            "sealed source is no longer the exact private mount"
        );
        ensure!(
            std::fs::create_dir("/tmp/post-seal-write")
                .err()
                .and_then(|error| error.raw_os_error())
                == Some(libc::EROFS),
            "native private source did not refuse a new directory with EROFS"
        );
        ensure!(
            source_root
                .create_child(OsStr::new("post-seal-write"), 0o700)
                .is_err(),
            "installed source remained writable"
        );
        let prepared = installed
            .prepare_content_for_adoption(&context, &inputs)?
            .create_private_scratch_once(&context, &inputs)?
            .prepare_launch_artifacts_once(&context, &inputs)?
            .prepare_supervisor_request(&context, &inputs, 10.0)?;
        let committed = prepared.commit_outer_launch_intent(&context, &inputs)?;
        committed
            .prepare_mounted_sandbox_request(&context, &inputs)?
            .prepare_held_in_dedicated_owner()?
    } else {
        prepare_authorized_held_guest_supervisor_once(
            &occurrence,
            &upload,
            &observed_runtime,
            &ryeos_external_execution_contract::canonical_json(&signed)?,
            &signed_assignment,
            source,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            10.0,
        )?
    };
    let receipt = held.mount_preparation_receipt()?;
    ensure!(
        receipt.mount_count == 3,
        "held native target lacks the stage and two sealed network inputs"
    );
    if release {
        let mut released = held.release_once()?;
        let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5));
        if cancel_probe {
            ensure!(
                released
                    .terminate_namespace_for_export_until(lillux::time::MonotonicDeadline::after(
                        lillux::time::Duration::ZERO,
                    ))
                    .is_err(),
                "expired cleanup deadline authorized namespace termination"
            );
            let _terminated = released.terminate_namespace_for_export_until(deadline)?;
            let duplicate = released
                .terminate_namespace_for_export_until(lillux::time::MonotonicDeadline::after(
                    lillux::time::Duration::from_secs(5),
                ))
                .unwrap_err();
            ensure!(
                duplicate
                    .to_string()
                    .contains("not a live owned namespace-init authority"),
                "settled native namespace refused for the wrong reason: {duplicate:#}"
            );
        } else {
            if !natural_probe {
                let applied = loop {
                    if let Some(receipt) = released.try_observe_applied_launch()? {
                        break receipt;
                    }
                    ensure!(
                        !deadline.has_elapsed(),
                        "released supervisor produced no child-origin applied-launch receipt"
                    );
                    lillux::time::sleep(lillux::time::Duration::from_millis(10));
                };
                ensure!(
                    applied.owned_child_pid == receipt.owned_child_pid,
                    "released supervisor changed its held process identity"
                );
                ensure!(
                    released.try_observe_applied_launch()? == Some(applied),
                    "repeated applied-launch point read changed the sole target receipt"
                );
            }
            let dead = loop {
                let observation = if natural_probe {
                    released.try_observe_natural_settlement().map(|settled| {
                        ensure!(
                            settled.is_none(),
                            "placeholder supervisor settled successfully"
                        );
                        Ok(())
                    })
                } else {
                    released.refuse_if_target_exited().map(|()| Ok(()))
                };
                match observation.and_then(|result| result) {
                    Ok(()) => {}
                    Err(error) => break error,
                }
                ensure!(
                    !deadline.has_elapsed(),
                    "placeholder supervisor did not reach terminal refusal"
                );
                lillux::time::sleep(lillux::time::Duration::from_millis(10));
            };
            if natural_probe {
                ensure!(
                    dead.to_string()
                        .contains("did not settle successfully after applied launch"),
                    "natural settlement accepted or misclassified placeholder launch: {dead:#}"
                );
                ensure!(
                    released.try_observe_natural_settlement().is_err(),
                    "consumed terminal target was observed a second time"
                );
            } else {
                ensure!(
                    dead.to_string()
                        .contains("exited before authenticated attachment")
                        && dead.to_string().contains("launch_failure=true"),
                    "released supervisor had an unrelated terminal refusal: {dead:#}"
                );
            }
        }
        drop(released);
    } else {
        drop(held);
    }
    // Native preparation changes this process's mount namespace. The parent
    // test driver retains the host-side fixture and cleans it after exit.
    println!(
        "native guest sealed-source preparation passed; release={release}; natural_probe={natural_probe}; cancel_probe={cancel_probe}"
    );
    Ok(())
}

fn main() {
    if std::env::var("RYEOS_GUEST_OCCURRENCE_NATIVE").as_deref() != Ok("1") {
        println!("native guest occurrence probe skipped (set RYEOS_GUEST_OCCURRENCE_NATIVE=1)");
        return;
    }
    if std::env::var("RYEOS_GUEST_OCCURRENCE_NATIVE_CHILD").as_deref() == Ok("1") {
        let root = std::env::var_os("RYEOS_GUEST_OCCURRENCE_FIXTURE_ROOT")
            .expect("native child fixture root is absent");
        let release = std::env::var("RYEOS_GUEST_OCCURRENCE_NATIVE_RELEASE").as_deref() == Ok("1");
        let natural_probe =
            std::env::var("RYEOS_GUEST_OCCURRENCE_NATURAL_PROBE").as_deref() == Ok("1");
        let cancel_probe =
            std::env::var("RYEOS_GUEST_OCCURRENCE_CANCEL_PROBE").as_deref() == Ok("1");
        if let Err(error) = run(
            std::path::Path::new(&root),
            release,
            natural_probe,
            cancel_probe,
        ) {
            eprintln!("native guest occurrence probe failed: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    for (release, natural_probe, cancel_probe) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let fixture = tempfile::Builder::new()
            .prefix("ryeos-guest-occurrence-native-")
            .tempdir_in(std::env::current_dir().expect("current test directory"))
            .expect("create host-side native fixture");
        let output = lillux::run(lillux::SubprocessRequest {
            cmd: std::env::current_exe()
                .expect("native test binary")
                .to_string_lossy()
                .into_owned(),
            argv0: None,
            args: Vec::new(),
            cwd: None,
            envs: vec![
                ("RYEOS_GUEST_OCCURRENCE_NATIVE".into(), "1".into()),
                ("RYEOS_GUEST_OCCURRENCE_NATIVE_CHILD".into(), "1".into()),
                (
                    "RYEOS_GUEST_OCCURRENCE_NATIVE_RELEASE".into(),
                    if release { "1" } else { "0" }.into(),
                ),
                (
                    "RYEOS_GUEST_OCCURRENCE_NATURAL_PROBE".into(),
                    if natural_probe { "1" } else { "0" }.into(),
                ),
                (
                    "RYEOS_GUEST_OCCURRENCE_CANCEL_PROBE".into(),
                    if cancel_probe { "1" } else { "0" }.into(),
                ),
                (
                    "RYEOS_GUEST_OCCURRENCE_FIXTURE_ROOT".into(),
                    fixture.path().to_string_lossy().into_owned(),
                ),
            ],
            stdin_data: None,
            timeout: 30.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        });
        assert!(
            output.success
                && !output.timed_out
                && !output.stdout_truncated
                && !output.stderr_truncated,
            "native owner failed: exit={} timeout={} stdout={} stderr={}",
            output.exit_code,
            output.timed_out,
            output.stdout,
            output.stderr
        );
        let ticket: GuestImportTicket = serde_json::from_slice(
            &std::fs::read(fixture.path().join("ticket.json")).expect("retained import ticket"),
        )
        .expect("decode retained import ticket");
        let inputs: ExternalGuestInputProjection = serde_json::from_slice(
            &std::fs::read(fixture.path().join("inputs.json")).expect("retained guest inputs"),
        )
        .expect("decode retained guest inputs");
        let signed: SignedGuestImportAuthorization = serde_json::from_slice(
            &std::fs::read(fixture.path().join("signed-authorization.json"))
                .expect("retained signed guest authorization"),
        )
        .expect("decode signed guest authorization");
        assert_eq!(signed.authorization.ticket, ticket);
        assert_eq!(signed.authorization.guest_inputs, inputs);
        let attachment_deadline_ms: i64 =
            std::fs::read_to_string(fixture.path().join("assignment-deadline.txt"))
                .expect("retained fixture assignment deadline")
                .parse()
                .expect("decode fixture assignment deadline");
        let assignment = occurrence_assignment(
            &inputs.base_snapshot.snapshot_hash,
            attachment_deadline_ms,
            &signed.authorization.guest_runtime_manifest_hash,
        );
        let controller = lillux::crypto::SigningKey::from_bytes(&[41; 32]);
        let signed_assignment = std::fs::read(fixture.path().join("signed-assignment.json"))
            .expect("retained root-signed native occurrence assignment");
        let mut competing_authorization = signed.authorization.clone();
        competing_authorization.nonce_sha256 = "9".repeat(64);
        let competing_signed =
            sign_guest_import_authorization(competing_authorization, &controller, &assignment)
                .expect("sign competing native occurrence authorization");
        let competing_verified = verify_native_import(
            &competing_signed,
            &signed_assignment,
            assignment.guest_runtime_manifest_hash,
        )
        .expect("verify competing native occurrence authorization");
        let verified = verify_native_import(
            &signed,
            &signed_assignment,
            assignment.guest_runtime_manifest_hash,
        )
        .expect("verify retained guest authorization");
        let occurrence = lillux::PinnedDirectory::open(&fixture.path().join("occurrence"))
            .expect("open retained occurrence")
            .expect("retained occurrence exists");
        assert_eq!(
            recover_guest_occurrence_authorized(&occurrence, &verified)
                .expect("recover completed owner record")
                .phase(),
            &GuestOccurrenceRecoveryPhase::LaunchUncertain,
            "held preparation cannot authorize a second launch after owner exit"
        );
        let competing_refusal =
            recover_guest_occurrence_authorized(&occurrence, &competing_verified)
                .err()
                .expect("a fresh signed nonce adopted the first import owner's durable record");
        assert!(
            competing_refusal
                .to_string()
                .contains("guest occurrence owner differs from retained placement"),
            "competing authorization refused at the wrong boundary: {competing_refusal:#}"
        );
        assert!(
            GuestOccurrenceOwner::begin_authorized(&occurrence, verified).is_err(),
            "completed native occurrence admitted a second owner"
        );
        print!("{}", output.stdout);
    }
}
