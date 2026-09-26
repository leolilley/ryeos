//! Descriptor-relative, private staging of one external guest-input package.
//!
//! This is not a Render adapter, a qualification verdict, or a Ready claim.
//! It checks the wire and exact base CAS closure before returning a pinned
//! private tree. Product/source authority and fixed-FD supervisor installation
//! remain separate joined checks.

use std::collections::BTreeMap;
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

/// Import the exact uploaded inode named by an independently retained byte
/// count and digest. Lillux performs positional, mutation-checked reads; a
/// full-stream digest mismatch discards even an otherwise valid staged tree.
pub fn stage_uploaded_guest_package(
    upload: &lillux::PinnedRegularFile,
    upload_bytes: u64,
    upload_sha256: &str,
    parent: &lillux::PinnedDirectory,
    expected: &GuestStagingExpected<'_>,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<StagedGuestPackage> {
    ensure!(
        !deadline.has_elapsed(),
        "guest upload staging deadline expired"
    );
    let mut reader =
        upload.stable_reader_exact(upload_bytes, upload_sha256, expected.maximum_framed_bytes)?;
    let staged = stage_guest_package(
        &mut DeadlineReader {
            inner: &mut reader,
            deadline,
        },
        parent,
        expected,
    )?;
    if let Err(error) = reader.finish() {
        if let Err(cleanup_error) = staged.discard() {
            return Err(error.context(format!(
                "uploaded guest staging cleanup failed: {cleanup_error:#}"
            )));
        }
        return Err(error);
    }
    if deadline.has_elapsed() {
        if let Err(cleanup_error) = staged.discard() {
            anyhow::bail!(
                "guest upload staging deadline expired and cleanup failed: {cleanup_error:#}"
            );
        }
        anyhow::bail!("guest upload staging deadline expired");
    }
    Ok(staged)
}

struct DeadlineReader<'a, R> {
    inner: &'a mut R,
    deadline: lillux::time::MonotonicDeadline,
}

impl<R: Read> Read for DeadlineReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.deadline.has_elapsed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "guest upload staging deadline expired",
            ));
        }
        self.inner.read(buffer)
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
    let mut product_symlinks = BTreeMap::<&str, Vec<(&str, &str)>>::new();
    for entry in &manifest.entries {
        if let GuestStagingEntry::Symlink { path, target } = entry {
            let (root, relative) = path
                .split_once('/')
                .context("guest product symlink has no relative path")?;
            product_symlinks
                .entry(root)
                .or_default()
                .push((relative, target));
        }
    }
    for links in product_symlinks.values() {
        ryeos_state::objects::validate_internal_symlink_graph(links.iter().copied())?;
    }
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
            GuestStagingEntry::Symlink { target, .. } => {
                directory.create_symlink(OsStr::new(child_name), target.as_bytes())?;
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
    stage.sync_tree_with_symlinks_bounded(staging_budget(), 4096)?;
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
    use std::io::Write;
    use std::sync::Arc;

    use super::*;
    use ryeos_external_execution_contract::staging_package::{
        GUEST_STAGING_PACKAGE_SCHEMA, GuestStagingStreamWriter,
    };
    use ryeos_external_execution_contract::{
        EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalGuestInputProjection,
        GuestBaseSnapshotInput, GuestMountAccess, GuestMountContentAuthority, GuestMountInput,
        GuestMountKind, GuestMountRole, GuestProductManifestKind,
    };
    use ryeos_state::objects::*;

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
        let product_manifest_object = ryeos_state::objects::ExternalContentManifestObject {
            schema: ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA.into(),
            kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            entries: vec![
                ryeos_state::objects::ExternalContentManifestEntry {
                    path: "bin".into(),
                    kind: ryeos_state::objects::ExternalContentManifestEntryKind::Dir,
                    mode: None,
                    blob_hash: None,
                    size: None,
                    target: None,
                },
                ryeos_state::objects::ExternalContentManifestEntry {
                    path: "bin/tool".into(),
                    kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
                    mode: Some(0o755),
                    blob_hash: Some(lillux::sha256_hex(b"tool")),
                    size: Some(4),
                    target: None,
                },
                ryeos_state::objects::ExternalContentManifestEntry {
                    path: "current".into(),
                    kind: ryeos_state::objects::ExternalContentManifestEntryKind::Symlink,
                    mode: None,
                    blob_hash: None,
                    size: None,
                    target: Some("bin/tool".into()),
                },
            ],
            entry_count: 3,
            total_bytes: 4,
        };
        product_manifest_object.validate().unwrap();
        let product_manifest =
            lillux::canonical_json(&serde_json::to_value(&product_manifest_object).unwrap())
                .unwrap()
                .into_bytes();
        let source_manifest = SourceClosureManifest::new(
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
        let source_binding = EffectiveSourceBinding {
            schema: EFFECTIVE_SOURCE_BINDING_SCHEMA,
            kind: EFFECTIVE_SOURCE_BINDING_KIND.into(),
            owner: SourceOwnerIdentity {
                canonical_ref: "tool:test/run".into(),
                item_kind: "tool".into(),
                source_space: SourceSpaceIdentity::Project,
                source_root: SourceRootIdentity::Project,
                root_source_content_digest: "a".repeat(64),
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
                schema_document: serde_json::json!({"kind": "kind", "location": {"directory": "tools"}}),
                normalized_declaration: serde_json::json!({
                    "derived": SOURCE_CLOSURE_DERIVED_KEY,
                    "location": {"type": "item_namespace"}, "testimony": "owner_signed_files",
                    "max_files": 8, "max_total_bytes": 1024, "max_file_bytes": 512, "max_depth": 8,
                }),
                root_kind_format: serde_json::json!({"extensions": ["yaml"]}),
                root_signature_envelope: serde_json::json!({"style": "header"}),
            },
            content_manifest_hash: source_manifest.digest().unwrap(),
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
        let source_binding_bytes =
            lillux::canonical_json(&serde_json::to_value(&source_binding).unwrap())
                .unwrap()
                .into_bytes();
        let source_manifest_bytes =
            lillux::canonical_json(&serde_json::to_value(&source_manifest).unwrap())
                .unwrap()
                .into_bytes();
        let verified_source =
            ryeos_state::source_verification::VerifiedAdmittedSourceRecords::from_canonical_bytes(
                &lillux::sha256_hex(&source_binding_bytes),
                &lillux::sha256_hex(&source_manifest_bytes),
                &source_binding_bytes,
                &source_manifest_bytes,
            )
            .unwrap();
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
        entries.extend([
            GuestStagingEntry::Directory {
                path: "input-01".into(),
                mode: 0o700,
            },
            GuestStagingEntry::Directory {
                path: "input-01/bin".into(),
                mode: 0o755,
            },
            GuestStagingEntry::RegularFile {
                path: "input-01/bin/tool".into(),
                mode: 0o755,
                bytes: 4,
                sha256: lillux::sha256_hex(b"tool"),
            },
            GuestStagingEntry::Symlink {
                path: "input-01/current".into(),
                target: "bin/tool".into(),
            },
            GuestStagingEntry::RegularFile {
                path: "record-00".into(),
                mode: 0o600,
                bytes: product_manifest.len() as u64,
                sha256: lillux::sha256_hex(&product_manifest),
            },
            GuestStagingEntry::Directory {
                path: "input-02".into(),
                mode: 0o700,
            },
            GuestStagingEntry::RegularFile {
                path: "input-02/run.py".into(),
                mode: 0o644,
                bytes: 3,
                sha256: lillux::sha256_hex(b"run"),
            },
        ]);
        files.insert("input-01/bin/tool".into(), b"tool".to_vec());
        files.insert("record-00".into(), product_manifest.clone());
        files.insert("input-02/run.py".into(), b"run".to_vec());
        for (path, bytes) in [
            ("record-01", &source_binding_bytes),
            ("record-02", &source_manifest_bytes),
        ] {
            entries.push(GuestStagingEntry::RegularFile {
                path: path.into(),
                mode: 0o600,
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
            inputs: vec![
                GuestMountInput {
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
                },
                GuestMountInput {
                    role: GuestMountRole::Product,
                    authority_id: "product".into(),
                    descriptor: 65,
                    destination: "/runtime/product".into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: None,
                    content_authority: GuestMountContentAuthority::ProductManifest {
                        manifest_kind: GuestProductManifestKind::Content,
                        manifest_hash: lillux::sha256_hex(&product_manifest),
                        manifest_descriptor: 66,
                        manifest_bytes: product_manifest.len() as u64,
                    },
                    bytes: 4,
                },
                GuestMountInput {
                    role: GuestMountRole::Source,
                    authority_id: verified_source.binding_hash().into(),
                    descriptor: 67,
                    destination: verified_source
                        .runtime_destination()
                        .to_str()
                        .unwrap()
                        .into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: None,
                    content_authority: GuestMountContentAuthority::SourceClosure {
                        binding_descriptor: 68,
                        binding_hash: lillux::sha256_hex(&source_binding_bytes),
                        binding_bytes: source_binding_bytes.len() as u64,
                        manifest_descriptor: 69,
                        manifest_hash: lillux::sha256_hex(&source_manifest_bytes),
                        manifest_bytes: source_manifest_bytes.len() as u64,
                    },
                    bytes: source_manifest.totals.total_bytes,
                },
            ],
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
            maximum_framed_bytes: manifest.framed_bytes().unwrap() + 128,
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
        for unsafe_target in ["../../escape", "/etc/passwd", "current"] {
            let mut invalid = manifest.clone();
            for entry in &mut invalid.entries {
                if let GuestStagingEntry::Symlink { target, .. } = entry {
                    *target = unsafe_target.into();
                }
            }
            let mut invalid_stream =
                GuestStagingStreamWriter::new(Vec::new(), invalid, &expected).unwrap();
            while let Some(entry) = invalid_stream.next_file() {
                let mut source = files.get(entry.path()).unwrap().as_slice();
                invalid_stream.copy_next_file(&mut source).unwrap();
            }
            let invalid_bytes = invalid_stream.finish().unwrap();
            let error = stage_guest_package(invalid_bytes.as_slice(), &parent, &expected)
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("symlink"));
            assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
        }
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
        let upload_dir = tempfile::tempdir().unwrap();
        let upload_parent = lillux::PinnedDirectory::open(upload_dir.path())
            .unwrap()
            .unwrap();
        upload_parent.tighten_owner_private_directory().unwrap();
        let mut upload_file = upload_parent
            .open_regular_create(OsStr::new("payload"), true, true, 0o600)
            .unwrap();
        upload_file.write_all(&bytes).unwrap();
        upload_file.sync_all().unwrap();
        drop(upload_file);
        let upload = upload_parent
            .open_pinned_regular(OsStr::new("payload"), false)
            .unwrap()
            .unwrap();
        assert!(
            stage_uploaded_guest_package(
                &upload,
                bytes.len() as u64,
                &"0".repeat(64),
                &parent,
                &expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .is_err()
        );
        assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
        let staged = stage_uploaded_guest_package(
            &upload,
            bytes.len() as u64,
            &lillux::sha256_hex(&bytes),
            &parent,
            &expected,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
        )
        .unwrap();
        assert_eq!(staged.base(), &measurement);
        assert_eq!(staged.manifest(), &manifest);
        crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        let config_source = staged
            .root()
            .open_pinned_regular(OsStr::new("input-00"), false)
            .unwrap()
            .unwrap();
        config_source.set_mode(0o600).unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        config_source.set_mode(0o644).unwrap();
        let mut writable_config = staged
            .root()
            .open_regular(OsStr::new("input-00"), true)
            .unwrap()
            .unwrap();
        writable_config.write_all(b"bad").unwrap();
        writable_config.sync_all().unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        use std::io::Seek as _;
        writable_config.rewind().unwrap();
        writable_config.write_all(&config).unwrap();
        writable_config.sync_all().unwrap();
        crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        let mut writable_record = staged
            .root()
            .open_regular(OsStr::new("record-00"), true)
            .unwrap()
            .unwrap();
        writable_record.write_all(b"x").unwrap();
        writable_record.sync_all().unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        writable_record.rewind().unwrap();
        writable_record.write_all(&product_manifest).unwrap();
        writable_record.sync_all().unwrap();
        crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        let mut writable_source = staged
            .root()
            .open_child_directory(OsStr::new("input-02"))
            .unwrap()
            .unwrap()
            .open_regular(OsStr::new("run.py"), true)
            .unwrap()
            .unwrap();
        writable_source.write_all(b"bad").unwrap();
        writable_source.sync_all().unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        writable_source.rewind().unwrap();
        writable_source.write_all(b"run").unwrap();
        writable_source.sync_all().unwrap();
        crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        for (name, original) in [
            ("record-01", &source_binding_bytes),
            ("record-02", &source_manifest_bytes),
        ] {
            let mut writable = staged
                .root()
                .open_regular(OsStr::new(name), true)
                .unwrap()
                .unwrap();
            writable.write_all(b"x").unwrap();
            writable.sync_all().unwrap();
            assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
            writable.rewind().unwrap();
            writable.write_all(original).unwrap();
            writable.sync_all().unwrap();
            crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        }

        // A controller package is produced from retained descriptors, never
        // from the diagnostic paths used to construct this test fixture.
        let source_fixture = tempfile::tempdir().unwrap();
        for entry in &manifest.entries {
            let path = entry.path();
            if path == "base" || path.starts_with("base/") {
                continue;
            }
            let destination = source_fixture.path().join(path);
            match entry {
                GuestStagingEntry::Directory { mode, .. } => {
                    std::fs::create_dir_all(&destination).unwrap();
                    std::fs::set_permissions(
                        &destination,
                        std::os::unix::fs::PermissionsExt::from_mode(*mode),
                    )
                    .unwrap();
                }
                GuestStagingEntry::RegularFile { mode, .. } => {
                    std::fs::write(&destination, files.get(path).unwrap()).unwrap();
                    std::fs::set_permissions(
                        &destination,
                        std::os::unix::fs::PermissionsExt::from_mode(*mode),
                    )
                    .unwrap();
                }
                GuestStagingEntry::Symlink { target, .. } => {
                    std::os::unix::fs::symlink(target, &destination).unwrap();
                }
            }
        }
        let source_root = lillux::PinnedDirectory::open(source_fixture.path())
            .unwrap()
            .unwrap();
        let inherited_file = |name: &str| {
            source_root
                .open_inherited_regular(OsStr::new(name), false)
                .unwrap()
                .unwrap()
        };
        // The installed signed-bundle resolver passes sealed executable
        // captures (0500), not the mode of the original bundle files.
        let supervisor_capture =
            lillux::sealed_executable_memfd(c"test-supervisor", files.get("supervisor").unwrap())
                .unwrap();
        let launcher_capture =
            lillux::sealed_executable_memfd(c"test-launcher", files.get("launcher").unwrap())
                .unwrap();
        let config_authority = inherited_file("input-00");
        let product_authority = source_root
            .open_child_directory(OsStr::new("input-01"))
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let source_authority = source_root
            .open_child_directory(OsStr::new("input-02"))
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let records = ["record-00", "record-01", "record-02"]
            .into_iter()
            .map(inherited_file)
            .collect::<Vec<_>>();
        let mut retained_inputs = inputs.clone();
        retained_inputs.base_snapshot.descriptor =
            transfer.descriptor().inherited_descriptor().unwrap();
        for (input, authority) in retained_inputs.inputs.iter_mut().zip([
            &config_authority,
            &product_authority,
            &source_authority,
        ]) {
            input.descriptor = authority.inherited_descriptor().unwrap();
        }
        if let GuestMountContentAuthority::ProductManifest {
            manifest_descriptor,
            ..
        } = &mut retained_inputs.inputs[1].content_authority
        {
            *manifest_descriptor = records[0].inherited_descriptor().unwrap();
        }
        if let GuestMountContentAuthority::SourceClosure {
            binding_descriptor,
            manifest_descriptor,
            ..
        } = &mut retained_inputs.inputs[2].content_authority
        {
            *binding_descriptor = records[1].inherited_descriptor().unwrap();
            *manifest_descriptor = records[2].inherited_descriptor().unwrap();
        }
        let retained = crate::guest_inputs::ExternalGuestInputAuthority::new(
            retained_inputs.clone(),
            transfer.descriptor().clone(),
            None,
            vec![config_authority, product_authority, source_authority],
            records,
            vec![],
        )
        .unwrap();
        let produced_expected = GuestStagingExpected {
            inputs: &retained_inputs,
            activation_request_digest: &manifest.activation_request_digest,
            bootstrap_sha256: &manifest.bootstrap_sha256,
            supervisor_sha256: &manifest.supervisor_sha256,
            launcher_sha256: &manifest.launcher_sha256,
            maximum_regular_bytes: 16 * 1024 * 1024,
            maximum_framed_bytes: 16 * 1024 * 1024,
        };
        let (produced_bytes, produced_manifest) =
            crate::guest_package_producer::write_guest_package(
                Vec::new(),
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &produced_expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .unwrap();
        assert_eq!(
            produced_manifest.guest_input_identity,
            manifest.guest_input_identity
        );
        for executable in ["supervisor", "launcher"] {
            assert!(produced_manifest.entries.iter().any(|entry| {
                matches!(entry, GuestStagingEntry::RegularFile { path, mode: 0o500, .. }
                    if path == executable)
            }));
        }
        let produced_stage =
            stage_guest_package(produced_bytes.as_slice(), &parent, &produced_expected).unwrap();
        for executable in ["supervisor", "launcher"] {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                produced_stage
                    .root()
                    .open_regular(OsStr::new(executable), false)
                    .unwrap()
                    .unwrap()
                    .metadata()
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o500
            );
        }
        crate::guest_content::recheck_staged_guest_content(&produced_stage, &retained_inputs)
            .unwrap();
        produced_stage.discard().unwrap();
        let prepare = || {
            crate::guest_package_producer::prepare_private_guest_package(
                &parent,
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &produced_expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
        };
        let prepared = prepare().unwrap();
        assert_eq!(prepared.manifest(), &produced_manifest);
        assert_eq!(
            prepared.manifest_sha256(),
            lillux::sha256_hex(
                lillux::canonical_json(&serde_json::to_value(&produced_manifest).unwrap())
                    .unwrap()
                    .as_bytes()
            )
        );
        assert_eq!(prepared.bytes(), produced_bytes.len() as u64);
        assert_eq!(prepared.sha256(), lillux::sha256_hex(&produced_bytes));
        let delivery = prepared.delivery_descriptor().unwrap();
        let mut delivery_reader = delivery
            .stable_regular_reader_exact(prepared.bytes(), prepared.sha256(), prepared.bytes())
            .unwrap();
        let mut delivered_bytes = Vec::new();
        use std::io::Read as _;
        delivery_reader.read_to_end(&mut delivered_bytes).unwrap();
        delivery_reader.finish().unwrap();
        assert_eq!(delivered_bytes, produced_bytes);
        drop(delivery);
        prepared.discard().unwrap();
        assert!(
            crate::guest_package_producer::prepare_private_guest_package(
                &parent,
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &produced_expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
            )
            .is_err()
        );
        let undersized = GuestStagingExpected {
            maximum_regular_bytes: 1,
            ..produced_expected
        };
        let size_error = crate::guest_package_producer::write_guest_package(
            Vec::new(),
            &retained,
            &inherited_file("bootstrap"),
            &supervisor_capture,
            &launcher_capture,
            &undersized,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
        )
        .err()
        .unwrap();
        assert!(format!("{size_error:#}").contains("budget"));
        let short_frame = GuestStagingExpected {
            maximum_framed_bytes: produced_manifest.framed_bytes().unwrap() - 1,
            ..produced_expected
        };
        assert!(
            crate::guest_package_producer::prepare_private_guest_package(
                &parent,
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &short_frame,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .is_err()
        );
        assert_eq!(parent.entries_no_follow_bounded(1).unwrap().len(), 1);
        struct StopWriter {
            remaining: usize,
        }
        impl Write for StopWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::other("injected package writer failure"));
                }
                let count = bytes.len().min(self.remaining);
                self.remaining -= count;
                Ok(count)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(
            crate::guest_package_producer::write_guest_package(
                StopWriter { remaining: 128 },
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &produced_expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .is_err()
        );
        std::fs::write(
            source_fixture.path().join("input-01/ambient-secret"),
            b"secret",
        )
        .unwrap();
        assert!(prepare().is_err());
        std::fs::remove_file(source_fixture.path().join("input-01/ambient-secret")).unwrap();
        assert_eq!(parent.entries_no_follow_bounded(1).unwrap().len(), 1);
        assert!(
            staged
                .root()
                .open_child_directory(OsStr::new("base"))
                .unwrap()
                .is_some()
        );
        let product = staged
            .root()
            .open_child_directory(OsStr::new("input-01"))
            .unwrap()
            .unwrap();
        assert_eq!(
            product
                .read_symlink_target(OsStr::new("current"), 4096)
                .unwrap()
                .unwrap(),
            b"bin/tool"
        );
        // Harness-only mutation: the observation cannot become a durable
        // Ready claim while a same-UID writer still owns the staged tree.
        std::fs::remove_file(product.path().join("current")).unwrap();
        std::os::unix::fs::symlink("bin/other", product.path().join("current")).unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        staged.discard().unwrap();
        assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
    }
}
