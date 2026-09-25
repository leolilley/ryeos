//! Descriptor-relative, private staging of one external guest-input package.
//!
//! This is not a Render adapter, a qualification verdict, or a Ready claim.
//! It checks the wire and exact base CAS closure before returning a pinned
//! private tree. Product/source authority and fixed-FD supervisor installation
//! remain separate joined checks.

use std::ffi::{OsStr, OsString};
use std::io::Read;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::staging_package::{
    GuestStagingEntry, GuestStagingExpected, GuestStagingPackageManifest, GuestStagingStreamReader,
    MAX_GUEST_STAGING_ENTRIES,
};

pub struct StagedGuestPackage {
    parent: lillux::PinnedDirectory,
    name: OsString,
    root: lillux::PinnedDirectory,
    manifest: GuestStagingPackageManifest,
    base: ryeos_project_capture::ProjectSnapshotTransferMeasurement,
}

impl StagedGuestPackage {
    pub fn root(&self) -> &lillux::PinnedDirectory {
        &self.root
    }

    pub fn manifest(&self) -> &GuestStagingPackageManifest {
        &self.manifest
    }

    pub fn base(&self) -> &ryeos_project_capture::ProjectSnapshotTransferMeasurement {
        &self.base
    }

    /// Explicitly discard the exact staged generation. The eventual fixed-FD
    /// adoption path must take ownership of this object and call this after
    /// whole-scope settlement; dropping it does not silently delete live input.
    pub fn discard(self) -> Result<()> {
        remove_staged_generation(&self.parent, &self.name, &self.root)
    }
}

/// Consume an immutable uploaded regular file into a newly created private
/// generation. The caller must provide an owner-private staging parent and
/// derive `expected` independently from the uploaded package. On any error,
/// this function attempts bounded exact-generation cleanup and never returns
/// the partial tree.
pub fn stage_guest_package<R: Read>(
    reader: R,
    parent: &lillux::PinnedDirectory,
    expected: &GuestStagingExpected<'_>,
) -> Result<StagedGuestPackage> {
    parent.require_owner_private_directory()?;
    let owner = parent.try_clone()?;
    let (name, stage) = parent.create_unique_child("guest-input", 0o700)?;
    let result = stage
        .require_owner_private_directory()
        .and_then(|()| stage_guest_package_in_private_root(reader, &stage, expected));
    match result {
        Ok((manifest, base)) => Ok(StagedGuestPackage {
            parent: owner,
            name,
            root: stage,
            manifest,
            base,
        }),
        Err(error) => {
            let cleanup = remove_staged_generation(&owner, &name, &stage);
            if let Err(cleanup_error) = cleanup {
                return Err(
                    error.context(format!("guest staging cleanup failed: {cleanup_error:#}"))
                );
            }
            Err(error)
        }
    }
}

fn remove_staged_generation(
    parent: &lillux::PinnedDirectory,
    name: &OsStr,
    root: &lillux::PinnedDirectory,
) -> Result<()> {
    root.remove_contents_recursive_bounded(staging_budget())?;
    ensure!(
        parent.remove_empty_child_if_same(name, root)?,
        "guest staging generation changed before cleanup"
    );
    Ok(())
}

fn stage_guest_package_in_private_root<R: Read>(
    reader: R,
    stage: &lillux::PinnedDirectory,
    expected: &GuestStagingExpected<'_>,
) -> Result<(
    GuestStagingPackageManifest,
    ryeos_project_capture::ProjectSnapshotTransferMeasurement,
)> {
    ensure!(
        stage.entries_no_follow_bounded(0)?.is_empty(),
        "guest staging generation is not empty"
    );
    let mut stream = GuestStagingStreamReader::new(reader, expected)?;
    let manifest = stream.manifest().clone();
    for entry in &manifest.entries {
        let path = entry.path();
        let (parent_path, child_name) = match path.rsplit_once('/') {
            Some((parent_path, name)) => (Some(parent_path), name),
            None => (None, path),
        };
        let directory = open_staged_parent(stage, parent_path)?;
        match entry {
            GuestStagingEntry::Directory { mode, .. } => {
                let created = directory.create_child(OsStr::new(child_name), 0o700)?;
                created.set_mode(*mode)?;
            }
            GuestStagingEntry::RegularFile { mode, .. } => {
                ensure!(
                    stream.next_file().is_some_and(|next| next.path() == path),
                    "guest staging file order changed"
                );
                let mut file =
                    directory.open_regular_create(OsStr::new(child_name), true, true, 0o600)?;
                stream.copy_next_file(&mut file)?;
                lillux::set_open_regular_file_mode(&file, *mode)?;
                file.sync_all()?;
            }
        }
    }
    stream.finish()?;

    let base_root = stage
        .open_child_directory(OsStr::new("base"))?
        .context("guest base transfer root is absent")?;
    let base = ryeos_project_capture::inspect_project_snapshot_transfer(
        &base_root,
        &expected.inputs.base_snapshot.snapshot_hash,
    )?;
    let admitted = &expected.inputs.base_snapshot;
    ensure!(
        base.snapshot_hash == admitted.snapshot_hash
            && base.closure_digest == admitted.closure_digest
            && base.object_count == admitted.object_count
            && base.blob_count == admitted.blob_count
            && base.total_bytes == admitted.total_bytes,
        "guest base transfer contradicts the retained snapshot measurement"
    );
    stage.sync_tree_bounded(staging_budget())?;
    Ok((manifest, base))
}

/// Reopen each component through the retained root descriptor. Holding one
/// directory per manifest entry would turn the entry bound into a file-
/// descriptor exhaustion path; the contract already limits depth to 32.
fn open_staged_parent(
    stage: &lillux::PinnedDirectory,
    parent_path: Option<&str>,
) -> Result<lillux::PinnedDirectory> {
    let mut directory = stage.try_clone()?;
    if let Some(path) = parent_path {
        for component in path.split('/') {
            directory = directory
                .open_child_directory(OsStr::new(component))?
                .context("guest staging parent directory is absent")?;
        }
    }
    Ok(directory)
}

fn staging_budget() -> lillux::DirectoryTraversalBudget {
    lillux::DirectoryTraversalBudget::new(MAX_GUEST_STAGING_ENTRIES, 32)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use ryeos_external_execution_contract::staging_package::{
        GUEST_STAGING_PACKAGE_SCHEMA, GuestStagingStreamWriter,
    };
    use ryeos_external_execution_contract::{
        EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalGuestInputProjection,
        GuestBaseSnapshotInput, GuestMountAccess, GuestMountContentAuthority, GuestMountInput,
        GuestMountKind, GuestMountRole,
    };

    #[test]
    fn malformed_base_closure_never_escapes_private_staging() {
        let bootstrap = b"boot".as_slice();
        let configuration = b"cfg".as_slice();
        let launcher = b"start".as_slice();
        let supervisor = b"runrun".as_slice();
        let inputs = ExternalGuestInputProjection {
            schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 56,
                snapshot_hash: "a".repeat(64),
                closure_digest: "b".repeat(64),
                object_count: 3,
                blob_count: 0,
                total_bytes: 0,
            },
            workspace_outputs: None,
            inputs: vec![GuestMountInput {
                role: GuestMountRole::Configuration,
                authority_id: "config".into(),
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
        let manifest = GuestStagingPackageManifest {
            schema: GUEST_STAGING_PACKAGE_SCHEMA,
            activation_request_digest: "c".repeat(64),
            guest_input_identity: inputs.identity_digest().unwrap(),
            bootstrap_sha256: lillux::sha256_hex(bootstrap),
            supervisor_sha256: lillux::sha256_hex(supervisor),
            launcher_sha256: lillux::sha256_hex(launcher),
            total_regular_bytes: (bootstrap.len()
                + configuration.len()
                + launcher.len()
                + supervisor.len()) as u64,
            entries: vec![
                GuestStagingEntry::Directory {
                    path: "base".into(),
                    mode: 0o700,
                },
                GuestStagingEntry::RegularFile {
                    path: "bootstrap".into(),
                    mode: 0o600,
                    bytes: bootstrap.len() as u64,
                    sha256: lillux::sha256_hex(bootstrap),
                },
                GuestStagingEntry::RegularFile {
                    path: "input-00".into(),
                    mode: 0o644,
                    bytes: configuration.len() as u64,
                    sha256: lillux::sha256_hex(configuration),
                },
                GuestStagingEntry::RegularFile {
                    path: "launcher".into(),
                    mode: 0o755,
                    bytes: launcher.len() as u64,
                    sha256: lillux::sha256_hex(launcher),
                },
                GuestStagingEntry::RegularFile {
                    path: "supervisor".into(),
                    mode: 0o755,
                    bytes: supervisor.len() as u64,
                    sha256: lillux::sha256_hex(supervisor),
                },
            ],
        };
        let expected = GuestStagingExpected {
            inputs: &inputs,
            activation_request_digest: &manifest.activation_request_digest,
            bootstrap_sha256: &manifest.bootstrap_sha256,
            supervisor_sha256: &manifest.supervisor_sha256,
            launcher_sha256: &manifest.launcher_sha256,
            maximum_regular_bytes: manifest.total_regular_bytes,
            maximum_framed_bytes: manifest.framed_bytes().unwrap(),
        };
        let mut stream =
            GuestStagingStreamWriter::new(Vec::new(), manifest.clone(), &expected).unwrap();
        for payload in [bootstrap, configuration, launcher, supervisor] {
            stream.copy_next_file(&mut payload.as_ref()).unwrap();
        }
        let bytes = stream.finish().unwrap();
        let private = tempfile::tempdir().unwrap();
        let parent = lillux::PinnedDirectory::open(private.path())
            .unwrap()
            .unwrap();
        parent.tighten_owner_private_directory().unwrap();
        let error = stage_guest_package(bytes.as_slice(), &parent, &expected)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("snapshot"));
        assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
    }

    #[test]
    fn exact_base_closure_can_be_staged_without_publishing_it() {
        let state = tempfile::tempdir().unwrap();
        let db = ryeos_state::StateDb::open(state.path(), Arc::new(ryeos_state::TrustStore::new()))
            .unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let cas = authority.cas_store().unwrap();
        let policy = ryeos_state::objects::ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            BTreeMap::new(),
        )
        .unwrap();
        let policy_hash = cas.store_object(&policy.to_value()).unwrap();
        let tree = ryeos_state::objects::ProjectTree {
            files: BTreeMap::new(),
        };
        let tree_hash = cas.store_object(&tree.to_value()).unwrap();
        let snapshot = ryeos_state::objects::ProjectSnapshot {
            project_tree_hash: tree_hash,
            effective_policy_hash: policy_hash,
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-09-26T00:00:00Z".into(),
            source: "guest-staging-test".into(),
        };
        let snapshot_hash = cas.store_object(&snapshot.to_value()).unwrap();
        let transfer = ryeos_project_capture::prepare_project_snapshot_transfer(
            &authority,
            &guard,
            &snapshot_hash,
        )
        .unwrap();
        let measurement = transfer.measurement().clone();
        let transfer_root = transfer
            .descriptor()
            .try_clone_pinned_directory("<guest-staging-test-base>".into())
            .unwrap();
        let mut files = BTreeMap::<String, Vec<u8>>::new();
        let mut directory_entries = vec![GuestStagingEntry::Directory {
            path: "base".into(),
            mode: 0o700,
        }];
        let mut file_entries = Vec::new();
        transfer_root
            .visit_regular_files_bounded(
                lillux::DirectoryTraversalBudget::new(400_010, 4),
                |relative, is_directory| {
                    if is_directory {
                        directory_entries.push(GuestStagingEntry::Directory {
                            path: format!("base/{}", relative.to_str().unwrap()),
                            mode: 0o700,
                        });
                    }
                    Ok(false)
                },
                |relative, file| {
                    let bytes = lillux::read_open_regular_file_bounded(file, 1024 * 1024).unwrap();
                    let path = format!("base/{}", relative.to_str().unwrap());
                    file_entries.push(GuestStagingEntry::RegularFile {
                        path: path.clone(),
                        mode: 0o600,
                        bytes: bytes.len() as u64,
                        sha256: lillux::sha256_hex(&bytes),
                    });
                    files.insert(path, bytes);
                    Ok(())
                },
            )
            .unwrap();
        let mut entries = directory_entries;
        entries.extend(file_entries);
        let bootstrap = b"boot".to_vec();
        let config = b"cfg".to_vec();
        let launcher = b"start".to_vec();
        let supervisor = b"runrun".to_vec();
        for (path, mode, bytes) in [
            ("bootstrap", 0o600, &bootstrap),
            ("input-00", 0o644, &config),
            ("launcher", 0o755, &launcher),
            ("supervisor", 0o755, &supervisor),
        ] {
            entries.push(GuestStagingEntry::RegularFile {
                path: path.into(),
                mode,
                bytes: bytes.len() as u64,
                sha256: lillux::sha256_hex(bytes),
            });
            files.insert(path.into(), bytes.clone());
        }
        entries.sort_by(|left, right| left.path().cmp(right.path()));
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
                authority_id: "config".into(),
                descriptor: 64,
                destination: "/runtime/config".into(),
                kind: GuestMountKind::RegularFile,
                access: GuestMountAccess::ReadOnly,
                normalized_mode: Some(0o644),
                content_authority: GuestMountContentAuthority::RawFile {
                    sha256: lillux::sha256_hex(&config),
                },
                bytes: config.len() as u64,
            }],
            executable_search: Vec::new(),
            environment: BTreeMap::new(),
        };
        let manifest = GuestStagingPackageManifest {
            schema: GUEST_STAGING_PACKAGE_SCHEMA,
            activation_request_digest: "c".repeat(64),
            guest_input_identity: inputs.identity_digest().unwrap(),
            bootstrap_sha256: lillux::sha256_hex(&bootstrap),
            supervisor_sha256: lillux::sha256_hex(&supervisor),
            launcher_sha256: lillux::sha256_hex(&launcher),
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
            maximum_framed_bytes: manifest.framed_bytes().unwrap(),
        };
        let mut stream =
            GuestStagingStreamWriter::new(Vec::new(), manifest.clone(), &expected).unwrap();
        while let Some(entry) = stream.next_file() {
            let mut source = files.get(entry.path()).unwrap().as_slice();
            stream.copy_next_file(&mut source).unwrap();
        }
        let bytes = stream.finish().unwrap();
        let private = tempfile::tempdir().unwrap();
        let parent = lillux::PinnedDirectory::open(private.path())
            .unwrap()
            .unwrap();
        parent.tighten_owner_private_directory().unwrap();
        let mut truncated = bytes.clone();
        truncated.pop();
        let mut tampered = bytes.clone();
        *tampered.last_mut().unwrap() ^= 1;
        let mut trailing = bytes.clone();
        trailing.push(0);
        for invalid in [truncated, tampered, trailing] {
            assert!(stage_guest_package(invalid.as_slice(), &parent, &expected).is_err());
            assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
        }
        let staged = stage_guest_package(bytes.as_slice(), &parent, &expected).unwrap();
        assert_eq!(staged.base(), &measurement);
        assert_eq!(staged.manifest(), &manifest);
        assert!(
            staged
                .root()
                .open_child_directory(OsStr::new("base"))
                .unwrap()
                .is_some()
        );
        staged.discard().unwrap();
        assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
    }
}
