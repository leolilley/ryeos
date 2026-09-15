//! External-content selection from an already retained regular-file project tree.
//!
//! Caller authority and durable staging belong to the import owner. This module
//! never opens a workspace or infers links/empty directories absent from ProjectTree.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};

use super::{ExternalContentCaptureKind, LargeContentCapturePolicy};
use crate::objects::{
    ExternalContentManifestEntry, ExternalContentManifestEntryKind as EntryKind,
    ExternalContentManifestObject, ExternalLargeContentManifestEntry,
    ExternalLargeContentManifestObject, ProjectFile,
};
use crate::project_materialization::VerifiedProjectSnapshotClosure;

/// Bounded, policy-selected metadata. File hashes still require verification
/// before publication; this is not a storage or permission capability.
pub struct RetainedProjectContent {
    files: BTreeMap<String, ProjectFile>,
    directories: BTreeSet<String>,
    total_bytes: u64,
}

impl RetainedProjectContent {
    pub fn select(
        snapshot: &VerifiedProjectSnapshotClosure,
        shape: ExternalContentCaptureKind,
        policy: &LargeContentCapturePolicy<'_>,
    ) -> anyhow::Result<Self> {
        Self::select_files(snapshot.tree().files(), shape, policy)
    }

    fn select_files(
        source: &BTreeMap<String, ProjectFile>,
        shape: ExternalContentCaptureKind,
        policy: &LargeContentCapturePolicy<'_>,
    ) -> anyhow::Result<Self> {
        let mut selected = Self {
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
            total_bytes: 0,
        };
        let prefix = format!("{}/", policy.locator_prefix);
        let mut observed = BTreeSet::new();
        for (path, file) in source {
            let relative = match shape {
                ExternalContentCaptureKind::File if path == &policy.locator_prefix => {
                    crate::objects::FILE_REALIZATION_ENTRY_PATH
                }
                ExternalContentCaptureKind::Tree => {
                    let Some(relative) = path.strip_prefix(&prefix) else {
                        continue;
                    };
                    relative
                }
                _ => continue,
            };
            let parents: Vec<_> = relative
                .match_indices('/')
                .map(|(i, _)| &relative[..i])
                .collect();
            let mut excluded = false;
            for entry in parents.iter().copied().chain(std::iter::once(relative)) {
                observed.insert(entry.to_owned());
                if observed.len() > policy.bounds.max_entries {
                    bail!("retained content exceeds the admitted observed entry bound");
                }
                // Observe the excluded directory, but do not traverse or
                // charge hidden descendants. This matches filesystem capture.
                if matches!(shape, ExternalContentCaptureKind::Tree) && policy.excludes(entry) {
                    excluded = true;
                    break;
                }
                if entry != relative && entry.split('/').count() >= policy.bounds.max_depth {
                    bail!("retained content exceeds the admitted directory depth at {path}");
                }
            }
            // A denied ancestor denies its entire subtree, exactly as a walk
            // that never descends into that directory would do.
            if excluded {
                continue;
            }
            file.validate()?;
            if file.size > policy.bounds.max_file_bytes {
                bail!("retained content file {path} exceeds the admitted file bound");
            }
            selected.total_bytes = selected
                .total_bytes
                .checked_add(file.size)
                .context("retained content byte count overflow")?;
            if selected.total_bytes > policy.bounds.max_total_bytes {
                bail!("retained content exceeds the admitted total byte bound");
            }
            selected
                .directories
                .extend(parents.into_iter().map(str::to_owned));
            selected.files.insert(relative.to_owned(), file.clone());
        }
        if selected.files.is_empty() {
            bail!("retained result member contains no admitted regular files");
        }
        Ok(selected)
    }

    pub fn entry_count(&self) -> usize {
        self.files.len() + self.directories.len()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn large_file_bytes(&self) -> u64 {
        self.files
            .values()
            .filter(|file| file.size > crate::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES)
            .map(|file| file.size)
            .sum()
    }

    pub fn expected_file_matches(&self, expected: &str) -> bool {
        self.files.len() == 1
            && self
                .files
                .values()
                .next()
                .is_some_and(|file| file.blob_hash == expected)
    }

    pub fn content_manifest(
        &self,
        cas: &lillux::CasStore,
    ) -> anyhow::Result<ExternalContentManifestObject> {
        let entries = self
            .files
            .iter()
            .map(|(path, file)| ExternalContentManifestEntry {
                path: path.clone(),
                kind: EntryKind::File,
                mode: Some(file.normalized_mode),
                blob_hash: Some(file.blob_hash.clone()),
                size: Some(file.size),
                target: None,
            })
            .chain(
                self.directories
                    .iter()
                    .map(|path| ExternalContentManifestEntry {
                        path: path.clone(),
                        kind: EntryKind::Dir,
                        mode: None,
                        blob_hash: None,
                        size: None,
                        target: None,
                    }),
            )
            .collect();
        let mut manifest = ExternalContentManifestObject {
            schema: crate::objects::EXTERNAL_CONTENT_TREE_SCHEMA.to_owned(),
            kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            entries,
            entry_count: self.entry_count(),
            total_bytes: self.total_bytes,
        };
        manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
        manifest.validate()?;
        for file in self.files.values() {
            verify_blob(cas, file)?;
        }
        Ok(manifest)
    }

    /// Small files share CAS identities. Large files use the existing streaming
    /// ingest and chunk commitment, with the original blob hash as expected SHA.
    /// The caller holds its CAS mutation guard until the final manifest is a
    /// durable root. Partial conversion is not a published result.
    pub fn large_manifest(
        &self,
        cas: &lillux::CasStore,
        store: &crate::LargeObjectStore,
    ) -> anyhow::Result<ExternalLargeContentManifestObject> {
        let mut entries = Vec::with_capacity(self.entry_count());
        for (path, file) in &self.files {
            let mut entry = ExternalLargeContentManifestEntry {
                path: path.clone(),
                kind: EntryKind::File,
                mode: Some(file.normalized_mode),
                blob_hash: None,
                file_sha256: None,
                size: Some(file.size),
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: None,
            };
            if file.size <= crate::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES {
                verify_blob(cas, file)?;
                entry.blob_hash = Some(file.blob_hash.clone());
            } else {
                let (input, size) = cas
                    .open_blob(&file.blob_hash)?
                    .context("retained blob is missing")?;
                if size != file.size {
                    bail!("retained blob size disagrees with ProjectFile");
                }
                let identity = lillux::observe_open_file_identity(&input)?;
                let ingested = store.ingest_open_regular(
                    input,
                    crate::PinnedLargeObjectSourceIdentity {
                        containing_device: identity.device(),
                        inode: identity.inode(),
                        size,
                    },
                    path,
                    Some(&file.blob_hash),
                )?;
                if ingested.size != file.size || ingested.file_sha256 != file.blob_hash {
                    bail!("retained large-content ingest contradicts ProjectFile");
                }
                entry.file_sha256 = Some(ingested.file_sha256);
                entry.chunk_size = Some(ingested.chunk_size);
                entry.chunk_hashes = ingested.chunk_hashes;
            }
            entries.push(entry);
        }
        entries.extend(
            self.directories
                .iter()
                .map(|path| ExternalLargeContentManifestEntry {
                    path: path.clone(),
                    kind: EntryKind::Dir,
                    mode: None,
                    blob_hash: None,
                    file_sha256: None,
                    size: None,
                    chunk_size: None,
                    chunk_hashes: Vec::new(),
                    target: None,
                }),
        );
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let manifest = ExternalLargeContentManifestObject {
            schema: crate::objects::EXTERNAL_LARGE_CONTENT_SCHEMA.to_owned(),
            kind: crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
            entries,
            entry_count: self.entry_count(),
            total_bytes: self.total_bytes,
        };
        manifest.validate()?;
        Ok(manifest)
    }
}

fn verify_blob(cas: &lillux::CasStore, file: &ProjectFile) -> anyhow::Result<()> {
    let (mut input, size) = cas
        .open_blob(&file.blob_hash)?
        .context("retained blob is missing")?;
    if size != file.size {
        bail!("retained blob size disagrees with ProjectFile");
    }
    let (digest, _) = lillux::digest_open_regular_file_stable_exact(&mut input, size)?;
    if digest != file.blob_hash {
        bail!("retained blob failed content verification");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::LargeContentCaptureBounds;
    use super::*;
    use crate::ignore::{IgnoreConfig, IgnoreMatcher};
    use crate::objects::{ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};

    fn bounds() -> LargeContentCaptureBounds {
        LargeContentCaptureBounds {
            max_depth: 8,
            max_entries: 16,
            max_file_bytes: 1024,
            max_total_bytes: 4096,
        }
    }

    fn ignores(patterns: &[&str]) -> IgnoreMatcher {
        IgnoreMatcher::from_config(&IgnoreConfig {
            patterns: patterns.iter().map(|p| (*p).to_owned()).collect(),
        })
        .unwrap()
    }

    fn file(cas: &lillux::CasStore, bytes: &[u8], executable: bool) -> ProjectFile {
        ProjectFile {
            blob_hash: cas.store_blob(bytes).unwrap(),
            size: bytes.len() as u64,
            normalized_mode: if executable {
                ProjectFile::EXECUTABLE_MODE
            } else {
                ProjectFile::REGULAR_MODE
            },
        }
    }

    fn snapshot(
        cas: &lillux::CasStore,
        files: &BTreeMap<String, ProjectFile>,
        message: &str,
    ) -> VerifiedProjectSnapshotClosure {
        let tree = ProjectTree {
            files: files
                .iter()
                .map(|(p, f)| (p.clone(), cas.store_object(&f.to_value()).unwrap()))
                .collect(),
        };
        let policy = ProjectSnapshotPolicy::from_matcher(
            crate::project_sync::ProjectSyncScope::FullProject,
            &ignores(&[]),
        )
        .unwrap();
        let snapshot = ProjectSnapshot {
            project_tree_hash: cas.store_object(&tree.to_value()).unwrap(),
            effective_policy_hash: cas.store_object(&policy.to_value()).unwrap(),
            message: Some(message.to_owned()),
            parent_hashes: Vec::new(),
            created_at: "2026-09-06T00:00:00Z".into(),
            source: "test".into(),
        };
        let hash = cas.store_object(&snapshot.to_value()).unwrap();
        VerifiedProjectSnapshotClosure::load(cas, &hash).unwrap()
    }

    #[test]
    fn retained_payload_identity_excludes_source_snapshot_and_reuses_blobs() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let executable = file(&cas, b"executable", true);
        let data = file(&cas, b"", false);
        let files = BTreeMap::from([
            ("dist/runtime/bin/program".into(), executable.clone()),
            ("dist/runtime/share/marker".into(), data.clone()),
            ("dist/runtime-other/not-selected".into(), executable.clone()),
        ]);
        let ignore = ignores(&[]);
        let policy =
            LargeContentCapturePolicy::new("dist/runtime".into(), &ignore, bounds()).unwrap();
        let first = snapshot(&cas, &files, "production A");
        let second = snapshot(&cas, &files, "production B");
        assert_ne!(first.snapshot_hash(), second.snapshot_hash());
        let a = RetainedProjectContent::select(&first, ExternalContentCaptureKind::Tree, &policy)
            .unwrap()
            .content_manifest(&cas)
            .unwrap();
        let b = RetainedProjectContent::select(&second, ExternalContentCaptureKind::Tree, &policy)
            .unwrap()
            .content_manifest(&cas)
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(a.entry_count, 4);
        assert_eq!(a.total_bytes, executable.size);
        assert_eq!(a.entries[1].path, "bin/program");
        assert_eq!(
            a.entries[1].blob_hash.as_deref(),
            Some(executable.blob_hash.as_str())
        );
        assert_eq!(a.entries[1].mode, Some(0o755));
        assert_eq!(
            a.entries[3].blob_hash.as_deref(),
            Some(data.blob_hash.as_str())
        );
        assert_eq!(a.entries[3].mode, Some(0o644));
    }

    #[test]
    fn retained_file_shape_keeps_empty_file_and_normalized_mode() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let empty = file(&cas, b"", false);
        let files = BTreeMap::from([("dist/marker".into(), empty.clone())]);
        let ignore = ignores(&[]);
        let policy =
            LargeContentCapturePolicy::new("dist/marker".into(), &ignore, bounds()).unwrap();
        let selected =
            RetainedProjectContent::select_files(&files, ExternalContentCaptureKind::File, &policy)
                .unwrap();
        let manifest = selected.content_manifest(&cas).unwrap();
        assert!(manifest.is_file_shaped());
        assert_eq!(
            manifest.entries[0].blob_hash.as_deref(),
            Some(empty.blob_hash.as_str())
        );
        assert_eq!(manifest.total_bytes, 0);
        assert!(
            RetainedProjectContent::select_files(&files, ExternalContentCaptureKind::Tree, &policy)
                .is_err()
        );
    }

    #[test]
    fn retained_regular_tree_matches_filesystem_capture_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let source = temp.path().join("dist/runtime");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::write(source.join("bin/tool"), b"program").unwrap();
        std::fs::write(source.join("marker"), b"").unwrap();
        let ignore = ignores(&[]);
        let root = lillux::PinnedDirectory::open(&source).unwrap().unwrap();
        let captured = super::super::capture_tree(
            &root,
            &[],
            &super::super::ExternalCapturePolicy::new("dist/runtime".into(), &ignore).unwrap(),
            &mut super::super::LaunchCaptureBudget::default(),
            &mut super::super::DigestOnlyExternalContentSink,
        )
        .unwrap();
        let files = BTreeMap::from([
            (
                "dist/runtime/bin/tool".into(),
                file(&cas, b"program", false),
            ),
            ("dist/runtime/marker".into(), file(&cas, b"", false)),
        ]);
        let policy =
            LargeContentCapturePolicy::new("dist/runtime".into(), &ignore, bounds()).unwrap();
        let selected =
            RetainedProjectContent::select_files(&files, ExternalContentCaptureKind::Tree, &policy)
                .unwrap();
        assert_eq!(selected.content_manifest(&cas).unwrap(), captured);
    }

    #[test]
    fn retained_selection_applies_floor_and_ignored_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let f = file(&cas, b"ok", false);
        let files = BTreeMap::from([
            ("dist/runtime/bin/tool".into(), f.clone()),
            (
                "dist/runtime/.ryeos-quarantine.test/hidden".into(),
                f.clone(),
            ),
            ("dist/runtime/cache/hidden".into(), f),
        ]);
        let ignore = ignores(&["dist/runtime/cache"]);
        let policy =
            LargeContentCapturePolicy::new("dist/runtime".into(), &ignore, bounds()).unwrap();
        let selected =
            RetainedProjectContent::select_files(&files, ExternalContentCaptureKind::Tree, &policy)
                .unwrap();
        assert_eq!(selected.entry_count(), 2);
        let policy =
            LargeContentCapturePolicy::new("dist/runtime/cache/hidden".into(), &ignore, bounds());
        if let Ok(policy) = policy {
            assert!(
                RetainedProjectContent::select_files(
                    &files,
                    ExternalContentCaptureKind::File,
                    &policy
                )
                .is_err()
            );
        }
        assert!(LargeContentCapturePolicy::new("../dist".into(), &ignore, bounds()).is_err());
    }

    #[test]
    fn retained_selection_refuses_missing_members_and_policy_overruns() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let files = BTreeMap::from([("dist/bin/tool".into(), file(&cas, b"abc", false))]);
        let ignore = ignores(&[]);
        let policy = LargeContentCapturePolicy::new("missing".into(), &ignore, bounds()).unwrap();
        assert!(
            RetainedProjectContent::select_files(&files, ExternalContentCaptureKind::Tree, &policy)
                .is_err()
        );
        for bound in [
            LargeContentCaptureBounds {
                max_depth: 1,
                ..bounds()
            },
            LargeContentCaptureBounds {
                max_entries: 1,
                ..bounds()
            },
            LargeContentCaptureBounds {
                max_file_bytes: 2,
                ..bounds()
            },
            LargeContentCaptureBounds {
                max_file_bytes: 2,
                max_total_bytes: 2,
                ..bounds()
            },
        ] {
            let policy = LargeContentCapturePolicy::new("dist".into(), &ignore, bound).unwrap();
            assert!(
                RetainedProjectContent::select_files(
                    &files,
                    ExternalContentCaptureKind::Tree,
                    &policy
                )
                .is_err()
            );
        }
    }

    #[test]
    fn retained_manifest_refuses_missing_corrupt_and_wrong_size_blobs() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let original = file(&cas, b"good", false);
        let ignore = ignores(&[]);
        let policy = LargeContentCapturePolicy::new("dist/file".into(), &ignore, bounds()).unwrap();
        for f in [
            ProjectFile {
                blob_hash: "f".repeat(64),
                ..original.clone()
            },
            ProjectFile {
                size: 3,
                ..original.clone()
            },
        ] {
            let selected = RetainedProjectContent::select_files(
                &BTreeMap::from([("dist/file".into(), f)]),
                ExternalContentCaptureKind::File,
                &policy,
            )
            .unwrap();
            assert!(selected.content_manifest(&cas).is_err());
        }
        let selected = RetainedProjectContent::select_files(
            &BTreeMap::from([("dist/file".into(), original.clone())]),
            ExternalContentCaptureKind::File,
            &policy,
        )
        .unwrap();
        std::fs::write(
            lillux::cas::shard_path(cas.root(), "blobs", &original.blob_hash, ""),
            b"evil",
        )
        .unwrap();
        assert!(selected.content_manifest(&cas).is_err());
    }

    #[test]
    fn retained_large_tier_reuses_small_cas_blobs_without_large_store_writes() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let f = file(&cas, b"program", true);
        let ignore = ignores(&[]);
        let policy = LargeContentCapturePolicy::new("dist".into(), &ignore, bounds()).unwrap();
        let selected = RetainedProjectContent::select_files(
            &BTreeMap::from([("dist/bin/tool".into(), f.clone())]),
            ExternalContentCaptureKind::Tree,
            &policy,
        )
        .unwrap();
        let runtime = lillux::PinnedDirectory::open_or_create(temp.path()).unwrap();
        let store = crate::LargeObjectStore::open_or_create_under(&runtime).unwrap();
        let manifest = selected.large_manifest(&cas, &store).unwrap();
        assert_eq!(store.total_stored_bytes().unwrap(), 0);
        assert_eq!(
            manifest.entries[1].blob_hash.as_deref(),
            Some(f.blob_hash.as_str())
        );
        assert_eq!(manifest.entries[1].mode, Some(0o755));
    }

    #[test]
    fn retained_excluded_descendants_consume_no_depth_or_entry_budget() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let f = file(&cas, b"ok", false);
        let mut files = BTreeMap::from([("dist/tool".into(), f.clone())]);
        for i in 0..32 {
            files.insert(format!("dist/cache/a/b/c/{i}"), f.clone());
        }
        let ignore = ignores(&["dist/cache"]);
        let policy = LargeContentCapturePolicy::new(
            "dist".into(),
            &ignore,
            LargeContentCaptureBounds {
                max_depth: 1,
                max_entries: 2,
                ..bounds()
            },
        )
        .unwrap();
        let selected =
            RetainedProjectContent::select_files(&files, ExternalContentCaptureKind::Tree, &policy)
                .unwrap();
        assert_eq!(selected.entry_count(), 1);
        assert_eq!(selected.total_bytes(), 2);
    }

    #[test]
    fn retained_large_conversion_and_import_authority_survive_stage_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let size = crate::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES + 1;
        let f = file(&cas, &vec![0x5a; size as usize], true);
        let small = file(&cas, b"small", false);
        let files = BTreeMap::from([
            ("dist/program".into(), f.clone()),
            ("dist/data".into(), small.clone()),
        ]);
        let ignore = ignores(&[]);
        let policy = LargeContentCapturePolicy::new(
            "dist".into(),
            &ignore,
            LargeContentCaptureBounds {
                max_file_bytes: size,
                max_total_bytes: size + small.size,
                ..bounds()
            },
        )
        .unwrap();
        let selected =
            RetainedProjectContent::select_files(&files, ExternalContentCaptureKind::Tree, &policy)
                .unwrap();
        assert!(selected.content_manifest(&cas).is_err()); // No automatic tier change.
        let runtime = lillux::PinnedDirectory::open_or_create(temp.path()).unwrap();
        let store = crate::LargeObjectStore::open_or_create_under(&runtime).unwrap();
        let recovery = crate::RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        let guard = crate::CasMutationGuard::acquire_shared(temp.path()).unwrap();
        let manifest = selected.large_manifest(&cas, &store).unwrap();
        assert_eq!(
            manifest.entries[1].file_sha256.as_deref(),
            Some(f.blob_hash.as_str())
        );
        assert_eq!(manifest.entries[1].mode, Some(0o755));
        assert!(!manifest.entries[1].chunk_hashes.is_empty());
        store
            .verify_manifest_commitment(&manifest.entries[1])
            .unwrap();
        assert_eq!(selected.large_manifest(&cas, &store).unwrap(), manifest);
        assert_eq!(store.total_stored_bytes().unwrap(), size);
        let owner = "a".repeat(64);
        let key =
            crate::DurableCasPublicationKey::external_content_import(&"b".repeat(64)).unwrap();
        let mut stage = recovery
            .begin_durable_cas_upload_admitted(
                &guard,
                &owner,
                "external-content-import",
                &key,
                None,
            )
            .unwrap();
        stage
            .protect_large_object_hash(&guard, &f.blob_hash)
            .unwrap();
        let hash = cas.store_object(&manifest.to_value().unwrap()).unwrap();
        stage
            .protect_cas_closure(&guard, [hash.as_str()], std::iter::empty())
            .unwrap();
        let id = stage.staging_id().to_owned();
        drop(stage);
        let reopened = recovery
            .open_durable_cas_upload_admitted(&guard, &id, &owner)
            .unwrap();
        reopened.ensure_publication_contract(&key, None).unwrap();
        reopened.ensure_protects_object(&hash).unwrap();
        reopened.ensure_protects_large_object(&f.blob_hash).unwrap();
        let roots = recovery.active_staged_cas_root_hashes().unwrap();
        assert_eq!(roots.object_hashes.len(), 1);
        assert!(roots.blob_hashes.is_empty());
        assert_eq!(roots.large_object_hashes, vec![f.blob_hash.clone()]);
        let closure =
            crate::object_closure::collect_object_closure_with_cas(&cas, roots.object_hashes)
                .unwrap();
        assert!(closure.is_complete());
        assert!(closure.blob_hashes.contains(&small.blob_hash));
        assert!(closure.large_object_hashes.contains(&f.blob_hash));
        assert!(!closure.blob_hashes.contains(&f.blob_hash));
    }
}
