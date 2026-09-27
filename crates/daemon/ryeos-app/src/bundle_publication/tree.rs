//! Exact portable-tree capture for native bundle generations.
//!
//! The generic external-content format admits internal symbolic links. Native
//! bundle publication deliberately uses a narrower profile: real directories
//! and singly-linked regular files with portable `0644`/`0755` modes only.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Component, Path},
};

use anyhow::{Context as _, bail};
use ryeos_state::{
    DigestOnlyExternalContentSink, ExternalCapturePolicy, ExternalContentBlobSink,
    LaunchCaptureBudget, external_content_manifest_digest,
    ignore::{IgnoreConfig, IgnoreMatcher},
    objects::{ExternalContentManifestEntryKind, ExternalContentManifestObject},
};

/// The immutable identity produced by capturing one complete signed bundle
/// tree. The object is stored only after a non-mutating audit and a second,
/// exact capture agree.
#[derive(Debug, Clone)]
pub struct CapturedBundleTree {
    manifest_hash: String,
    manifest: ExternalContentManifestObject,
}

impl CapturedBundleTree {
    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    pub fn manifest(&self) -> &ExternalContentManifestObject {
        &self.manifest
    }
}

/// Audit a bundle tree without retaining any bytes.
#[cfg(unix)]
pub fn inspect_bundle_tree(root: &Path) -> anyhow::Result<ExternalContentManifestObject> {
    let root = lillux::PinnedDirectory::open(root)?
        .with_context(|| format!("bundle tree is absent: {}", root.display()))?;
    let ignore = IgnoreMatcher::from_config(&IgnoreConfig { patterns: vec![] })?;
    let policy = ExternalCapturePolicy::new("bundle".to_owned(), &ignore)?;
    let mut budget = LaunchCaptureBudget::default();
    let mut sink = BundleAuditSink(DigestOnlyExternalContentSink);
    let manifest = ryeos_state::capture_tree(&root, &[], &policy, &mut budget, &mut sink)?;
    validate_native_bundle_tree(&manifest)?;
    root.ensure_path_binding()?;
    Ok(manifest)
}

/// Capture an exact bundle tree into the supplied CAS.
///
/// The first pass is deliberately digest-only. Consequently an unsupported
/// tree cannot leave payload blobs in CAS. The retaining pass must reproduce
/// the same manifest, closing mutation between audit and capture.
#[cfg(unix)]
pub fn capture_bundle_tree(
    root_path: &Path,
    cas: &lillux::CasStore,
) -> anyhow::Result<CapturedBundleTree> {
    let audited = inspect_bundle_tree(root_path)?;
    let root = lillux::PinnedDirectory::open(root_path)?
        .with_context(|| format!("bundle tree is absent: {}", root_path.display()))?;
    let ignore = IgnoreMatcher::from_config(&IgnoreConfig { patterns: vec![] })?;
    let policy = ExternalCapturePolicy::new("bundle".to_owned(), &ignore)?;
    let mut budget = LaunchCaptureBudget::default();
    let mut sink = BundleCasBlobSink { cas };
    let manifest = ryeos_state::capture_tree(&root, &[], &policy, &mut budget, &mut sink)?;
    validate_native_bundle_tree(&manifest)?;
    root.ensure_path_binding()?;
    if manifest != audited {
        bail!("bundle tree changed between audit and retained capture");
    }

    let expected_hash = external_content_manifest_digest(&manifest)?;
    let stored = cas.put_object(&serde_json::to_value(&manifest)?)?;
    if stored.hash != expected_hash {
        bail!("stored bundle manifest disagrees with its canonical identity");
    }
    Ok(CapturedBundleTree {
        manifest_hash: stored.hash,
        manifest,
    })
}

/// Enforce the clean-cut native publication tree profile.
pub fn validate_native_bundle_tree(manifest: &ExternalContentManifestObject) -> anyhow::Result<()> {
    manifest.validate()?;
    for entry in &manifest.entries {
        match entry.kind {
            ExternalContentManifestEntryKind::Dir => {}
            ExternalContentManifestEntryKind::File => match entry.mode {
                Some(0o644 | 0o755) => {}
                Some(mode) => bail!(
                    "bundle entry {} has unsupported portable mode {mode:#o}",
                    entry.path
                ),
                None => bail!("bundle file {} has no portable mode", entry.path),
            },
            ExternalContentManifestEntryKind::Symlink => {
                bail!("bundle entry {} is a symbolic link", entry.path)
            }
        }
    }
    Ok(())
}

/// Materialize an already verified native manifest from local CAS into a new
/// transaction-owned directory. Paths are canonicalized and revalidated
/// before every write; the destination and all parents are newly created, so
/// no pre-existing link can redirect a write. Callers must keep the private
/// staging namespace inaccessible to concurrent mutation for this operation.
#[cfg(unix)]
pub fn materialize_bundle_tree(
    manifest: &ExternalContentManifestObject,
    cas: &lillux::CasStore,
    destination: &Path,
) -> anyhow::Result<()> {
    validate_native_bundle_tree(manifest)?;
    let parent_path = destination
        .parent()
        .context("bundle staging destination has no parent")?;
    let name = destination
        .file_name()
        .context("bundle staging destination has no name")?;
    let parent =
        lillux::PinnedDirectory::open(parent_path)?.context("bundle staging parent is absent")?;
    if parent.open_entry(name, false)?.is_some() {
        bail!("bundle materialization destination already exists");
    }
    let root = parent.create_child(name, 0o700)?;
    let result = (|| {
        let mut directories = BTreeMap::from([(String::new(), root.try_clone()?)]);
        for entry in manifest
            .entries
            .iter()
            .filter(|entry| entry.kind == ExternalContentManifestEntryKind::Dir)
        {
            let relative = safe_relative(&entry.path)?;
            let (parent_key, child_name) = split_parent(relative)?;
            let directory = directories
                .get(parent_key)
                .context("bundle directory parent was not declared first")?
                .create_child(OsStr::new(child_name), 0o755)?;
            directories.insert(entry.path.clone(), directory);
        }
        for entry in manifest
            .entries
            .iter()
            .filter(|entry| entry.kind == ExternalContentManifestEntryKind::File)
        {
            let relative = safe_relative(&entry.path)?;
            let (parent_key, child_name) = split_parent(relative)?;
            let directory = directories
                .get(parent_key)
                .context("bundle file parent is absent from verified manifest")?;
            let hash = entry
                .blob_hash
                .as_deref()
                .context("bundle file omits blob hash")?;
            let bytes = cas
                .get_blob(hash)?
                .context("bundle payload blob is absent")?;
            if bytes.len() as u64 != entry.size.context("bundle file omits size")?
                || lillux::cas::sha256_hex(&bytes) != hash
            {
                bail!("bundle payload differs from manifest identity");
            }
            if directory
                .atomic_create_regular(OsStr::new(child_name), &bytes, entry.mode.unwrap())?
                .is_none()
            {
                bail!("bundle file appeared during descriptor-bound materialization");
            }
        }
        root.ensure_path_binding()?;
        let observed = inspect_bundle_tree(destination)?;
        if &observed != manifest {
            bail!("materialized bundle tree differs from verified manifest");
        }
        root.sync_tree()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = root.remove_contents_recursive();
        let _ = parent.remove_empty_child_if_same(name, &root);
    }
    result
}

#[cfg(unix)]
fn split_parent(path: &Path) -> anyhow::Result<(&str, &str)> {
    let parent = path.parent().and_then(Path::to_str).unwrap_or("");
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .context("bundle path is not UTF-8")?;
    Ok((parent, name))
}

#[cfg(unix)]
fn safe_relative(value: &str) -> anyhow::Result<&Path> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("bundle manifest path is not a canonical relative path");
    }
    Ok(path)
}

struct BundleCasBlobSink<'a> {
    cas: &'a lillux::CasStore,
}

struct BundleAuditSink(DigestOnlyExternalContentSink);

impl ExternalContentBlobSink for BundleAuditSink {
    fn store_file(
        &mut self,
        file: std::fs::File,
        path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = file.metadata()?;
        if metadata.nlink() != 1 {
            bail!("bundle payload {path} is multiply linked");
        }
        let mode = metadata.mode() & 0o7777;
        if mode != 0o644 && mode != 0o755 {
            bail!("bundle payload {path} has unsupported mode {mode:#o}");
        }
        self.0.store_file(file, path, expected_size)
    }
}

impl ExternalContentBlobSink for BundleCasBlobSink<'_> {
    fn store_file(
        &mut self,
        file: std::fs::File,
        path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = file.metadata()?;
        if metadata.nlink() != 1 {
            bail!("bundle payload {path} is multiply linked");
        }
        let mode = metadata.mode() & 0o7777;
        if mode != 0o644 && mode != 0o755 {
            bail!("bundle payload {path} has unsupported mode {mode:#o}");
        }
        let stored = self
            .cas
            .put_blob_from_open_regular_bounded(
                file,
                Path::new(path),
                ryeos_state::MAX_CAPTURE_FILE_BYTES,
            )
            .with_context(|| format!("retain bundle payload {path}"))?;
        if stored.size != expected_size {
            bail!("bundle payload {path} changed size during capture");
        }
        Ok((stored.hash, stored.size))
    }
}

#[cfg(not(unix))]
pub fn inspect_bundle_tree(_root: &Path) -> anyhow::Result<ExternalContentManifestObject> {
    bail!("native bundle tree capture requires descriptor-relative Unix filesystem access")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    #[test]
    fn captures_exact_tree_and_stores_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("bundle");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("data"), b"native bundle\n").unwrap();
        let executable = root.join("run");
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cas = lillux::CasStore::new(temporary.path().join("cas"));

        let captured = capture_bundle_tree(&root, &cas).unwrap();

        assert_eq!(captured.manifest().entry_count, 2);
        assert_eq!(
            cas.get_object(captured.manifest_hash()).unwrap().unwrap(),
            serde_json::to_value(captured.manifest()).unwrap()
        );
    }

    #[test]
    fn inspection_refuses_links_and_nonportable_modes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("bundle");
        std::fs::create_dir(&root).unwrap();
        let private = root.join("private");
        std::fs::write(&private, b"secret").unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(inspect_bundle_tree(&root).is_err());

        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink("private", root.join("link")).unwrap();
        assert!(inspect_bundle_tree(&root).is_err());
    }

    #[test]
    fn inspection_refuses_hard_links() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("bundle");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("first"), b"same inode").unwrap();
        std::fs::hard_link(root.join("first"), root.join("second")).unwrap();

        assert!(inspect_bundle_tree(&root).is_err());
    }
}

#[cfg(not(unix))]
pub fn capture_bundle_tree(
    _root: &Path,
    _cas: &lillux::CasStore,
) -> anyhow::Result<CapturedBundleTree> {
    bail!("native bundle tree capture requires descriptor-relative Unix filesystem access")
}
