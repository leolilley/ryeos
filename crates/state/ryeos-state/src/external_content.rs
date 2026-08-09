//! Verified CAS closure for one realized external-content manifest.

use std::collections::BTreeMap;
use std::ffi::OsStr;

use anyhow::Context as _;

use crate::objects::{
    ExternalContentManifestEntryKind, ExternalContentManifestObject,
    MAX_EXTERNAL_CONTENT_FILE_BYTES, MAX_EXTERNAL_CONTENT_MANIFEST_BYTES, MAX_SYMLINK_TARGET_BYTES,
};

/// Manifest plus every verified payload required to materialize it.
///
/// Construction is the authority boundary: callers cannot obtain this value
/// from hash-shaped metadata or blob presence alone. Every object/blob is
/// loaded through its exact content address and checked against the manifest's
/// semantic bounds.
#[derive(Debug, Clone)]
pub struct VerifiedExternalContentClosure {
    manifest_hash: String,
    manifest: ExternalContentManifestObject,
    verified_blob_sizes: BTreeMap<String, u64>,
}

pub const MAX_CAPTURE_ENTRIES: usize = crate::objects::MAX_EXTERNAL_CONTENT_ENTRIES;
pub const MAX_CAPTURE_BYTES: u64 = crate::objects::MAX_EXTERNAL_CONTENT_TOTAL_BYTES;
pub const MAX_CAPTURE_FILE_BYTES: u64 = crate::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES;
pub const MAX_CAPTURE_DEPTH: usize = 64;

/// Node-admitted capture policy. The caller supplies a canonical path prefix
/// for the selected named root; this module owns only meaning-blind matching.
pub struct ExternalCapturePolicy<'a> {
    locator_prefix: String,
    configured_ignore: &'a crate::ignore::IgnoreMatcher,
}

impl<'a> ExternalCapturePolicy<'a> {
    pub fn new(
        locator_prefix: String,
        configured_ignore: &'a crate::ignore::IgnoreMatcher,
    ) -> anyhow::Result<Self> {
        crate::objects::validate_canonical_project_relative_path(&locator_prefix)?;
        let policy = Self {
            locator_prefix,
            configured_ignore,
        };
        if policy.excludes_complete_path(&policy.locator_prefix) {
            anyhow::bail!("external content locator is excluded by admitted capture policy");
        }
        Ok(policy)
    }

    pub fn locator_prefix(&self) -> &str {
        &self.locator_prefix
    }

    fn excludes(&self, manifest_relative_path: &str) -> bool {
        self.excludes_complete_path(&format!(
            "{}/{}",
            self.locator_prefix, manifest_relative_path
        ))
    }

    fn excludes_complete_path(&self, path: &str) -> bool {
        crate::ignore::durable_capture_floor().is_ignored(path)
            || self.configured_ignore.is_ignored(path)
    }
}

/// Aggregate bounds shared across every declaration in one launch.
#[derive(Debug, Clone)]
pub struct LaunchCaptureBudget {
    max_depth: usize,
    max_file_bytes: u64,
    max_entries: usize,
    max_total_bytes: u64,
    remaining_entries: usize,
    remaining_bytes: u64,
}

impl Default for LaunchCaptureBudget {
    fn default() -> Self {
        Self {
            max_depth: MAX_CAPTURE_DEPTH,
            max_file_bytes: MAX_CAPTURE_FILE_BYTES,
            max_entries: MAX_CAPTURE_ENTRIES,
            max_total_bytes: MAX_CAPTURE_BYTES,
            remaining_entries: MAX_CAPTURE_ENTRIES,
            remaining_bytes: MAX_CAPTURE_BYTES,
        }
    }
}

impl LaunchCaptureBudget {
    /// Construct a stricter budget when node policy is narrower than the
    /// external-content manifest's wire bounds.
    pub fn bounded(
        max_depth: usize,
        max_entries: usize,
        max_file_bytes: u64,
        max_total_bytes: u64,
    ) -> anyhow::Result<Self> {
        if max_depth == 0
            || max_depth > MAX_CAPTURE_DEPTH
            || max_entries == 0
            || max_entries > MAX_CAPTURE_ENTRIES
            || max_file_bytes == 0
            || max_file_bytes > MAX_CAPTURE_FILE_BYTES
            || max_total_bytes == 0
            || max_total_bytes > MAX_CAPTURE_BYTES
            || max_file_bytes > max_total_bytes
        {
            anyhow::bail!("external content capture budget is outside the manifest contract");
        }
        Ok(Self {
            max_depth,
            max_file_bytes,
            max_entries,
            max_total_bytes,
            remaining_entries: max_entries,
            remaining_bytes: max_total_bytes,
        })
    }

    fn ensure_depth(&self, depth: usize, path: &str) -> anyhow::Result<()> {
        if depth >= self.max_depth {
            anyhow::bail!(
                "external content exceeds {} directory levels at {path}",
                self.max_depth
            );
        }
        Ok(())
    }

    fn ensure_file_bytes(&self, bytes: u64, path: &str) -> anyhow::Result<()> {
        if bytes > self.max_file_bytes {
            anyhow::bail!(
                "external content file {path} is {bytes} bytes; the admitted bound is {}",
                self.max_file_bytes
            );
        }
        Ok(())
    }

    pub fn charge_entry(&mut self) -> anyhow::Result<()> {
        self.remaining_entries = self.remaining_entries.checked_sub(1).ok_or_else(|| {
            anyhow::anyhow!(
                "external content exceeds {} aggregate entries",
                self.max_entries
            )
        })?;
        Ok(())
    }

    pub fn charge_bytes(&mut self, bytes: u64) -> anyhow::Result<()> {
        self.remaining_bytes = self.remaining_bytes.checked_sub(bytes).ok_or_else(|| {
            anyhow::anyhow!(
                "external content exceeds {} aggregate bytes",
                self.max_total_bytes
            )
        })?;
        Ok(())
    }
}

pub trait ExternalContentBlobSink {
    fn store_file(
        &mut self,
        file: std::fs::File,
        path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)>;

    fn store_target(&mut self, target: &[u8], path: &str) -> anyhow::Result<String>;
}

/// Node-admitted bounds for one large-content import. These are supplied by
/// signed daemon policy; state owns enforcement but no defaults or host paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LargeContentCaptureBounds {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl LargeContentCaptureBounds {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_depth == 0 || self.max_depth > 256 {
            anyhow::bail!("large-content capture depth bound is invalid");
        }
        if self.max_entries == 0
            || self.max_entries > crate::objects::MAX_LARGE_CONTENT_MANIFEST_ENTRIES
        {
            anyhow::bail!("large-content capture entry bound exceeds the manifest contract");
        }
        if self.max_file_bytes == 0
            || self.max_file_bytes > crate::objects::MAX_LARGE_CONTENT_FILE_BYTES
            || self.max_total_bytes == 0
            || self.max_total_bytes > crate::objects::MAX_LARGE_CONTENT_TOTAL_BYTES
            || self.max_file_bytes > self.max_total_bytes
        {
            anyhow::bail!("large-content capture byte bounds are incoherent");
        }
        Ok(())
    }
}

/// Capture policy over the full named-root-relative path. The non-bypassable
/// floor and the configured node matcher are the same shared policy used by
/// project/external CAS capture; large import does not grow a second ignore
/// vocabulary.
pub struct LargeContentCapturePolicy<'a> {
    locator_prefix: String,
    configured_ignore: &'a crate::ignore::IgnoreMatcher,
    bounds: LargeContentCaptureBounds,
}

impl<'a> LargeContentCapturePolicy<'a> {
    pub fn new(
        locator_prefix: String,
        configured_ignore: &'a crate::ignore::IgnoreMatcher,
        bounds: LargeContentCaptureBounds,
    ) -> anyhow::Result<Self> {
        crate::objects::validate_canonical_project_relative_path(&locator_prefix)?;
        bounds.validate()?;
        let policy = Self {
            locator_prefix,
            configured_ignore,
            bounds,
        };
        if policy.excludes_complete_path(&policy.locator_prefix) {
            anyhow::bail!("large-content locator is excluded by node capture policy");
        }
        Ok(policy)
    }

    fn excludes(&self, relative: &str) -> bool {
        self.excludes_complete_path(&format!("{}/{}", self.locator_prefix, relative))
    }

    fn excludes_complete_path(&self, path: &str) -> bool {
        crate::ignore::durable_capture_floor().is_ignored(path)
            || self.configured_ignore.is_ignored(path)
    }
}

/// Storage seam used by the descriptor walker. A large realization is a
/// complete tree, not a bag of large files: bounded files and link targets use
/// CAS while files above the content-tier ceiling use [`crate::LargeObjectStore`].
/// Implementations durably root both stores in the caller's upload stage.
pub trait ExternalLargeContentSink {
    fn store_large_file(
        &mut self,
        file: std::fs::File,
        identity: crate::large_object_store::PinnedLargeObjectSourceIdentity,
        relative_path: &str,
        expected_sha256: Option<&str>,
    ) -> anyhow::Result<crate::large_object_store::IngestedLargeObject>;

    fn store_content_file(
        &mut self,
        file: std::fs::File,
        relative_path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)>;

    fn store_target(&mut self, target: &[u8], relative_path: &str) -> anyhow::Result<String>;
}

#[cfg(unix)]
pub fn capture_large_tree(
    root: &lillux::PinnedDirectory,
    policy: &LargeContentCapturePolicy<'_>,
    sink: &mut dyn ExternalLargeContentSink,
) -> anyhow::Result<crate::objects::ExternalLargeContentManifestObject> {
    let (root_device, _) = root.device_inode()?;
    let mut state = LargeCaptureState {
        observed_entries: 0,
        total_bytes: 0,
        entries: Vec::new(),
    };
    capture_large_directory(root, "", 0, root_device, policy, sink, &mut state)?;
    if state.entries.is_empty() {
        anyhow::bail!("large-content tree contains no admitted entries");
    }
    state
        .entries
        .sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let manifest = crate::objects::ExternalLargeContentManifestObject {
        schema: crate::objects::EXTERNAL_LARGE_CONTENT_SCHEMA.to_owned(),
        kind: crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
        entry_count: state.entries.len(),
        entries: state.entries,
        total_bytes: state.total_bytes,
    };
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(unix)]
pub fn capture_large_file(
    file: std::fs::File,
    identity: crate::large_object_store::PinnedLargeObjectSourceIdentity,
    display_path: &str,
    expected_sha256: Option<&str>,
    policy: &LargeContentCapturePolicy<'_>,
    sink: &mut dyn ExternalLargeContentSink,
) -> anyhow::Result<crate::objects::ExternalLargeContentManifestObject> {
    if identity.size > policy.bounds.max_file_bytes {
        anyhow::bail!("large-content file {display_path} is outside the admitted file bound");
    }
    if identity.size > policy.bounds.max_total_bytes {
        anyhow::bail!("large-content file {display_path} exceeds the admitted aggregate bound");
    }
    let mode = lillux::normalized_portable_regular_mode(&file.metadata()?)?;
    let (entry, stored_size) = if identity.size <= MAX_EXTERNAL_CONTENT_FILE_BYTES {
        let (blob_hash, stored_size) =
            sink.store_content_file(file, display_path, identity.size)?;
        if expected_sha256.is_some_and(|expected| expected != blob_hash) {
            anyhow::bail!("large-content file {display_path} contradicts its expected digest");
        }
        (
            crate::objects::ExternalLargeContentManifestEntry {
                path: crate::objects::FILE_REALIZATION_ENTRY_PATH.to_owned(),
                kind: ExternalContentManifestEntryKind::File,
                mode: Some(mode),
                blob_hash: Some(blob_hash),
                file_sha256: None,
                size: Some(stored_size),
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: None,
                target_blob: None,
            },
            stored_size,
        )
    } else {
        let ingested = sink.store_large_file(file, identity, display_path, expected_sha256)?;
        let stored_size = ingested.size;
        (
            crate::objects::ExternalLargeContentManifestEntry {
                path: crate::objects::FILE_REALIZATION_ENTRY_PATH.to_owned(),
                kind: ExternalContentManifestEntryKind::File,
                mode: Some(mode),
                blob_hash: None,
                file_sha256: Some(ingested.file_sha256),
                size: Some(ingested.size),
                chunk_size: Some(ingested.chunk_size),
                chunk_hashes: ingested.chunk_hashes,
                target: None,
                target_blob: None,
            },
            stored_size,
        )
    };
    if stored_size != identity.size {
        anyhow::bail!("large-content file {display_path} changed size during ingest");
    }
    let manifest = crate::objects::ExternalLargeContentManifestObject {
        schema: crate::objects::EXTERNAL_LARGE_CONTENT_SCHEMA.to_owned(),
        kind: crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
        entries: vec![entry],
        entry_count: 1,
        total_bytes: stored_size,
    };
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(unix)]
struct LargeCaptureState {
    observed_entries: usize,
    total_bytes: u64,
    entries: Vec<crate::objects::ExternalLargeContentManifestEntry>,
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn capture_large_directory(
    directory: &lillux::PinnedDirectory,
    prefix: &str,
    depth: usize,
    root_device: u64,
    policy: &LargeContentCapturePolicy<'_>,
    sink: &mut dyn ExternalLargeContentSink,
    state: &mut LargeCaptureState,
) -> anyhow::Result<()> {
    if depth >= policy.bounds.max_depth {
        anyhow::bail!("large-content capture exceeds its depth bound at {prefix:?}");
    }
    for entry in directory.entries_no_follow()? {
        let name = entry
            .name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("large-content path under {prefix:?} is not UTF-8"))?
            .to_owned();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        crate::objects::validate_canonical_project_relative_path(&path)?;
        if policy.excludes(&path) {
            continue;
        }
        state.observed_entries = state
            .observed_entries
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("large-content entry count overflow"))?;
        if state.observed_entries > policy.bounds.max_entries {
            anyhow::bail!("large-content capture exceeds its entry bound");
        }
        if entry.containing_device != root_device {
            anyhow::bail!("large-content entry {path} crosses the admitted filesystem boundary");
        }
        match entry.entry_type {
            lillux::PinnedEntryType::Directory => {
                state
                    .entries
                    .push(crate::objects::ExternalLargeContentManifestEntry {
                        path: path.clone(),
                        kind: ExternalContentManifestEntryKind::Dir,
                        mode: None,
                        blob_hash: None,
                        file_sha256: None,
                        size: None,
                        chunk_size: None,
                        chunk_hashes: Vec::new(),
                        target: None,
                        target_blob: None,
                    });
                let child = directory
                    .open_child_directory(OsStr::new(&name))?
                    .ok_or_else(|| anyhow::anyhow!("large-content directory {path} vanished"))?;
                capture_large_directory(
                    &child,
                    &path,
                    depth + 1,
                    root_device,
                    policy,
                    sink,
                    state,
                )?;
            }
            lillux::PinnedEntryType::Regular => {
                let file = directory
                    .open_regular(OsStr::new(&name), false)?
                    .ok_or_else(|| anyhow::anyhow!("large-content file {path} vanished"))?;
                let metadata = file.metadata()?;
                use std::os::unix::fs::MetadataExt as _;
                if !metadata.file_type().is_file()
                    || metadata.dev() != entry.containing_device
                    || metadata.ino() != entry.inode
                {
                    anyhow::bail!("large-content file {path} changed identity during traversal");
                }
                let size = metadata.len();
                if size > policy.bounds.max_file_bytes {
                    anyhow::bail!("large-content file {path} is outside the admitted file bound");
                }
                state.total_bytes = state
                    .total_bytes
                    .checked_add(size)
                    .ok_or_else(|| anyhow::anyhow!("large-content byte count overflow"))?;
                if state.total_bytes > policy.bounds.max_total_bytes {
                    anyhow::bail!("large-content capture exceeds its aggregate byte bound");
                }
                let mode = lillux::normalized_portable_regular_mode(&metadata)?;
                let manifest_entry = if size <= MAX_EXTERNAL_CONTENT_FILE_BYTES {
                    let (blob_hash, stored_size) = sink.store_content_file(file, &path, size)?;
                    if stored_size != size {
                        anyhow::bail!("large-content file {path} changed size during ingest");
                    }
                    crate::objects::ExternalLargeContentManifestEntry {
                        path,
                        kind: ExternalContentManifestEntryKind::File,
                        mode: Some(mode),
                        blob_hash: Some(blob_hash),
                        file_sha256: None,
                        size: Some(size),
                        chunk_size: None,
                        chunk_hashes: Vec::new(),
                        target: None,
                        target_blob: None,
                    }
                } else {
                    let ingested = sink.store_large_file(
                        file,
                        crate::large_object_store::PinnedLargeObjectSourceIdentity {
                            containing_device: entry.containing_device,
                            inode: entry.inode,
                            size,
                        },
                        &path,
                        None,
                    )?;
                    if ingested.size != size {
                        anyhow::bail!("large-content file {path} changed size during ingest");
                    }
                    crate::objects::ExternalLargeContentManifestEntry {
                        path,
                        kind: ExternalContentManifestEntryKind::File,
                        mode: Some(mode),
                        blob_hash: None,
                        file_sha256: Some(ingested.file_sha256),
                        size: Some(ingested.size),
                        chunk_size: Some(ingested.chunk_size),
                        chunk_hashes: ingested.chunk_hashes,
                        target: None,
                        target_blob: None,
                    }
                };
                state.entries.push(manifest_entry);
            }
            lillux::PinnedEntryType::Symlink => {
                let target = directory
                    .read_symlink_target(
                        OsStr::new(&name),
                        crate::objects::MAX_SYMLINK_TARGET_BYTES as usize,
                    )?
                    .ok_or_else(|| anyhow::anyhow!("large-content symlink {path} vanished"))?;
                if target.is_empty() || target.contains(&0) {
                    anyhow::bail!("large-content symlink {path} has an invalid target");
                }
                let (inline, target_blob) = match String::from_utf8(target.clone()) {
                    Ok(text) if target.len() <= crate::objects::MAX_INLINE_SYMLINK_TARGET_BYTES => {
                        (Some(text), None)
                    }
                    _ => (None, Some(sink.store_target(&target, &path)?)),
                };
                state
                    .entries
                    .push(crate::objects::ExternalLargeContentManifestEntry {
                        path,
                        kind: ExternalContentManifestEntryKind::Symlink,
                        mode: None,
                        blob_hash: None,
                        file_sha256: None,
                        size: None,
                        chunk_size: None,
                        chunk_hashes: Vec::new(),
                        target: inline,
                        target_blob,
                    });
            }
            other => anyhow::bail!(
                "large-content entry {path} is {other:?}; only files, directories, and symlinks are supported"
            ),
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn capture_tree(
    root: &lillux::PinnedDirectory,
    authored_excludes: &[String],
    policy: &ExternalCapturePolicy<'_>,
    budget: &mut LaunchCaptureBudget,
    sink: &mut dyn ExternalContentBlobSink,
) -> anyhow::Result<ExternalContentManifestObject> {
    let (root_device, _) = root.device_inode()?;
    let mut entries = Vec::new();
    let mut declaration_entries = 0usize;
    let mut declaration_bytes = 0u64;
    let mut total_bytes = 0u64;
    capture_directory(
        root,
        "",
        0,
        root_device,
        authored_excludes,
        policy,
        budget,
        sink,
        &mut entries,
        &mut declaration_entries,
        &mut declaration_bytes,
        &mut total_bytes,
    )?;
    entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let manifest = ExternalContentManifestObject {
        schema: crate::objects::EXTERNAL_CONTENT_TREE_SCHEMA.to_owned(),
        kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
        entry_count: entries.len(),
        entries,
        total_bytes,
    };
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(unix)]
pub fn capture_file(
    file: std::fs::File,
    display_path: &str,
    budget: &mut LaunchCaptureBudget,
    sink: &mut dyn ExternalContentBlobSink,
) -> anyhow::Result<ExternalContentManifestObject> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("external content locator is not a regular file: {display_path}");
    }
    let size = metadata.len();
    budget.ensure_file_bytes(size, display_path)?;
    budget.charge_entry()?;
    budget.charge_bytes(size)?;
    let mode = lillux::normalized_portable_regular_mode(&metadata)?;
    let (blob_hash, stored_size) = sink.store_file(file, display_path, size)?;
    if stored_size != size {
        anyhow::bail!("external content file {display_path} changed size during capture");
    }
    let manifest = ExternalContentManifestObject {
        schema: crate::objects::EXTERNAL_CONTENT_TREE_SCHEMA.to_owned(),
        kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
        entries: vec![crate::objects::ExternalContentManifestEntry {
            path: crate::objects::FILE_REALIZATION_ENTRY_PATH.to_owned(),
            kind: ExternalContentManifestEntryKind::File,
            mode: Some(mode),
            blob_hash: Some(blob_hash),
            size: Some(size),
            target: None,
            target_blob: None,
        }],
        entry_count: 1,
        total_bytes: size,
    };
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn capture_directory(
    directory: &lillux::PinnedDirectory,
    prefix: &str,
    depth: usize,
    root_device: u64,
    authored_excludes: &[String],
    policy: &ExternalCapturePolicy<'_>,
    budget: &mut LaunchCaptureBudget,
    sink: &mut dyn ExternalContentBlobSink,
    entries: &mut Vec<crate::objects::ExternalContentManifestEntry>,
    declaration_entries: &mut usize,
    declaration_bytes: &mut u64,
    total_bytes: &mut u64,
) -> anyhow::Result<()> {
    budget.ensure_depth(depth, prefix)?;
    for entry in directory.entries_no_follow()? {
        let name = entry
            .name
            .to_str()
            .ok_or_else(|| {
                anyhow::anyhow!("external content entry under {prefix:?} is not valid UTF-8")
            })?
            .to_owned();
        if author_excluded(&name, authored_excludes) {
            continue;
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        crate::objects::validate_canonical_project_relative_path(&path)?;
        if policy.excludes(&path) {
            continue;
        }
        if entry.containing_device != root_device {
            anyhow::bail!(
                "external content entry {path} is on a different filesystem from its declared root"
            );
        }
        *declaration_entries = declaration_entries
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("external content entry count overflow"))?;
        if *declaration_entries > MAX_CAPTURE_ENTRIES {
            anyhow::bail!("external content declaration exceeds {MAX_CAPTURE_ENTRIES} entries");
        }
        budget.charge_entry()?;
        match entry.entry_type {
            lillux::PinnedEntryType::Directory => {
                entries.push(crate::objects::ExternalContentManifestEntry {
                    path: path.clone(),
                    kind: ExternalContentManifestEntryKind::Dir,
                    mode: None,
                    blob_hash: None,
                    size: None,
                    target: None,
                    target_blob: None,
                });
                let child = directory
                    .open_child_directory(OsStr::new(&name))?
                    .ok_or_else(|| anyhow::anyhow!("external content directory {path} vanished"))?;
                capture_directory(
                    &child,
                    &path,
                    depth + 1,
                    root_device,
                    authored_excludes,
                    policy,
                    budget,
                    sink,
                    entries,
                    declaration_entries,
                    declaration_bytes,
                    total_bytes,
                )?;
            }
            lillux::PinnedEntryType::Regular => {
                let file = directory
                    .open_regular(OsStr::new(&name), false)?
                    .ok_or_else(|| anyhow::anyhow!("external content file {path} vanished"))?;
                let metadata = file.metadata()?;
                let size = metadata.len();
                budget.ensure_file_bytes(size, &path)?;
                *declaration_bytes = declaration_bytes
                    .checked_add(size)
                    .ok_or_else(|| anyhow::anyhow!("external content byte count overflow"))?;
                if *declaration_bytes > MAX_CAPTURE_BYTES {
                    anyhow::bail!("external content declaration exceeds {MAX_CAPTURE_BYTES} bytes");
                }
                budget.charge_bytes(size)?;
                *total_bytes = total_bytes
                    .checked_add(size)
                    .ok_or_else(|| anyhow::anyhow!("external content total byte count overflow"))?;
                let mode = lillux::normalized_portable_regular_mode(&metadata)?;
                let (blob_hash, stored_size) = sink.store_file(file, &path, size)?;
                if stored_size != size {
                    anyhow::bail!("external content file {path} changed size during capture");
                }
                entries.push(crate::objects::ExternalContentManifestEntry {
                    path,
                    kind: ExternalContentManifestEntryKind::File,
                    mode: Some(mode),
                    blob_hash: Some(blob_hash),
                    size: Some(size),
                    target: None,
                    target_blob: None,
                });
            }
            lillux::PinnedEntryType::Symlink => {
                let target = directory
                    .read_symlink_target(
                        OsStr::new(&name),
                        crate::objects::MAX_SYMLINK_TARGET_BYTES as usize,
                    )?
                    .ok_or_else(|| anyhow::anyhow!("external content symlink {path} vanished"))?;
                if target.is_empty() || target.contains(&0) {
                    anyhow::bail!("external content symlink {path} has an invalid target");
                }
                let (inline, target_blob) = match String::from_utf8(target.clone()) {
                    Ok(text) if target.len() <= crate::objects::MAX_INLINE_SYMLINK_TARGET_BYTES => {
                        (Some(text), None)
                    }
                    _ => (None, Some(sink.store_target(&target, &path)?)),
                };
                entries.push(crate::objects::ExternalContentManifestEntry {
                    path,
                    kind: ExternalContentManifestEntryKind::Symlink,
                    mode: None,
                    blob_hash: None,
                    size: None,
                    target: inline,
                    target_blob,
                });
            }
            other => anyhow::bail!(
                "external content entry {path} is {other:?}; only files, directories, and symlinks are supported"
            ),
        }
    }
    Ok(())
}

fn author_excluded(name: &str, excludes: &[String]) -> bool {
    excludes
        .iter()
        .any(|pattern| match pattern.strip_prefix('*') {
            Some(suffix) => name.ends_with(suffix),
            None => name == pattern,
        })
}

impl VerifiedExternalContentClosure {
    pub fn load(cas: &lillux::CasStore, manifest_hash: &str) -> anyhow::Result<Self> {
        let value = crate::object_closure::load_exact_cas_object_with_cas(
            cas,
            manifest_hash,
            MAX_EXTERNAL_CONTENT_MANIFEST_BYTES as u64,
        )
        .with_context(|| format!("load external content manifest {manifest_hash}"))?;
        let manifest = ExternalContentManifestObject::from_value(&value)?;
        let mut verified_blob_sizes = BTreeMap::new();

        for entry in &manifest.entries {
            let (hash, max_bytes, expected_size) = match entry.kind {
                ExternalContentManifestEntryKind::File => (
                    entry
                        .blob_hash
                        .as_deref()
                        .expect("validated file entry has blob hash"),
                    MAX_EXTERNAL_CONTENT_FILE_BYTES,
                    entry.size,
                ),
                ExternalContentManifestEntryKind::Symlink => {
                    let Some(hash) = entry.target_blob.as_deref() else {
                        continue;
                    };
                    (hash, MAX_SYMLINK_TARGET_BYTES, None)
                }
                ExternalContentManifestEntryKind::Dir => continue,
            };
            let bytes = crate::object_closure::load_exact_cas_blob_with_cas(cas, hash, max_bytes)
                .with_context(|| {
                format!(
                    "load external content blob {hash} for manifest entry {}",
                    entry.path
                )
            })?;
            let size = u64::try_from(bytes.len())
                .map_err(|_| anyhow::anyhow!("external content blob size overflow"))?;
            if let Some(expected_size) = expected_size
                && size != expected_size
            {
                anyhow::bail!(
                    "external content blob {hash} for {} has size {size}, expected {expected_size}",
                    entry.path
                );
            }
            if entry.kind == ExternalContentManifestEntryKind::Symlink
                && (bytes.is_empty() || bytes.contains(&0))
            {
                anyhow::bail!(
                    "external content symlink target blob {hash} for {} is invalid",
                    entry.path
                );
            }
            verified_blob_sizes.insert(hash.to_string(), size);
        }

        Ok(Self {
            manifest_hash: manifest_hash.to_string(),
            manifest,
            verified_blob_sizes,
        })
    }

    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    pub fn manifest(&self) -> &ExternalContentManifestObject {
        &self.manifest
    }

    pub fn verified_blob_sizes(&self) -> &BTreeMap<String, u64> {
        &self.verified_blob_sizes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cas() -> (tempfile::TempDir, lillux::CasStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cas");
        std::fs::create_dir_all(&root).unwrap();
        (dir, lillux::CasStore::new(root))
    }

    fn file_entry(
        path: &str,
        blob_hash: &str,
        size: u64,
    ) -> crate::objects::ExternalContentManifestEntry {
        crate::objects::ExternalContentManifestEntry {
            path: path.to_string(),
            kind: ExternalContentManifestEntryKind::File,
            mode: Some(0o644),
            blob_hash: Some(blob_hash.to_string()),
            size: Some(size),
            target: None,
            target_blob: None,
        }
    }

    fn manifest(
        entries: Vec<crate::objects::ExternalContentManifestEntry>,
        total_bytes: u64,
    ) -> ExternalContentManifestObject {
        ExternalContentManifestObject {
            schema: crate::objects::EXTERNAL_CONTENT_TREE_SCHEMA.to_string(),
            kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_string(),
            entry_count: entries.len(),
            entries,
            total_bytes,
        }
    }

    #[test]
    fn a_stored_manifest_closure_round_trips_from_cas() {
        let (_dir, cas) = temp_cas();
        let alpha = cas.store_blob(b"alpha bytes").unwrap();
        let omega = cas.store_blob(b"omega").unwrap();
        let stored = manifest(
            vec![
                file_entry("alpha.txt", &alpha, 11),
                file_entry("omega.txt", &omega, 5),
            ],
            16,
        );
        let hash = cas
            .store_object(&serde_json::to_value(&stored).unwrap())
            .unwrap();

        let closure = VerifiedExternalContentClosure::load(&cas, &hash).unwrap();
        assert_eq!(closure.manifest_hash(), hash);
        assert_eq!(closure.manifest(), &stored);
        assert_eq!(closure.verified_blob_sizes().get(&alpha), Some(&11));
        assert_eq!(closure.verified_blob_sizes().get(&omega), Some(&5));
    }

    #[test]
    fn closure_load_refuses_missing_and_lying_payloads() {
        let (_dir, cas) = temp_cas();

        // A manifest naming a blob CAS does not hold is unavailable, never
        // partially verified.
        let absent = manifest(vec![file_entry("ghost.txt", &"f".repeat(64), 3)], 3);
        let hash = cas
            .store_object(&serde_json::to_value(&absent).unwrap())
            .unwrap();
        let error = VerifiedExternalContentClosure::load(&cas, &hash).unwrap_err();
        assert!(format!("{error:#}").contains("ghost.txt"), "got {error:#}");

        // A manifest asserting a size its blob does not have is a lie about
        // executable identity, not a tolerable drift.
        let blob = cas.store_blob(b"12345").unwrap();
        let lying = manifest(vec![file_entry("short.txt", &blob, 6)], 6);
        let hash = cas
            .store_object(&serde_json::to_value(&lying).unwrap())
            .unwrap();
        let error = VerifiedExternalContentClosure::load(&cas, &hash).unwrap_err();
        assert!(format!("{error:#}").contains("expected 6"), "got {error:#}");
    }

    #[test]
    fn node_narrowed_capture_budget_is_enforced_exactly() {
        let mut budget = LaunchCaptureBudget::bounded(2, 1, 4, 5).unwrap();
        budget.ensure_depth(1, "nested").unwrap();
        assert!(budget.ensure_depth(2, "too/deep").is_err());
        budget.ensure_file_bytes(4, "four.bin").unwrap();
        assert!(budget.ensure_file_bytes(5, "five.bin").is_err());
        budget.charge_entry().unwrap();
        let entry_error = budget.charge_entry().unwrap_err().to_string();
        assert!(entry_error.contains("1 aggregate entries"));
        budget.charge_bytes(5).unwrap();
        let byte_error = budget.charge_bytes(1).unwrap_err().to_string();
        assert!(byte_error.contains("5 aggregate bytes"));
    }
}
