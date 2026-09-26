//! Opt-in, single-threaded production source-owner component probe.
//! `RYEOS_GUEST_OCCURRENCE_NATIVE=1 cargo test ... --test guest_occurrence_owner_native`
//! requires a host that admits Lillux user/mount namespaces. This is not a
//! supervisor, Render, or Codex qualification result.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution::guest_installation::{
    GuestOccurrenceOwner, GuestOccurrenceRecoveryPhase, recover_guest_occurrence,
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

fn run() -> Result<()> {
    // Keep durable fixture paths outside /tmp: Lillux replaces /tmp with the
    // process-private source mount after all uploaded bytes are pinned. The
    // invocation's writable working tree is also available on hosts where
    // /var/tmp is deliberately read-only.
    let fixture = tempfile::Builder::new()
        .prefix("ryeos-guest-occurrence-native-")
        .tempdir_in(std::env::current_dir()?)?;
    let state_path = fixture.path().join("state");
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
    let bootstrap = b"native-bootstrap";
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
        bootstrap_sha256: lillux::sha256_hex(bootstrap),
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
    let upload_dir = fixture.path().join("upload");
    let occurrence_dir = fixture.path().join("occurrence");
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
        binding_hash: "1".repeat(64),
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
    let owner = GuestOccurrenceOwner::begin(&occurrence, &ticket, &context, &inputs)?;
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
    drop(installed);
    ensure!(
        GuestOccurrenceOwner::begin(&occurrence, &ticket, &context, &inputs).is_err(),
        "completed native import admitted a second owner"
    );
    ensure!(
        recover_guest_occurrence(&occurrence, &ticket, &context, &inputs)?.phase()
            == &GuestOccurrenceRecoveryPhase::InstallationUncertain,
        "dropped native custody became adoptable"
    );
    println!("native guest occurrence source stage, seal, and custody-drop recovery passed");
    Ok(())
}

fn main() {
    if std::env::var("RYEOS_GUEST_OCCURRENCE_NATIVE").as_deref() != Ok("1") {
        println!("native guest occurrence probe skipped (set RYEOS_GUEST_OCCURRENCE_NATIVE=1)");
        return;
    }
    if let Err(error) = run() {
        eprintln!("native guest occurrence probe failed: {error:#}");
        std::process::exit(1);
    }
}
