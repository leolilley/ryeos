use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

use ryeos_state::objects::{ProjectFile, ProjectSnapshotPolicy, ProjectTree};

mod workspace_outputs;
pub use workspace_outputs::{WorkspaceOutputObjectStage, capture_native_workspace_outputs};

const MAX_TRANSFER_ENTRIES: usize = 400_010;
const MAX_TRANSFER_DEPTH: usize = 4;

/// Content testimony for one exact, self-contained project-snapshot CAS
/// closure.  The digest is independent of filesystem layout and descriptor
/// coordinates; counts and bytes additionally bound transfer admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSnapshotTransferMeasurement {
    pub snapshot_hash: String,
    pub closure_digest: String,
    pub object_count: u64,
    pub blob_count: u64,
    pub total_bytes: u64,
}

impl ProjectSnapshotTransferMeasurement {
    pub fn validate_against(
        &self,
        snapshot_hash: &str,
        closure_digest: &str,
        object_count: u64,
        blob_count: u64,
        total_bytes: u64,
    ) -> Result<()> {
        anyhow::ensure!(
            self.snapshot_hash == snapshot_hash
                && self.closure_digest == closure_digest
                && self.object_count == object_count
                && self.blob_count == blob_count
                && self.total_bytes == total_bytes,
            "external base snapshot transfer contradicts its admitted testimony"
        );
        Ok(())
    }
}

/// Descriptor-held, bounded partial CAS containing exactly one complete
/// project-snapshot closure. This is the transferable B authority for an
/// external candidate; it never exposes the controller's complete CAS.
pub struct PreparedProjectSnapshotTransfer {
    parent: lillux::PinnedDirectory,
    name: OsString,
    root: lillux::PinnedDirectory,
    descriptor: lillux::InheritedDescriptorAuthority,
    snapshot_hash: String,
    closure_digest: String,
    object_count: u64,
    blob_count: u64,
    total_bytes: u64,
}

impl PreparedProjectSnapshotTransfer {
    pub fn descriptor(&self) -> lillux::InheritedDescriptorAuthority {
        self.descriptor.clone()
    }

    pub fn closure_digest(&self) -> &str {
        &self.closure_digest
    }

    pub fn object_count(&self) -> u64 {
        self.object_count
    }

    pub fn blob_count(&self) -> u64 {
        self.blob_count
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn measurement(&self) -> ProjectSnapshotTransferMeasurement {
        ProjectSnapshotTransferMeasurement {
            snapshot_hash: self.snapshot_hash.clone(),
            closure_digest: self.closure_digest.clone(),
            object_count: self.object_count,
            blob_count: self.blob_count,
            total_bytes: self.total_bytes,
        }
    }
}

impl Drop for PreparedProjectSnapshotTransfer {
    fn drop(&mut self) {
        let _ = self.root.remove_contents_recursive().and_then(|()| {
            self.parent
                .remove_empty_child_if_same(&self.name, &self.root)
                .and_then(|removed| {
                    if removed {
                        Ok(())
                    } else {
                        anyhow::bail!("project snapshot transfer identity changed during cleanup")
                    }
                })
        });
    }
}

/// Copy exactly one verified project snapshot closure into a private partial
/// CAS. Object and blob addresses, canonical bytes and aggregate bounds are
/// reverified while copying. The returned root is a CAS root (`objects/` and
/// `blobs/`), not a state/runtime root.
pub fn prepare_project_snapshot_transfer(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    snapshot_hash: &str,
) -> Result<PreparedProjectSnapshotTransfer> {
    authority.ensure_guard(guard)?;
    let source = authority.cas_store()?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &source,
        [snapshot_hash.to_owned()],
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
    )?;
    anyhow::ensure!(
        closure.is_complete() && closure.large_object_hashes.is_empty(),
        "external base snapshot closure is incomplete or contains non-CAS content"
    );
    let transfer_parent = authority
        .runtime_directory()
        .open_or_create_child(std::ffi::OsStr::new("external-project-transfers"), 0o700)?;
    transfer_parent.require_owner_private_directory()?;
    let (name, root) = transfer_parent.create_unique_child("snapshot", 0o700)?;
    root.require_owner_private_directory()?;
    let result = (|| -> Result<ProjectSnapshotTransferMeasurement> {
        let target = lillux::CasStore::from_pinned_root(root.try_clone()?);
        let mut entries =
            Vec::with_capacity(closure.object_hashes.len() + closure.blob_hashes.len());
        let mut total_bytes = 0_u64;
        for hash in &closure.object_hashes {
            let value = source
                .get_object(hash)?
                .ok_or_else(|| anyhow::anyhow!("base snapshot object {hash} disappeared"))?;
            let bytes = lillux::canonical_json(&value)?.len() as u64;
            total_bytes = total_bytes
                .checked_add(bytes)
                .ok_or_else(|| anyhow::anyhow!("base snapshot transfer byte overflow"))?;
            anyhow::ensure!(
                target.store_object(&value)? == *hash,
                "base snapshot object address changed during transfer"
            );
            entries.push(serde_json::json!({"kind":"object","hash":hash,"bytes":bytes}));
        }
        for hash in &closure.blob_hashes {
            let (file, bytes) = source
                .open_blob(hash)?
                .ok_or_else(|| anyhow::anyhow!("base snapshot blob {hash} disappeared"))?;
            total_bytes = total_bytes
                .checked_add(bytes)
                .ok_or_else(|| anyhow::anyhow!("base snapshot transfer byte overflow"))?;
            let copied = target.put_blob_from_open_regular_bounded(
                file,
                Path::new("<external-base-snapshot-blob>"),
                bytes,
            )?;
            anyhow::ensure!(
                copied.hash == *hash && copied.size == bytes,
                "base snapshot blob address changed during transfer"
            );
            entries.push(serde_json::json!({"kind":"blob","hash":hash,"bytes":bytes}));
        }
        let pruned = target.prune_abandoned_blob_captures(false)?;
        anyhow::ensure!(
            pruned.files == 0 && pruned.bytes == 0,
            "fresh base snapshot transfer contained interrupted blob capture state"
        );
        let expected = ProjectSnapshotTransferMeasurement {
            snapshot_hash: snapshot_hash.to_owned(),
            closure_digest: transfer_closure_digest(snapshot_hash, &entries)?,
            object_count: u64::try_from(closure.object_hashes.len())?,
            blob_count: u64::try_from(closure.blob_hashes.len())?,
            total_bytes,
        };
        let observed = inspect_project_snapshot_transfer(&root, snapshot_hash)?;
        anyhow::ensure!(
            observed == expected,
            "prepared base snapshot transfer changed during verification"
        );
        Ok(observed)
    })();
    let measurement = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = root.remove_contents_recursive();
            let _ = transfer_parent.remove_empty_child_if_same(&name, &root);
            return Err(error);
        }
    };
    let descriptor = root.inherited_descriptor_authority()?;
    Ok(PreparedProjectSnapshotTransfer {
        parent: transfer_parent,
        name,
        root,
        descriptor,
        snapshot_hash: snapshot_hash.to_owned(),
        closure_digest: measurement.closure_digest,
        object_count: measurement.object_count,
        blob_count: measurement.blob_count,
        total_bytes: measurement.total_bytes,
    })
}

/// Inspect a descriptor-pinned partial CAS and prove that it contains exactly
/// one complete project snapshot closure: no missing entries, no additional
/// files or directories, no links, and no independently addressed content.
pub fn inspect_project_snapshot_transfer(
    root: &lillux::PinnedDirectory,
    snapshot_hash: &str,
) -> Result<ProjectSnapshotTransferMeasurement> {
    root.require_owner_private_directory()?;
    let cas = lillux::CasStore::from_pinned_root(root.try_clone()?);
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [snapshot_hash.to_owned()],
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
    )?;
    anyhow::ensure!(
        closure.is_complete() && closure.large_object_hashes.is_empty(),
        "external base snapshot transfer is incomplete or contains non-CAS content"
    );

    let mut entries = Vec::with_capacity(closure.object_hashes.len() + closure.blob_hashes.len());
    let mut expected_files = std::collections::BTreeMap::<PathBuf, u64>::new();
    let mut total_bytes = 0_u64;
    for hash in &closure.object_hashes {
        let value = cas
            .get_object(hash)?
            .ok_or_else(|| anyhow::anyhow!("base snapshot object {hash} is missing"))?;
        let bytes = u64::try_from(lillux::canonical_json(&value)?.len())?;
        total_bytes = total_bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("base snapshot transfer byte overflow"))?;
        let relative = lillux::shard_path(Path::new(""), "objects", hash, ".json");
        anyhow::ensure!(
            expected_files.insert(relative, bytes).is_none(),
            "base snapshot object address is duplicated"
        );
        entries.push(serde_json::json!({"kind":"object","hash":hash,"bytes":bytes}));
    }
    for hash in &closure.blob_hashes {
        let (mut file, bytes) = cas
            .open_blob(hash)?
            .ok_or_else(|| anyhow::anyhow!("base snapshot blob {hash} is missing"))?;
        let (outcome, _) = lillux::digest_open_regular_file_stable_exact(&mut file, bytes)?;
        anyhow::ensure!(
            outcome == *hash,
            "base snapshot blob {hash} changed content address"
        );
        total_bytes = total_bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("base snapshot transfer byte overflow"))?;
        let relative = lillux::shard_path(Path::new(""), "blobs", hash, "");
        anyhow::ensure!(
            expected_files.insert(relative, bytes).is_none(),
            "base snapshot blob address is duplicated"
        );
        entries.push(serde_json::json!({"kind":"blob","hash":hash,"bytes":bytes}));
    }
    ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        &cas,
        snapshot_hash,
    )?;

    let expected_directories = expected_files
        .keys()
        .flat_map(|path| path.ancestors().skip(1))
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect::<std::collections::BTreeSet<_>>();
    let remaining = std::cell::RefCell::new(expected_files);
    root.visit_regular_files_bounded(
        lillux::DirectoryTraversalBudget::new(MAX_TRANSFER_ENTRIES, MAX_TRANSFER_DEPTH),
        |relative, is_directory| {
            if is_directory {
                anyhow::ensure!(
                    expected_directories.contains(relative),
                    "external base snapshot transfer contains an unexpected directory {}",
                    relative.display()
                );
            }
            Ok(false)
        },
        |relative, file| {
            let expected = remaining.borrow_mut().remove(relative).ok_or_else(|| {
                anyhow::anyhow!(
                    "external base snapshot transfer contains an unexpected file {}",
                    relative.display()
                )
            })?;
            anyhow::ensure!(
                lillux::observe_open_regular_file(&file)?.size() == expected,
                "external base snapshot transfer file {} changed size",
                relative.display()
            );
            Ok(())
        },
    )?;
    anyhow::ensure!(
        remaining.borrow().is_empty(),
        "external base snapshot transfer is missing an expected CAS entry"
    );

    Ok(ProjectSnapshotTransferMeasurement {
        snapshot_hash: snapshot_hash.to_owned(),
        closure_digest: transfer_closure_digest(snapshot_hash, &entries)?,
        object_count: u64::try_from(closure.object_hashes.len())?,
        blob_count: u64::try_from(closure.blob_hashes.len())?,
        total_bytes,
    })
}

/// Install a verified transfer into an empty private candidate runtime.  The
/// runtime layout is the one consumed by `PinnedStateAuthority`: its partial
/// CAS lives under `objects/` and its mutable candidate refs start empty.
pub fn install_project_snapshot_transfer(
    source: &lillux::PinnedDirectory,
    candidate_runtime: &lillux::PinnedDirectory,
    expected: &ProjectSnapshotTransferMeasurement,
) -> Result<()> {
    candidate_runtime.require_owner_private_directory()?;
    anyhow::ensure!(
        candidate_runtime.entries_no_follow_bounded(0)?.is_empty(),
        "external candidate runtime is not empty before base installation"
    );
    let observed = inspect_project_snapshot_transfer(source, &expected.snapshot_hash)?;
    anyhow::ensure!(
        &observed == expected,
        "external base snapshot transfer contradicts its admitted measurement"
    );
    let cas_root = candidate_runtime.create_child(std::ffi::OsStr::new("objects"), 0o700)?;
    let refs_root = match candidate_runtime.create_child(std::ffi::OsStr::new("refs"), 0o700) {
        Ok(root) => root,
        Err(error) => {
            let _ = candidate_runtime
                .remove_empty_child_if_same(std::ffi::OsStr::new("objects"), &cas_root);
            return Err(error);
        }
    };
    let result = (|| -> Result<()> {
        source.copy_contents_to_filtered(
            &cas_root,
            lillux::DirectoryTraversalBudget::new(MAX_TRANSFER_ENTRIES, MAX_TRANSFER_DEPTH),
            |_| Ok(false),
        )?;
        let installed = inspect_project_snapshot_transfer(&cas_root, &expected.snapshot_hash)?;
        anyhow::ensure!(
            &installed == expected,
            "installed external base snapshot changed identity"
        );
        anyhow::ensure!(
            refs_root.entries_no_follow_bounded(0)?.is_empty(),
            "external candidate refs root was not initialized empty"
        );
        // Candidate capture is a real durable CAS publication. Establish its
        // minimal recovery generation relative to this exact pinned runtime
        // before the runtime is handed to another process; the launcher must
        // only acquire already-established guard/staging authority, never
        // manufacture it during capture.
        ryeos_state::CasMutationGuard::initialize_fresh_recovery_in_pinned_runtime(
            candidate_runtime,
        )?;
        candidate_runtime.sync_tree_bounded(lillux::DirectoryTraversalBudget::new(
            MAX_TRANSFER_ENTRIES,
            MAX_TRANSFER_DEPTH,
        ))?;
        Ok(())
    })();
    if result.is_err() {
        if let Ok(Some(recovery)) =
            candidate_runtime.open_child_directory(std::ffi::OsStr::new("recovery"))
        {
            let _ = recovery.remove_contents_recursive_bounded(
                lillux::DirectoryTraversalBudget::new(MAX_TRANSFER_ENTRIES, MAX_TRANSFER_DEPTH),
            );
            let _ = candidate_runtime
                .remove_empty_child_if_same(std::ffi::OsStr::new("recovery"), &recovery);
        }
        let _ = cas_root.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
            MAX_TRANSFER_ENTRIES,
            MAX_TRANSFER_DEPTH,
        ));
        let _ = candidate_runtime
            .remove_empty_child_if_same(std::ffi::OsStr::new("objects"), &cas_root);
        let _ =
            candidate_runtime.remove_empty_child_if_same(std::ffi::OsStr::new("refs"), &refs_root);
    }
    result
}

fn transfer_closure_digest(snapshot_hash: &str, entries: &[serde_json::Value]) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&serde_json::json!({
            "domain":"ryeos.external-base-snapshot-transfer.v1",
            "snapshot_hash":snapshot_hash,
            "entries":entries,
        }))?
        .as_bytes(),
    ))
}

/// Capture one complete project tree with descriptor-relative traversal and
/// streaming blob ingestion. The policy is immutable input to this capture.
pub fn ingest_project_tree(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    project_root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
) -> Result<ProjectTree> {
    ingest_project_tree_with_operational_exclusions(authority, guard, project_root, policy, &[])
}

/// Capture a daemon-owned execution workspace while omitting operational
/// shadow roots that were populated from separately admitted realizations.
/// The exclusions are not author policy and never change source capture;
/// they prevent mounted/copy-bound inputs from becoming project outputs.
pub fn ingest_project_tree_with_operational_exclusions(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    project_root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
    operational_exclusions: &[String],
) -> Result<ProjectTree> {
    ingest_project_tree_inner(
        authority,
        guard,
        project_root,
        policy,
        operational_exclusions,
        None,
    )
}

/// Byte and monotonic-time bounds for a captured candidate. Checks include
/// directory traversal, streamed file ingestion and final tree construction.
/// Kernel I/O stalls still require the supervisor's independent lifetime bound.
#[derive(Clone, Copy)]
pub struct ProjectCaptureBudget {
    pub max_bytes: u64,
    pub deadline: lillux::time::MonotonicDeadline,
}

impl ProjectCaptureBudget {
    fn check(&self) -> Result<()> {
        anyhow::ensure!(
            !self.deadline.has_elapsed(),
            "project capture deadline expired"
        );
        Ok(())
    }
}

pub fn ingest_project_tree_bounded(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    project_root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
    budget: ProjectCaptureBudget,
) -> Result<ProjectTree> {
    ingest_project_tree_inner(authority, guard, project_root, policy, &[], Some(budget))
}

pub fn ingest_project_tree_bounded_with_exclusions(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    project_root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
    operational_exclusions: &[String],
    budget: ProjectCaptureBudget,
) -> Result<ProjectTree> {
    ingest_project_tree_inner(
        authority,
        guard,
        project_root,
        policy,
        operational_exclusions,
        Some(budget),
    )
}

fn ingest_project_tree_inner(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    project_root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
    operational_exclusions: &[String],
    budget: Option<ProjectCaptureBudget>,
) -> Result<ProjectTree> {
    if let Some(budget) = budget {
        budget.check()?;
    }
    authority.ensure_guard(guard)?;
    policy.validate()?;
    validate_operational_exclusions(operational_exclusions)?;
    let matcher = policy.matcher()?;
    let cas = authority.cas_store()?;
    let mut files = std::collections::BTreeMap::new();
    let mut descriptor_bytes = 0_u64;
    let mut content_bytes = 0_u64;
    project_root.visit_regular_files_bounded(
        lillux::DirectoryTraversalBudget::new(
            ryeos_state::project_sync::MAX_PROJECT_TREE_ENTRIES,
            ryeos_state::project_sync::MAX_PROJECT_TREE_DEPTH,
        ),
        |relative, is_directory| {
            if let Some(budget) = budget {
                budget.check()?;
            }
            let rel = canonical_relative_path(relative)?;
            if is_operationally_excluded(&rel, operational_exclusions)
                || ryeos_state::project_sync::is_project_snapshot_floor_excluded(&rel)
                || matcher.is_ignored(&rel)
            {
                return Ok(true);
            }
            if policy.sync_scope == ryeos_state::project_sync::ProjectSyncScope::AiOnly {
                return Ok(!matches!(
                    ryeos_state::project_sync::classify_project_ai_path(&rel, Some(&matcher)),
                    ryeos_state::project_sync::ProjectAiPathClass::Deployable(_)
                ));
            }
            let _ = is_directory;
            Ok(false)
        },
        |relative, file| {
            if let Some(budget) = budget {
                budget.check()?;
            }
            if files.len() >= ryeos_state::project_sync::MAX_PROJECT_TREE_FILES {
                anyhow::bail!(
                    "project capture exceeds {} regular files",
                    ryeos_state::project_sync::MAX_PROJECT_TREE_FILES
                );
            }
            let rel = canonical_relative_path(relative)?;
            ryeos_state::project_sync::validate_project_manifest_path(
                &rel,
                policy.sync_scope,
                Some(&matcher),
            )?;
            let streamed = if let Some(budget) = budget {
                cas.put_blob_from_open_regular_with_deadline(
                    file,
                    &project_root.path().join(relative),
                    budget
                        .max_bytes
                        .checked_sub(content_bytes)
                        .ok_or_else(|| anyhow::anyhow!("capture byte budget exhausted"))?,
                    budget.deadline,
                )?
            } else {
                cas.put_blob_from_open_regular(file, &project_root.path().join(relative))?
            };
            content_bytes = content_bytes
                .checked_add(streamed.size)
                .ok_or_else(|| anyhow::anyhow!("project capture content byte overflow"))?;
            let project_file = ProjectFile {
                blob_hash: streamed.hash,
                size: streamed.size,
                normalized_mode: streamed.normalized_mode,
            };
            project_file.validate()?;
            let object_bytes = lillux::canonical_json(&project_file.to_value())?.len() as u64;
            descriptor_bytes = descriptor_bytes
                .checked_add(object_bytes)
                .and_then(|total| total.checked_add(rel.len() as u64))
                .ok_or_else(|| anyhow::anyhow!("project capture descriptor byte count overflow"))?;
            if descriptor_bytes
                > ryeos_state::project_materialization::MAX_PROJECT_TREE_DESCRIPTOR_BYTES
            {
                anyhow::bail!(
                    "project capture exceeds {} descriptor bytes",
                    ryeos_state::project_materialization::MAX_PROJECT_TREE_DESCRIPTOR_BYTES
                );
            }
            let file_hash = cas.store_object(&project_file.to_value())?;
            if files.insert(rel.clone(), file_hash).is_some() {
                anyhow::bail!("duplicate canonical project path during capture: {rel}");
            }
            Ok(())
        },
    )?;
    let tree = ProjectTree { files };
    ryeos_state::project_sync::validate_project_tree_paths(&tree, policy)?;
    if let Some(budget) = budget {
        budget.check()?;
    }
    Ok(tree)
}

pub fn validate_operational_exclusions(exclusions: &[String]) -> Result<()> {
    let mut previous: Option<&str> = None;
    for exclusion in exclusions {
        ryeos_state::project_sync::validate_safe_relative_path(exclusion)?;
        if previous.is_some_and(|value| value >= exclusion.as_str()) {
            anyhow::bail!("operational project exclusions are not uniquely path-sorted");
        }
        previous = Some(exclusion);
    }
    Ok(())
}

pub fn is_operationally_excluded(path: &str, exclusions: &[String]) -> bool {
    exclusions.iter().any(|root| {
        path == root
            || path
                .strip_prefix(root)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

pub fn restore_operational_shadow_files(
    captured: &mut ProjectTree,
    base: &ProjectTree,
    exclusions: &[String],
) -> Result<()> {
    captured
        .files
        .retain(|path, _| !is_operationally_excluded(path, exclusions));
    captured.files.extend(
        base.files
            .iter()
            .filter(|(path, _)| is_operationally_excluded(path, exclusions))
            .map(|(path, hash)| (path.clone(), hash.clone())),
    );
    // The process can replace an input's ancestor with a regular file. The
    // restored base and captured edits must still form one valid tree before
    // any candidate snapshot can commit it.
    captured.validate()
}

fn canonical_relative_path(relative: &Path) -> Result<String> {
    let value = relative
        .to_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project-relative path '{}' is not valid UTF-8",
                relative.display()
            )
        })?
        .replace('\\', "/");
    ryeos_state::project_sync::validate_safe_relative_path(&value)?;
    Ok(value)
}

pub fn materialize_project_file(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    object_hash: &str,
    target_path: &Path,
) -> Result<()> {
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    let file = ryeos_state::project_materialization::load_project_file_bounded(&cas, object_hash)?
        .ok_or_else(|| anyhow::anyhow!("project_file object {object_hash} not found"))?;
    let size =
        cas.materialize_blob_to_new_file(&file.blob_hash, target_path, file.normalized_mode)?;
    if size != file.size {
        let _ = lillux::remove_file_durable(target_path);
        anyhow::bail!(
            "project_file {} declared size {}, materialized {}",
            object_hash,
            file.size,
            size
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::sync::Arc;

    use super::{
        ingest_project_tree, is_operationally_excluded, materialize_project_file,
        restore_operational_shadow_files,
    };
    use ryeos_app::node_policy::NodePolicySection as _;

    #[test]
    fn live_descriptor_capture_cannot_be_redirected_by_rebinding_diagnostic_path() {
        let fixture = tempfile::tempdir().unwrap();
        let original = fixture.path().join("candidate");
        let displaced = fixture.path().join("displaced");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("policy.py"), b"exact candidate").unwrap();
        let root = lillux::PinnedDirectory::open(&original).unwrap().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let db =
            ryeos_state::StateDb::open(state_root.path(), Arc::new(ryeos_state::TrustStore::new()))
                .unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let policy = ryeos_state::objects::ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            Default::default(),
        )
        .unwrap();
        let before = ingest_project_tree(&authority, &guard, &root, &policy).unwrap();
        std::fs::rename(&original, &displaced).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("policy.py"), b"unrelated replacement").unwrap();
        assert!(root.ensure_path_binding().is_err());
        let after = super::ingest_project_tree_bounded(
            &authority,
            &guard,
            &root,
            &policy,
            super::ProjectCaptureBudget {
                max_bytes: 1024,
                deadline: lillux::time::MonotonicDeadline::after(
                    lillux::time::Duration::from_secs(10),
                ),
            },
        )
        .unwrap();
        assert_eq!(before.files, after.files);
        let replaced = lillux::PinnedDirectory::open(&original).unwrap().unwrap();
        assert_ne!(
            after.files,
            ingest_project_tree(&authority, &guard, &replaced, &policy)
                .unwrap()
                .files,
        );
    }

    #[test]
    fn private_input_shadows_preserve_base_files_and_do_not_publish_evidence() {
        use ryeos_state::objects::ProjectTree;
        let base = ProjectTree {
            files: [
                ("evidence/existing.json".to_owned(), "a".repeat(64)),
                ("src/solver.py".to_owned(), "b".repeat(64)),
            ]
            .into(),
        };
        let mut captured = ProjectTree {
            files: [
                ("evidence/existing.json".to_owned(), "c".repeat(64)),
                ("evidence/new.json".to_owned(), "d".repeat(64)),
                ("src/solver.py".to_owned(), "e".repeat(64)),
                ("evidence-adjacent.json".to_owned(), "f".repeat(64)),
            ]
            .into(),
        };
        let exclusions = vec![
            "evidence/existing.json".to_owned(),
            "evidence/new.json".to_owned(),
        ];
        restore_operational_shadow_files(&mut captured, &base, &exclusions).unwrap();
        assert_eq!(
            captured.files.get("evidence/existing.json"),
            base.files.get("evidence/existing.json")
        );
        assert!(!captured.files.contains_key("evidence/new.json"));
        assert_eq!(captured.files["src/solver.py"], "e".repeat(64));
        assert_eq!(captured.files["evidence-adjacent.json"], "f".repeat(64));
    }

    #[test]
    fn private_input_shadow_rejects_a_captured_regular_file_ancestor() {
        use ryeos_state::objects::ProjectTree;
        let base = ProjectTree {
            files: [("evidence/input.json".to_owned(), "a".repeat(64))].into(),
        };
        let mut captured = ProjectTree {
            files: [("evidence".to_owned(), "b".repeat(64))].into(),
        };
        let error = restore_operational_shadow_files(
            &mut captured,
            &base,
            &["evidence/input.json".to_owned()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("nested below regular file"));
    }

    #[test]
    fn operational_shadow_roots_match_only_segment_bounded_descendants() {
        let exclusions = vec!["vendor/runtime".to_string()];
        assert!(is_operationally_excluded("vendor/runtime", &exclusions));
        assert!(is_operationally_excluded(
            "vendor/runtime/lib/module.py",
            &exclusions
        ));
        assert!(!is_operationally_excluded(
            "vendor/runtime-extra/module.py",
            &exclusions
        ));
        assert!(!is_operationally_excluded("vendor", &exclusions));
    }

    #[test]
    fn signed_standard_policy_drives_capture_closure_and_materialization() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/.ai/node/init/profiles/standard.yaml"
        ));
        let body = lillux::signature::strip_signature_lines(raw);
        let profile: ryeos_app::node_policy::generation::NodeInitProfile =
            serde_yaml::from_str(&body).unwrap();
        profile
            .validate(
                &ryeos_app::node_policy::NodePolicyTable::new(),
                Path::new("standard.yaml"),
            )
            .unwrap();
        let parsed = ryeos_app::node_policy::sections::ingest_ignore::IngestIgnorePolicySection
            .parse(
                &ryeos_app::node_policy::NodePolicyContext {
                    section: "ingest_ignore".to_owned(),
                    source_file: "standard.yaml".into(),
                    signer_fingerprint: "ab".repeat(32),
                },
                profile.policies().get("ingest_ignore").unwrap(),
            )
            .unwrap();
        let policy_record = parsed
            .as_any()
            .downcast_ref::<
                ryeos_app::node_policy::sections::ingest_ignore::CompiledIngestIgnorePolicy,
            >()
            .unwrap();

        let project = tempfile::tempdir().unwrap();
        for (relative, bytes) in [
            ("src/lib.rs", b"pub fn retained() {}\n".as_slice()),
            (
                ".dev-keys/PUBLISHER_DEV.pem",
                b"public development fixture\n".as_slice(),
            ),
            (".git/config", b"git metadata\n".as_slice()),
            (".env", b"LOCAL_ONLY=value\n".as_slice()),
            ("target/debug/output", b"build output\n".as_slice()),
            (".ai/.bundles.lock", b"".as_slice()),
        ] {
            let path = project.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }

        let state_root = tempfile::tempdir().unwrap();
        let state_db =
            ryeos_state::StateDb::open(state_root.path(), Arc::new(ryeos_state::TrustStore::new()))
                .unwrap();
        let authority = state_db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let project_root = lillux::PinnedDirectory::open(project.path())
            .unwrap()
            .unwrap();
        let snapshot_policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
            &project_root,
            &policy_record.matcher,
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
        )
        .unwrap();
        let tree =
            ingest_project_tree(&authority, &guard, &project_root, &snapshot_policy).unwrap();

        let budget = super::ProjectCaptureBudget {
            max_bytes: 1024,
            deadline: lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10)),
        };
        assert_eq!(
            super::ingest_project_tree_bounded(
                &authority,
                &guard,
                &project_root,
                &snapshot_policy,
                budget
            )
            .unwrap()
            .files,
            tree.files
        );
        assert!(
            super::ingest_project_tree_bounded(
                &authority,
                &guard,
                &project_root,
                &snapshot_policy,
                super::ProjectCaptureBudget {
                    max_bytes: 1,
                    ..budget
                }
            )
            .is_err()
        );
        assert!(
            super::ingest_project_tree_bounded(
                &authority,
                &guard,
                &project_root,
                &snapshot_policy,
                super::ProjectCaptureBudget {
                    deadline: lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO,),
                    ..budget
                }
            )
            .is_err()
        );

        assert_eq!(
            tree.files.keys().cloned().collect::<Vec<_>>(),
            vec![
                ".dev-keys/PUBLISHER_DEV.pem".to_owned(),
                "src/lib.rs".to_owned()
            ]
        );

        let cas = authority.cas_store().unwrap();
        let policy_hash = cas.store_object(&snapshot_policy.to_value()).unwrap();
        let tree_hash = cas.store_object(&tree.to_value()).unwrap();
        let snapshot = ryeos_state::objects::ProjectSnapshot {
            project_tree_hash: tree_hash,
            effective_policy_hash: policy_hash,
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-09-04T00:00:00Z".to_owned(),
            source: "signed-standard-policy-test".to_owned(),
        };
        let snapshot_hash = cas.store_object(&snapshot.to_value()).unwrap();
        let closure = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
            &cas,
            &snapshot_hash,
        )
        .unwrap();
        assert_eq!(closure.tree().tree().files, tree.files);
        let object_closure =
            ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                &cas,
                [snapshot_hash.clone()],
                ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
            )
            .unwrap();
        assert!(object_closure.is_complete());

        let transfer =
            super::prepare_project_snapshot_transfer(&authority, &guard, &snapshot_hash).unwrap();
        let transfer_root = transfer
            .descriptor()
            .try_clone_pinned_directory(std::path::PathBuf::from("<project-transfer-test>"))
            .unwrap();
        assert_eq!(
            super::inspect_project_snapshot_transfer(&transfer_root, &snapshot_hash).unwrap(),
            transfer.measurement()
        );
        drop(guard);
        let candidate_runtime_dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(
            candidate_runtime_dir.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let candidate_runtime = lillux::PinnedDirectory::open(candidate_runtime_dir.path())
            .unwrap()
            .unwrap();
        super::install_project_snapshot_transfer(
            &transfer_root,
            &candidate_runtime,
            &transfer.measurement(),
        )
        .unwrap();
        let candidate_authority =
            ryeos_state::PinnedStateAuthority::from_external_candidate_runtime(
                candidate_runtime.try_clone().unwrap(),
            )
            .unwrap();
        let candidate_guard = candidate_authority.acquire_shared_guard().unwrap();
        candidate_authority.ensure_guard(&candidate_guard).unwrap();
        candidate_authority.require_recovery().unwrap();
        ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
            &candidate_authority.cas_store().unwrap(),
            &snapshot_hash,
        )
        .unwrap();
        drop(candidate_guard);
        let mut extra = transfer_root
            .open_regular_create(std::ffi::OsStr::new("ambient"), true, true, 0o600)
            .unwrap();
        use std::io::Write as _;
        extra.write_all(b"ambient").unwrap();
        extra.sync_all().unwrap();
        assert!(
            super::inspect_project_snapshot_transfer(&transfer_root, &snapshot_hash)
                .unwrap_err()
                .to_string()
                .contains("unexpected file")
        );

        let materialized = tempfile::tempdir().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        for (relative, object_hash) in &tree.files {
            let target = materialized.path().join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            materialize_project_file(&authority, &guard, object_hash, &target).unwrap();
        }
        let admitted = ryeos_state::project_materialization::PinnedProjectMaterialization::verify(
            &authority,
            &guard,
            &snapshot_hash,
            materialized.path(),
        )
        .unwrap();
        admitted.ensure_path_binding().unwrap();
        assert_eq!(
            std::fs::read(materialized.path().join(".dev-keys/PUBLISHER_DEV.pem")).unwrap(),
            b"public development fixture\n"
        );
        for excluded in [".git", ".env", "target", ".ai/.bundles.lock"] {
            assert!(!materialized.path().join(excluded).exists(), "{excluded}");
        }
    }
}
