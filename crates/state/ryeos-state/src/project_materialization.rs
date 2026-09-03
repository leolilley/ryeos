//! Opaque proof that one descriptor-pinned directory is the exact filesystem
//! realization of a CAS project snapshot.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;

use crate::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
use crate::{CasMutationGuard, PinnedStateAuthority};

pub const MAX_PROJECT_SNAPSHOT_OBJECT_BYTES: u64 = 256 * 1024;
pub const MAX_PROJECT_POLICY_OBJECT_BYTES: u64 = 1024 * 1024;
pub const MAX_PROJECT_TREE_OBJECT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PROJECT_FILE_OBJECT_BYTES: u64 = 4 * 1024;
pub const MAX_PROJECT_TREE_DESCRIPTOR_BYTES: u64 = 64 * 1024 * 1024;

/// Bounded, semantically validated descriptor closure for one project tree
/// and its immutable capture policy.
#[derive(Debug, Clone)]
pub struct VerifiedProjectTreeClosure {
    tree: ProjectTree,
    policy: ProjectSnapshotPolicy,
    files: Arc<BTreeMap<String, ProjectFile>>,
}

impl VerifiedProjectTreeClosure {
    pub fn load(
        cas: &lillux::CasStore,
        tree_hash: &str,
        policy_hash: &str,
    ) -> anyhow::Result<Self> {
        let (tree, tree_object_bytes) = load_project_tree_with_size_bounded(cas, tree_hash)?
            .ok_or_else(|| anyhow::anyhow!("project tree {tree_hash} not found"))?;
        let policy = load_project_policy_bounded(cas, policy_hash)?
            .ok_or_else(|| anyhow::anyhow!("project snapshot policy {policy_hash} not found"))?;
        crate::project_sync::validate_project_tree_paths(&tree, &policy)?;

        let mut files = BTreeMap::new();
        let mut descriptor_bytes = tree_object_bytes;
        for (relative, object_hash) in &tree.files {
            let (file, object_bytes) = load_project_file_with_size_bounded(cas, object_hash)?
                .ok_or_else(|| anyhow::anyhow!("project file {object_hash} not found"))?;
            descriptor_bytes = descriptor_bytes
                .checked_add(object_bytes)
                .and_then(|total| total.checked_add(relative.len() as u64))
                .ok_or_else(|| anyhow::anyhow!("project tree descriptor byte count overflow"))?;
            if descriptor_bytes > MAX_PROJECT_TREE_DESCRIPTOR_BYTES {
                anyhow::bail!(
                    "project tree exceeds {MAX_PROJECT_TREE_DESCRIPTOR_BYTES} descriptor bytes"
                );
            }
            let (_, blob_size) = cas.open_blob(&file.blob_hash)?.ok_or_else(|| {
                anyhow::anyhow!("project blob {} for {} not found", file.blob_hash, relative)
            })?;
            if blob_size != file.size {
                anyhow::bail!(
                    "project blob {} for {} has size {}, expected {}",
                    file.blob_hash,
                    relative,
                    blob_size,
                    file.size
                );
            }
            files.insert(relative.clone(), file);
        }
        crate::project_sync::validate_captured_policy_source_from_files(&tree, &policy, &files)?;
        Ok(Self {
            tree,
            policy,
            files: Arc::new(files),
        })
    }

    pub fn tree(&self) -> &ProjectTree {
        &self.tree
    }

    pub fn policy(&self) -> &ProjectSnapshotPolicy {
        &self.policy
    }

    pub fn files(&self) -> &BTreeMap<String, ProjectFile> {
        &self.files
    }
}

/// Bounded descriptor closure rooted at one project snapshot object.
#[derive(Debug, Clone)]
pub struct VerifiedProjectSnapshotClosure {
    snapshot_hash: String,
    snapshot: ProjectSnapshot,
    tree: VerifiedProjectTreeClosure,
}

impl VerifiedProjectSnapshotClosure {
    pub fn load(cas: &lillux::CasStore, snapshot_hash: &str) -> anyhow::Result<Self> {
        let snapshot = load_project_snapshot_bounded(cas, snapshot_hash)?
            .ok_or_else(|| anyhow::anyhow!("project snapshot {snapshot_hash} not found"))?;
        let tree = VerifiedProjectTreeClosure::load(
            cas,
            &snapshot.project_tree_hash,
            &snapshot.effective_policy_hash,
        )?;
        Ok(Self {
            snapshot_hash: snapshot_hash.to_owned(),
            snapshot,
            tree,
        })
    }

    pub fn snapshot_hash(&self) -> &str {
        &self.snapshot_hash
    }

    pub fn snapshot(&self) -> &ProjectSnapshot {
        &self.snapshot
    }

    pub fn tree(&self) -> &VerifiedProjectTreeClosure {
        &self.tree
    }
}

/// Descriptor-rooted, content-verified project materialization authority.
///
/// Fields are private and the only production constructor hashes the complete
/// pinned tree against the snapshot's CAS closure. Consumers therefore cannot
/// turn an arbitrary temporary pathname plus a claimed snapshot hash into
/// immutable execution authority.
#[derive(Clone)]
pub struct PinnedProjectMaterialization {
    snapshot_hash: String,
    root: Arc<lillux::PinnedDirectory>,
    expected_tree: Arc<BTreeMap<String, ProjectFile>>,
    cas: Arc<lillux::CasStore>,
}

impl PinnedProjectMaterialization {
    /// Construct a content-checked materialization without a durable CAS
    /// authority for cross-crate contract tests.
    ///
    /// This is intentionally absent from production builds: only
    /// [`Self::verify`] and [`Self::verify_from_closure`] may mint production
    /// materialization authority.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn from_observed_tree_for_test(
        snapshot_hash: String,
        path: &Path,
        expected_tree: BTreeMap<String, ProjectFile>,
    ) -> anyhow::Result<Self> {
        if !lillux::valid_hash(&snapshot_hash) {
            anyhow::bail!("test materialization snapshot hash must be 64 hexadecimal characters");
        }
        for (relative, file) in &expected_tree {
            crate::project_sync::validate_safe_relative_path(relative)?;
            file.validate()?;
        }
        let root = lillux::PinnedDirectory::open(path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "test materialized project root is missing: {}",
                path.display()
            )
        })?;
        let expected_tree = Arc::new(expected_tree);
        let observed = observe_materialized_tree(&root, &expected_tree)?;
        if observed.as_ref() != expected_tree.as_ref() {
            anyhow::bail!("test materialized project tree differs from its expected content");
        }
        root.ensure_path_binding()?;
        Ok(Self {
            snapshot_hash,
            root: Arc::new(root),
            expected_tree,
            // Contract tests exercise descriptor validation and relocation,
            // not authoritative blob reads. Keep the unused CAS namespace
            // outside the materialized tree so it cannot affect observation.
            cas: Arc::new(lillux::CasStore::new(
                path.with_extension(format!("ryeos-test-cas-{}", rand::random::<u64>())),
            )),
        })
    }

    pub fn verify(
        authority: &PinnedStateAuthority,
        guard: &CasMutationGuard,
        snapshot_hash: &str,
        path: &Path,
    ) -> anyhow::Result<Self> {
        authority.ensure_guard(guard)?;
        let cas = authority.cas_store()?;
        let closure = VerifiedProjectSnapshotClosure::load(&cas, snapshot_hash)?;
        Self::verify_from_closure(authority, guard, &closure, path)
    }

    pub fn verify_from_closure(
        authority: &PinnedStateAuthority,
        guard: &CasMutationGuard,
        closure: &VerifiedProjectSnapshotClosure,
        path: &Path,
    ) -> anyhow::Result<Self> {
        authority.ensure_guard(guard)?;
        let cas = Arc::new(authority.cas_store()?);
        let snapshot_hash = closure.snapshot_hash();
        let expected = Arc::clone(&closure.tree.files);

        let root = lillux::PinnedDirectory::open(path)?.ok_or_else(|| {
            anyhow::anyhow!("materialized project root is missing: {}", path.display())
        })?;
        let observed = observe_materialized_tree(&root, &expected)?;
        if observed.as_ref() != expected.as_ref() {
            anyhow::bail!(
                "materialized project tree differs from authoritative snapshot {snapshot_hash}"
            );
        }
        root.ensure_path_binding()?;
        Ok(Self {
            snapshot_hash: snapshot_hash.to_owned(),
            root: Arc::new(root),
            expected_tree: expected,
            cas,
        })
    }

    /// Reconstitute the immutable project-resolution authority for a retained
    /// writable workspace after a proved-dead execution owner.
    ///
    /// This constructor deliberately does not compare the current tree with
    /// the base snapshot: a retained CoW lower is expected to contain the
    /// session's unpublished changes. Its caller must first verify the durable
    /// execution-workspace journal, including the original snapshot, backend,
    /// mount identity, and all pinned root identities. The exact lower
    /// device/inode recorded by that journal is repeated here so a replaced
    /// pathname cannot be promoted into project authority.
    ///
    /// The returned value continues to answer authoritative project metadata
    /// and blob reads from the admitted CAS closure. Only the descriptor-rooted
    /// execution view is retained as mutable workspace state.
    pub fn recover_retained_workspace_from_closure(
        authority: &PinnedStateAuthority,
        guard: &CasMutationGuard,
        closure: &VerifiedProjectSnapshotClosure,
        path: &Path,
        expected_root_identity: &str,
    ) -> anyhow::Result<Self> {
        authority.ensure_guard(guard)?;
        let cas = Arc::new(authority.cas_store()?);
        let root = lillux::PinnedDirectory::open(path)?.ok_or_else(|| {
            anyhow::anyhow!("retained project workspace is missing: {}", path.display())
        })?;
        let (device, inode) = root.device_inode()?;
        let observed_root_identity = format!("dev{device}-ino{inode}");
        if observed_root_identity != expected_root_identity {
            anyhow::bail!(
                "retained project workspace root identity changed: expected {expected_root_identity}, observed {observed_root_identity}"
            );
        }
        root.ensure_path_binding()?;
        Ok(Self {
            snapshot_hash: closure.snapshot_hash().to_owned(),
            root: Arc::new(root),
            expected_tree: Arc::clone(&closure.tree.files),
            cas,
        })
    }

    pub fn snapshot_hash(&self) -> &str {
        &self.snapshot_hash
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    pub fn ensure_path_binding(&self) -> anyhow::Result<()> {
        self.root.ensure_path_binding()?;
        let observed = observe_materialized_tree(&self.root, &self.expected_tree)?;
        if observed.as_ref() != self.expected_tree.as_ref() {
            anyhow::bail!(
                "materialized project tree changed after authority was admitted for snapshot {}",
                self.snapshot_hash
            );
        }
        Ok(())
    }

    /// Revalidate only the retained root inode/path binding. This is not a
    /// content proof; callers may use it after an initial full verification
    /// only when every consumed positive/absence is separately checked
    /// against the authoritative CAS tree.
    pub fn ensure_root_binding(&self) -> anyhow::Result<()> {
        self.root.ensure_path_binding()
    }

    pub fn owns_path(&self, path: &Path) -> anyhow::Result<bool> {
        if self.path() != path {
            return Ok(false);
        }
        let current = lillux::PinnedDirectory::open(path)?
            .ok_or_else(|| anyhow::anyhow!("materialized project path disappeared"))?;
        self.root.is_same_directory(&current)
    }

    /// Enumerate authoritative metadata beneath one project-relative prefix
    /// without opening any blobs or consulting the checkout.
    pub fn authoritative_entries_under(
        &self,
        relative_prefix: &str,
        recursive: bool,
        max_entries: usize,
    ) -> anyhow::Result<Vec<(String, ProjectFile)>> {
        let prefix = relative_prefix.trim_end_matches('/');
        crate::project_sync::validate_safe_relative_path(prefix)?;
        let prefix = format!("{prefix}/");
        let mut files = Vec::new();
        for (relative, project_file) in self.expected_tree.iter() {
            let Some(suffix) = relative.strip_prefix(&prefix) else {
                continue;
            };
            if suffix.is_empty() {
                continue;
            }
            if !recursive && suffix.contains('/') {
                continue;
            }
            if files.len() >= max_entries {
                anyhow::bail!("authoritative project prefix {prefix} exceeds {max_entries} files");
            }
            files.push((suffix.to_owned(), project_file.clone()));
        }
        Ok(files)
    }

    /// Read one exact project-relative file from the authoritative CAS
    /// closure. The checkout path is never opened, and descriptor size is
    /// rejected before allocating or reading the body.
    pub fn authoritative_file_bounded(
        &self,
        relative: &str,
        max_bytes: u64,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        crate::project_sync::validate_safe_relative_path(relative)?;
        let Some(project_file) = self.expected_tree.get(relative) else {
            return Ok(None);
        };
        if project_file.size > max_bytes {
            anyhow::bail!("authoritative project file {relative} exceeds {max_bytes} bytes");
        }
        let Some((file, observed_size)) = self.cas.open_blob(&project_file.blob_hash)? else {
            anyhow::bail!(
                "authoritative project blob {} for {} is missing",
                project_file.blob_hash,
                relative
            );
        };
        if observed_size != project_file.size || observed_size > max_bytes {
            anyhow::bail!(
                "authoritative project blob {} for {} has unexpected size",
                project_file.blob_hash,
                relative
            );
        }
        let bytes =
            lillux::read_open_regular_file_exact_bounded(file, project_file.size, max_bytes)?;
        if lillux::sha256_hex(&bytes) != project_file.blob_hash {
            anyhow::bail!(
                "authoritative project blob {} for {} failed content verification",
                project_file.blob_hash,
                relative
            );
        }
        Ok(Some(bytes))
    }

    /// Validate one whole-file digest observed by a resolver against the
    /// authoritative snapshot tree. `path` must be rooted in this exact
    /// materialization.
    pub fn validates_observed_file(&self, path: &Path, digest: &str) -> anyhow::Result<bool> {
        let relative = path.strip_prefix(self.path()).map_err(|_| {
            anyhow::anyhow!(
                "observed project file {} is outside admitted root {}",
                path.display(),
                self.path().display()
            )
        })?;
        let relative = relative.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "observed project file path is not UTF-8: {}",
                path.display()
            )
        })?;
        crate::project_sync::validate_safe_relative_path(relative)?;
        Ok(self
            .expected_tree
            .get(relative)
            .is_some_and(|expected| expected.blob_hash == digest))
    }

    /// Validate a resolver's exact negative project-file probe against the
    /// authoritative snapshot tree.
    pub fn validates_observed_absence(&self, path: &Path) -> anyhow::Result<bool> {
        let relative = path.strip_prefix(self.path()).map_err(|_| {
            anyhow::anyhow!(
                "observed project absence {} is outside admitted root {}",
                path.display(),
                self.path().display()
            )
        })?;
        let relative = relative.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "observed project absence path is not UTF-8: {}",
                path.display()
            )
        })?;
        crate::project_sync::validate_safe_relative_path(relative)?;
        Ok(!self.expected_tree.contains_key(relative))
    }
}

/// Re-prove the complete descriptor-rooted tree. Every regular file is hashed;
/// filesystem metadata is never accepted as content identity. Directory
/// traversal also observes additions and removals, so retaining the root inode
/// alone can never stand in for pinned child content.
fn observe_materialized_tree(
    root: &lillux::PinnedDirectory,
    expected: &BTreeMap<String, ProjectFile>,
) -> anyhow::Result<Arc<BTreeMap<String, ProjectFile>>> {
    let mut observed = BTreeMap::new();
    let mut descriptor_bytes = 0_u64;
    root.visit_regular_files_bounded(
        lillux::DirectoryTraversalBudget::new(
            crate::project_sync::MAX_PROJECT_TREE_ENTRIES,
            crate::project_sync::MAX_PROJECT_TREE_DEPTH,
        ),
        |_relative, _directory| Ok(false),
        |relative, mut file| {
            if observed.len() >= crate::project_sync::MAX_PROJECT_TREE_FILES {
                anyhow::bail!(
                    "materialized project exceeds {} regular files",
                    crate::project_sync::MAX_PROJECT_TREE_FILES
                );
            }
            let relative = relative
                .to_str()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "materialized project contains a non-UTF-8 path: {}",
                        relative.display()
                    )
                })?
                .to_owned();
            crate::project_sync::validate_safe_relative_path(&relative)?;
            let expected_file = expected.get(&relative).ok_or_else(|| {
                anyhow::anyhow!(
                    "materialized project contains unexpected regular file {relative}"
                )
            })?;
            let before = file.metadata()?;
            if before.len() != expected_file.size {
                anyhow::bail!(
                    "materialized project file {relative} has size {}, expected {}",
                    before.len(),
                    expected_file.size
                );
            }
            let before_mode = lillux::normalized_portable_regular_mode(&before)?;
            if before_mode != expected_file.normalized_mode {
                anyhow::bail!(
                    "materialized project file {relative} has mode {before_mode:#o}, expected {:#o}",
                    expected_file.normalized_mode
                );
            }
            let (blob_hash, metadata) = lillux::digest_open_regular_file_stable_exact(
                &mut file,
                expected_file.size,
            )?;
            let normalized_mode = lillux::normalized_portable_regular_mode(&metadata)?;
            let observed_file = ProjectFile {
                blob_hash,
                size: metadata.len(),
                normalized_mode,
            };
            observed_file
                .validate()
                .with_context(|| format!("validate materialized project file {relative}"))?;
            if &observed_file != expected_file {
                anyhow::bail!(
                    "materialized project file {relative} differs from its admitted descriptor"
                );
            }
            let object_bytes = lillux::canonical_json(&observed_file.to_value())?.len() as u64;
            descriptor_bytes = descriptor_bytes
                .checked_add(object_bytes)
                .and_then(|total| total.checked_add(relative.len() as u64))
                .ok_or_else(|| {
                    anyhow::anyhow!("materialized project descriptor byte count overflow")
                })?;
            if descriptor_bytes > MAX_PROJECT_TREE_DESCRIPTOR_BYTES {
                anyhow::bail!(
                    "materialized project exceeds {MAX_PROJECT_TREE_DESCRIPTOR_BYTES} descriptor bytes"
                );
            }
            if observed.insert(relative.clone(), observed_file).is_some() {
                anyhow::bail!("materialized project contains duplicate path {relative}");
            }
            Ok(())
        },
    )?;
    Ok(Arc::new(observed))
}

fn read_bounded_cas_object(
    cas: &lillux::CasStore,
    hash: &str,
    max_bytes: u64,
) -> anyhow::Result<Option<(serde_json::Value, u64)>> {
    let Some((file, size)) = cas.open_object(hash)? else {
        return Ok(None);
    };
    if size > max_bytes {
        anyhow::bail!("CAS object {hash} exceeds {max_bytes} bytes");
    }
    let bytes = lillux::read_open_regular_file_bounded(file, max_bytes)?;
    if bytes.len() as u64 != size || lillux::sha256_hex(&bytes) != hash {
        anyhow::bail!("CAS object {hash} failed content-address verification");
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("decode CAS object {hash}"))?;
    let canonical = lillux::canonical_json(&value)?;
    if canonical.as_bytes() != bytes {
        anyhow::bail!("CAS object {hash} violates the canonical JSON contract");
    }
    Ok(Some((value, size)))
}

pub fn load_project_snapshot_bounded(
    cas: &lillux::CasStore,
    hash: &str,
) -> anyhow::Result<Option<ProjectSnapshot>> {
    read_bounded_cas_object(cas, hash, MAX_PROJECT_SNAPSHOT_OBJECT_BYTES)?
        .map(|(value, _)| ProjectSnapshot::from_value(&value))
        .transpose()
}

pub fn load_project_tree_bounded(
    cas: &lillux::CasStore,
    hash: &str,
) -> anyhow::Result<Option<ProjectTree>> {
    load_project_tree_with_size_bounded(cas, hash).map(|loaded| loaded.map(|(tree, _)| tree))
}

fn load_project_tree_with_size_bounded(
    cas: &lillux::CasStore,
    hash: &str,
) -> anyhow::Result<Option<(ProjectTree, u64)>> {
    read_bounded_cas_object(cas, hash, MAX_PROJECT_TREE_OBJECT_BYTES)?
        .map(|(value, bytes)| ProjectTree::from_value(&value).map(|tree| (tree, bytes)))
        .transpose()
}

pub fn load_project_policy_bounded(
    cas: &lillux::CasStore,
    hash: &str,
) -> anyhow::Result<Option<ProjectSnapshotPolicy>> {
    read_bounded_cas_object(cas, hash, MAX_PROJECT_POLICY_OBJECT_BYTES)?
        .map(|(value, _)| ProjectSnapshotPolicy::from_value(&value))
        .transpose()
}

pub fn load_project_file_bounded(
    cas: &lillux::CasStore,
    hash: &str,
) -> anyhow::Result<Option<ProjectFile>> {
    load_project_file_with_size_bounded(cas, hash).map(|loaded| loaded.map(|(file, _)| file))
}

fn load_project_file_with_size_bounded(
    cas: &lillux::CasStore,
    hash: &str,
) -> anyhow::Result<Option<(ProjectFile, u64)>> {
    read_bounded_cas_object(cas, hash, MAX_PROJECT_FILE_OBJECT_BYTES)?
        .map(|(value, bytes)| ProjectFile::from_value(&value).map(|file| (file, bytes)))
        .transpose()
}

impl std::fmt::Debug for PinnedProjectMaterialization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PinnedProjectMaterialization")
            .field("snapshot_hash", &self.snapshot_hash)
            .field("path", &self.root.path())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StoredSnapshot {
        _root: tempfile::TempDir,
        cas: lillux::CasStore,
        snapshot_hash: String,
        tree_hash: String,
        policy_hash: String,
        file_hash: String,
        blob_hash: String,
    }

    fn stored_snapshot() -> StoredSnapshot {
        let root = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(root.path().join("cas"));
        let bytes = b"name: example\n";
        let blob_hash = cas.store_blob(bytes).unwrap();
        let file = ProjectFile {
            blob_hash: blob_hash.clone(),
            size: bytes.len() as u64,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let file_hash = cas.store_object(&file.to_value()).unwrap();
        let tree = ProjectTree {
            files: BTreeMap::from([(".ai/tools/example.yaml".to_owned(), file_hash.clone())]),
        };
        let tree_hash = cas.store_object(&tree.to_value()).unwrap();
        let policy = ProjectSnapshotPolicy::from_matcher(
            crate::project_sync::ProjectSyncScope::FullProject,
            &crate::ignore::matcher_from_builtins(),
        )
        .unwrap();
        let policy_hash = cas.store_object(&policy.to_value()).unwrap();
        let snapshot = ProjectSnapshot {
            project_tree_hash: tree_hash.clone(),
            effective_policy_hash: policy_hash.clone(),
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-07-28T00:00:00Z".to_owned(),
            source: "test".to_owned(),
        };
        let snapshot_hash = cas.store_object(&snapshot.to_value()).unwrap();
        StoredSnapshot {
            _root: root,
            cas,
            snapshot_hash,
            tree_hash,
            policy_hash,
            file_hash,
            blob_hash,
        }
    }

    fn object_path(cas: &lillux::CasStore, hash: &str) -> std::path::PathBuf {
        lillux::shard_path(cas.root(), "objects", hash, ".json")
    }

    #[test]
    fn verified_snapshot_closure_loads_the_complete_bounded_descriptor_graph() {
        let fixture = stored_snapshot();
        let closure =
            VerifiedProjectSnapshotClosure::load(&fixture.cas, &fixture.snapshot_hash).unwrap();
        assert_eq!(closure.snapshot_hash(), fixture.snapshot_hash);
        assert_eq!(closure.snapshot().project_tree_hash, fixture.tree_hash);
        assert_eq!(
            closure.snapshot().effective_policy_hash,
            fixture.policy_hash
        );
        assert_eq!(
            closure.tree().files()[".ai/tools/example.yaml"].blob_hash,
            fixture.blob_hash
        );
    }

    #[test]
    fn verified_snapshot_closure_rejects_every_missing_descriptor_or_blob_edge() {
        let missing_hash = "f".repeat(64);
        let fixture = stored_snapshot();
        assert!(VerifiedProjectSnapshotClosure::load(&fixture.cas, &missing_hash).is_err());

        let missing_tree = ProjectSnapshot {
            project_tree_hash: missing_hash.clone(),
            effective_policy_hash: fixture.policy_hash.clone(),
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-07-28T00:00:00Z".to_owned(),
            source: "test".to_owned(),
        };
        let missing_tree_hash = fixture.cas.store_object(&missing_tree.to_value()).unwrap();
        assert!(VerifiedProjectSnapshotClosure::load(&fixture.cas, &missing_tree_hash).is_err());

        let missing_policy = ProjectSnapshot {
            project_tree_hash: fixture.tree_hash.clone(),
            effective_policy_hash: missing_hash.clone(),
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-07-28T00:00:00Z".to_owned(),
            source: "test".to_owned(),
        };
        let missing_policy_hash = fixture
            .cas
            .store_object(&missing_policy.to_value())
            .unwrap();
        assert!(VerifiedProjectSnapshotClosure::load(&fixture.cas, &missing_policy_hash).is_err());

        let tree = ProjectTree {
            files: BTreeMap::from([(".ai/tools/missing.yaml".to_owned(), missing_hash.clone())]),
        };
        let tree_hash = fixture.cas.store_object(&tree.to_value()).unwrap();
        assert!(
            VerifiedProjectTreeClosure::load(&fixture.cas, &tree_hash, &fixture.policy_hash)
                .is_err()
        );

        let missing_blob_file = ProjectFile {
            blob_hash: missing_hash,
            size: 1,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let missing_blob_file_hash = fixture
            .cas
            .store_object(&missing_blob_file.to_value())
            .unwrap();
        let tree = ProjectTree {
            files: BTreeMap::from([(
                ".ai/tools/missing-blob.yaml".to_owned(),
                missing_blob_file_hash,
            )]),
        };
        let tree_hash = fixture.cas.store_object(&tree.to_value()).unwrap();
        assert!(
            VerifiedProjectTreeClosure::load(&fixture.cas, &tree_hash, &fixture.policy_hash)
                .is_err()
        );
    }

    #[test]
    fn bounded_object_loader_rejects_corrupt_and_oversized_objects_before_decode() {
        let fixture = stored_snapshot();
        std::fs::write(object_path(&fixture.cas, &fixture.file_hash), b"{}").unwrap();
        assert!(load_project_file_bounded(&fixture.cas, &fixture.file_hash).is_err());

        let oversized_hash = "e".repeat(64);
        let oversized_path = object_path(&fixture.cas, &oversized_hash);
        std::fs::create_dir_all(oversized_path.parent().unwrap()).unwrap();
        let oversized = std::fs::File::create(&oversized_path).unwrap();
        oversized
            .set_len(MAX_PROJECT_FILE_OBJECT_BYTES + 1)
            .unwrap();
        assert!(load_project_file_bounded(&fixture.cas, &oversized_hash).is_err());
    }

    #[test]
    fn verified_tree_closure_rejects_unsafe_depth_and_policy_source_mismatch() {
        let fixture = stored_snapshot();
        let unsafe_tree = serde_json::json!({
            "kind": "project_tree",
            "schema": ProjectTree::SCHEMA,
            "files": {"../escape": fixture.file_hash.clone()},
        });
        let unsafe_hash = fixture.cas.store_object(&unsafe_tree).unwrap();
        assert!(
            VerifiedProjectTreeClosure::load(&fixture.cas, &unsafe_hash, &fixture.policy_hash)
                .is_err()
        );

        let deep_path = (0..=(crate::project_sync::MAX_PROJECT_TREE_DEPTH + 1))
            .map(|index| format!("d{index}"))
            .collect::<Vec<_>>()
            .join("/");
        let deep_tree = ProjectTree {
            files: BTreeMap::from([(deep_path, fixture.file_hash.clone())]),
        };
        let deep_hash = fixture.cas.store_object(&deep_tree.to_value()).unwrap();
        assert!(
            VerifiedProjectTreeClosure::load(&fixture.cas, &deep_hash, &fixture.policy_hash)
                .is_err()
        );

        let policy_source_tree = ProjectTree {
            files: BTreeMap::from([(
                crate::project_sync::PROJECT_SNAPSHOT_CONFIG_RELATIVE.to_owned(),
                fixture.file_hash.clone(),
            )]),
        };
        let policy_source_hash = fixture
            .cas
            .store_object(&policy_source_tree.to_value())
            .unwrap();
        assert!(
            VerifiedProjectTreeClosure::load(
                &fixture.cas,
                &policy_source_hash,
                &fixture.policy_hash,
            )
            .is_err()
        );
    }

    #[test]
    fn verified_tree_closure_rejects_file_count_above_the_exact_limit() {
        let fixture = stored_snapshot();
        let mut files = BTreeMap::new();
        for index in 0..=crate::project_sync::MAX_PROJECT_TREE_FILES {
            files.insert(format!("files/{index:06}"), fixture.file_hash.clone());
        }
        let tree_hash = fixture
            .cas
            .store_object(&ProjectTree { files }.to_value())
            .unwrap();
        let error =
            VerifiedProjectTreeClosure::load(&fixture.cas, &tree_hash, &fixture.policy_hash)
                .unwrap_err();
        assert!(error.to_string().contains(&format!(
            "exceeds {} regular files",
            crate::project_sync::MAX_PROJECT_TREE_FILES
        )));
    }

    fn fixture() -> (std::path::PathBuf, PinnedProjectMaterialization) {
        let root = std::env::temp_dir().join(format!(
            "ryeos-pinned-materialization-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(root.join(".ai/tools")).unwrap();
        let contents = b"name: example\n";
        std::fs::write(root.join(".ai/tools/example.yaml"), contents).unwrap();
        let pinned = lillux::PinnedDirectory::open(&root).unwrap().unwrap();
        let expected_tree = Arc::new(BTreeMap::from([(
            ".ai/tools/example.yaml".to_owned(),
            ProjectFile {
                blob_hash: lillux::sha256_hex(contents),
                size: contents.len() as u64,
                normalized_mode: 0o644,
            },
        )]));
        let cas = Arc::new(lillux::CasStore::new(root.with_extension("cas")));
        for (relative, expected) in expected_tree.iter() {
            let bytes = std::fs::read(root.join(relative)).unwrap();
            assert_eq!(cas.put_blob(&bytes).unwrap().hash, expected.blob_hash);
        }
        (
            root,
            PinnedProjectMaterialization {
                snapshot_hash: "a".repeat(64),
                root: Arc::new(pinned),
                expected_tree,
                cas,
            },
        )
    }

    #[test]
    fn child_mutation_invalidates_pinned_materialization() {
        let (root, materialization) = fixture();
        assert!(materialization.ensure_path_binding().is_ok());
        std::fs::write(root.join(".ai/tools/example.yaml"), b"name: changed\n").unwrap();
        assert!(materialization.ensure_path_binding().is_err());
    }

    #[test]
    fn child_addition_invalidates_pinned_materialization() {
        let (root, materialization) = fixture();
        std::fs::write(root.join(".ai/tools/shadow.yaml"), b"name: shadow\n").unwrap();
        assert!(materialization.ensure_path_binding().is_err());
    }

    #[test]
    fn authoritative_listing_is_prefix_relative_and_reads_from_exact_cas_path() {
        let (root, materialization) = fixture();
        let entries = materialization
            .authoritative_entries_under(".ai/tools", false, 8)
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "example.yaml");

        let bytes = materialization
            .authoritative_file_bounded(".ai/tools/example.yaml", 1024)
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"name: example\n");

        std::fs::write(root.join(".ai/tools/example.yaml"), b"name: mutated\n").unwrap();
        std::fs::write(root.join(".ai/tools/shadow.yaml"), b"name: shadow\n").unwrap();
        let still_authoritative = materialization
            .authoritative_file_bounded(".ai/tools/example.yaml", 1024)
            .unwrap()
            .unwrap();
        assert_eq!(still_authoritative, b"name: example\n");
        let still_exact = materialization
            .authoritative_entries_under(".ai/tools", false, 8)
            .unwrap();
        assert_eq!(still_exact.len(), 1);
        assert_eq!(still_exact[0].0, "example.yaml");
    }

    #[test]
    fn observed_content_proofs_reject_noncanonical_relative_paths() {
        let (root, materialization) = fixture();
        let escaped = root.join("../outside");
        assert!(
            materialization
                .validates_observed_file(&escaped, &"a".repeat(64))
                .is_err()
        );
        assert!(
            materialization
                .validates_observed_absence(&escaped)
                .is_err()
        );
    }
}
